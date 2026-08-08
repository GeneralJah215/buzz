//! Durable SQLite storage for locally delivered channel messages.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use buzz_core::event::StoredEvent;
use buzz_core::filter::filters_match;
use chrono::{DateTime, Utc};
use nostr::{
    Alphabet, Event, EventBuilder, EventId, Filter, JsonUtil, Keys, Kind, PublicKey,
    SingleLetterTag, Tag, Timestamp,
};
use parking_lot::Mutex;
use rusqlite::{
    params, params_from_iter, types::Value as SqlValue, Connection, OptionalExtension, Transaction,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

const RECEIPT_KIND: u16 = 20_900;
const AUTHORIZATION_SNAPSHOT_KIND: u16 = 20_901;
const MAX_DIGEST_CONTENT_BYTES: usize = 200 * 1024;
const MAX_QUERY_ROWS: usize = 10_000;

/// Storage failures are fail-closed at the relay boundary.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// SQLite returned an error.
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// A persisted event or receipt could not be decoded.
    #[error("corrupt edge database: {0}")]
    Corrupt(String),
    /// An agent presented a different owner than the first verified owner.
    #[error("agent owner attestation conflicts with the cached owner")]
    OwnerConflict,
    /// A query exceeded the bounded local result budget.
    #[error("local query exceeded {MAX_QUERY_ROWS} stored events")]
    QueryTooLarge,
    /// Receipt signing failed.
    #[error("failed to sign local receipt: {0}")]
    ReceiptSigning(String),
    /// The database is already bound to a different community.
    #[error("edge database community binding mismatch")]
    CommunityMismatch,
    /// The canonical relay origin is malformed.
    #[error("invalid canonical relay origin: {0}")]
    InvalidCanonicalOrigin(String),
    /// The database directory could not be created.
    #[error("failed to prepare edge database directory: {0}")]
    DataDirectory(String),
    /// An authorization snapshot could not be signed or verified.
    #[error("invalid authorization snapshot: {0}")]
    AuthorizationSnapshot(String),
    /// A relay-signed roster or removal signal was malformed or unauthentic.
    #[error("invalid authorization signal: {0}")]
    AuthorizationSignal(String),
    /// Digest materialization inputs are incomplete or inconsistent.
    #[error("invalid digest materialization: {0}")]
    InvalidDigest(String),
    /// The owner-selected authorization lease duration is invalid.
    #[error("invalid authorization lease duration: {0}")]
    InvalidAuthorizationLease(String),
}

/// Owner-selected bound for serving from a signed authorization snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthorizationPolicy {
    lease_seconds: i64,
}

impl AuthorizationPolicy {
    /// Create a non-zero lease duration that fits in signed Unix-second arithmetic.
    pub fn new(lease_duration: Duration) -> Result<Self, StorageError> {
        let lease_seconds = i64::try_from(lease_duration.as_secs())
            .map_err(|error| StorageError::InvalidAuthorizationLease(error.to_string()))?;
        if lease_seconds == 0 {
            return Err(StorageError::InvalidAuthorizationLease(
                "duration must be greater than zero".to_string(),
            ));
        }
        Ok(Self { lease_seconds })
    }

    /// Lease duration in whole seconds.
    pub fn lease_seconds(self) -> i64 {
        self.lease_seconds
    }
}

/// Result of loading the signed authorization lease at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthorizationLease {
    /// No signed snapshot has been persisted yet.
    Missing,
    /// The snapshot is valid and still inside its bounded offline lease.
    Valid {
        /// Complete signed per-channel active-author rosters and source cursors.
        channels: Vec<VerifiedChannelAuthorization>,
        /// Upstream verification time in Unix seconds.
        verified_at: i64,
        /// Lease cutoff in Unix seconds.
        expires_at: i64,
    },
    /// The signature is valid, but the owner-selected lease has elapsed.
    Expired {
        /// Lease cutoff in Unix seconds.
        expires_at: i64,
    },
}

/// One upstream-verified channel roster included in the signed authorization lease.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VerifiedChannelAuthorization {
    /// Selected channel covered by this authorization.
    pub channel_id: Uuid,
    /// Relay-signed kind-39002 source event ID.
    pub membership_event_id: EventId,
    /// Source-event cursor used to order replacement snapshots.
    pub membership_event_created_at: i64,
    /// Exact relay-signed kind-39002 source bytes.
    pub membership_event_bytes: Vec<u8>,
    /// Durable fetch cursor for the source projection. An unchanged source event
    /// never advances this cursor or any other roster freshness field.
    #[serde(default)]
    pub membership_fetch_cursor: Option<UpstreamCursor>,
    /// Last processed relay-signed kind-40099 channel removal signal.
    #[serde(default)]
    pub signal_cursor: Option<UpstreamCursor>,
    /// Last processed relay-signed kind-44100/44101 signal addressed to the edge.
    #[serde(default)]
    pub edge_notification_cursor: Option<UpstreamCursor>,
    /// Complete active author set extracted from the source event.
    pub active_authors: Vec<PublicKey>,
    /// Authors removed by a verified signal or canonical membership rejection
    /// since the current roster source was published.
    #[serde(default)]
    pub removed_authors: Vec<PublicKey>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct AuthorizationSnapshotPayload {
    canonical_origin: String,
    community_id: Uuid,
    verified_at: i64,
    expires_at: i64,
    channels: Vec<VerifiedChannelAuthorization>,
}

/// Immutable canonical relay/community pair for one edge database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommunityBinding {
    canonical_origin: String,
    community_id: Uuid,
}

impl CommunityBinding {
    /// Normalize and validate one canonical WebSocket relay origin.
    pub fn new(canonical_origin: &str, community_id: Uuid) -> Result<Self, StorageError> {
        let mut origin = url::Url::parse(canonical_origin)
            .map_err(|error| StorageError::InvalidCanonicalOrigin(error.to_string()))?;
        if origin.scheme() != "ws" && origin.scheme() != "wss" {
            return Err(StorageError::InvalidCanonicalOrigin(
                "scheme must be ws or wss".to_string(),
            ));
        }
        if origin.host_str().is_none()
            || !origin.username().is_empty()
            || origin.password().is_some()
            || (origin.path() != "" && origin.path() != "/")
            || origin.query().is_some()
            || origin.fragment().is_some()
        {
            return Err(StorageError::InvalidCanonicalOrigin(
                "origin must contain only scheme, host, and optional port".to_string(),
            ));
        }
        origin.set_path("");
        Ok(Self {
            canonical_origin: origin.to_string().trim_end_matches('/').to_string(),
            community_id,
        })
    }

    /// Normalized canonical WebSocket origin.
    pub fn canonical_origin(&self) -> &str {
        &self.canonical_origin
    }

    /// Community UUID bound to this database.
    pub fn community_id(&self) -> Uuid {
        self.community_id
    }

    /// Derive a stable, non-secret database filename from the bound pair.
    pub fn database_filename(&self) -> String {
        let digest =
            Sha256::digest(format!("{}\n{}", self.canonical_origin, self.community_id).as_bytes());
        format!("buzz-edge-{}.sqlite3", hex::encode(&digest[..16]))
    }
}

/// Result of atomically persisting a signed local event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertOutcome {
    /// The event, receipt, and pending outbox row were committed together.
    Inserted,
    /// The event ID was already present. No second fan-out should occur.
    Duplicate,
}

/// One outbox row leased to an author for upstream submission.
#[derive(Debug, Clone)]
pub struct ClaimedOutboxRow {
    /// Identifies the row when acknowledging the result.
    pub event_id: EventId,
    /// The exact stored bytes. Re-submitting these unchanged is what makes
    /// upstream's event-ID dedup produce exactly-once canonical storage.
    pub event: Event,
    /// Unix seconds after which this claim lapses back to `pending`.
    pub lease_expires_at: i64,
}

/// The author's report of what upstream did with one claimed event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DrainOutcome {
    /// Upstream accepted it.
    Delivered,
    /// Upstream already had it. **This is success** — event-ID dedup means the
    /// event is in canonical history, which is the whole point of re-submitting
    /// identical bytes after an ambiguous result.
    Duplicate,
    /// Upstream refused it permanently (bad signature, revoked membership,
    /// oversized). Never retried; surfaced for the operator.
    Rejected(String),
    /// Timeout, disconnect, or silence. Records nothing — the lease lapses and
    /// the row returns to `pending`. Writing a state here could contradict an
    /// upstream acceptance the author has not observed yet.
    Transient,
}

/// Outbox counts for the Desktop delivery surfaces.
///
/// `delivered_exact` and `delivered_via_digest` are kept apart on purpose:
/// the spec requires **delivered locally** and **synced to canonical history**
/// to be labelled separately everywhere they surface (§13).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboxSummary {
    /// Waiting for an author to drain them.
    pub pending: u64,
    /// Leased to an author right now.
    pub claimed: u64,
    /// In canonical history under their original event IDs.
    pub delivered_exact: u64,
    /// Represented in canonical history by an edge-authored digest instead.
    pub delivered_via_digest: u64,
    /// Permanently refused upstream. Needs a human.
    pub quarantined: u64,
}

/// One byte-stable, pre-signed digest part loaded for canonical submission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterializedDigestPart {
    /// One-based deterministic part number.
    pub part_index: u32,
    /// Total part count persisted with every part.
    pub total_parts: u32,
    /// Signed event ID used for canonical deduplication.
    pub event_id: EventId,
    /// Exact signed bytes that must be reused on every retry.
    pub event_bytes: Vec<u8>,
}

/// Durable high-water mark for one mirrored upstream channel.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpstreamCursor {
    /// Highest mirrored event timestamp in Unix seconds.
    pub created_at: i64,
    /// Highest event ID seen at that timestamp.
    pub event_id: EventId,
}

/// Durable effect of one relay-signed authorization signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorizationSignalOutcome {
    /// The signal was valid but older than or equal to the durable cursor.
    Duplicate,
    /// A relay-signed non-removal system event advanced the durable signal cursor.
    SignalObserved { channel_id: Uuid },
    /// A changed kind-39002 replaced the roster projection atomically.
    RosterReplaced { channel_id: Uuid },
    /// A kind-40099 or canonical rejection removed one local author.
    AuthorRemoved { channel_id: Uuid, author: PublicKey },
    /// An edge-self kind-44101 revoked local routing for the channel.
    EdgeRemoved { channel_id: Uuid },
    /// An edge-self kind-44100 requires a fresh authoritative eligibility read.
    RefreshRequired { channel_id: Uuid },
}

/// SQLite-backed event cache, membership cache, and phase-1 outbox.
pub struct EdgeStore {
    connection: Mutex<Connection>,
    binding: CommunityBinding,
    authorization_policy: AuthorizationPolicy,
}

impl EdgeStore {
    /// Open or create an edge database and apply its idempotent schema.
    pub fn open(
        path: impl AsRef<Path>,
        binding: CommunityBinding,
        authorization_policy: AuthorizationPolicy,
    ) -> Result<Self, StorageError> {
        let connection = Connection::open(path)?;
        Self::from_connection(connection, binding, authorization_policy)
    }

    /// Open the database filename derived from the canonical/community pair.
    pub fn open_bound(
        data_directory: impl AsRef<Path>,
        binding: CommunityBinding,
        authorization_policy: AuthorizationPolicy,
    ) -> Result<(Self, PathBuf), StorageError> {
        std::fs::create_dir_all(data_directory.as_ref())
            .map_err(|error| StorageError::DataDirectory(error.to_string()))?;
        let path = data_directory.as_ref().join(binding.database_filename());
        let store = Self::open(&path, binding, authorization_policy)?;
        Ok((store, path))
    }

    /// Open an in-memory edge database. Intended for tests and probes.
    pub fn open_in_memory(
        binding: CommunityBinding,
        authorization_policy: AuthorizationPolicy,
    ) -> Result<Self, StorageError> {
        let connection = Connection::open_in_memory()?;
        Self::from_connection(connection, binding, authorization_policy)
    }

    fn from_connection(
        connection: Connection,
        binding: CommunityBinding,
        authorization_policy: AuthorizationPolicy,
    ) -> Result<Self, StorageError> {
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.execute_batch(
            "PRAGMA foreign_keys = ON;
             PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS edge_schema (
                 version INTEGER PRIMARY KEY CHECK (version = 1)
             );
             INSERT OR IGNORE INTO edge_schema(version) VALUES (1);

             CREATE TABLE IF NOT EXISTS community_binding (
                 singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                 canonical_origin TEXT NOT NULL,
                 community_id TEXT NOT NULL
             );

             CREATE TABLE IF NOT EXISTS channel_members (
                 channel_id TEXT NOT NULL,
                 pubkey TEXT NOT NULL,
                 active INTEGER NOT NULL CHECK (active IN (0, 1)),
                 updated_at INTEGER NOT NULL,
                 PRIMARY KEY(channel_id, pubkey)
             );

             CREATE TABLE IF NOT EXISTS selected_channels (
                 channel_id TEXT PRIMARY KEY,
                 active INTEGER NOT NULL CHECK (active IN (0, 1)),
                 edge_eligible INTEGER NOT NULL DEFAULT 0 CHECK (edge_eligible IN (0, 1)),
                 eligibility_checked_at INTEGER,
                 updated_at INTEGER NOT NULL
             );

             CREATE TABLE IF NOT EXISTS nip_oa_owners (
                 agent_pubkey TEXT PRIMARY KEY,
                 owner_pubkey TEXT NOT NULL,
                 updated_at INTEGER NOT NULL
             );

             CREATE TABLE IF NOT EXISTS events (
                 event_id TEXT PRIMARY KEY,
                 author_pubkey TEXT NOT NULL,
                 channel_id TEXT NOT NULL,
                 created_at INTEGER NOT NULL,
                 received_at INTEGER NOT NULL,
                 event_json BLOB NOT NULL,
                 source TEXT NOT NULL CHECK (source IN ('local', 'upstream')),
                 receipt_id TEXT UNIQUE,
                 receipt_json BLOB,
                 CHECK (
                     (source = 'local' AND receipt_id IS NOT NULL AND receipt_json IS NOT NULL)
                     OR (source = 'upstream' AND receipt_id IS NULL AND receipt_json IS NULL)
                 )
             );

             CREATE INDEX IF NOT EXISTS events_channel_created_idx
                 ON events(channel_id, created_at DESC, event_id);

             CREATE TABLE IF NOT EXISTS event_dependencies (
                 child_event_id TEXT NOT NULL REFERENCES events(event_id) ON DELETE CASCADE,
                 ancestor_event_id TEXT NOT NULL,
                 relation TEXT NOT NULL CHECK (relation IN ('root', 'reply', 'legacy')),
                 ordinal INTEGER NOT NULL,
                 PRIMARY KEY(child_event_id, ancestor_event_id)
             );

             CREATE INDEX IF NOT EXISTS event_dependencies_ancestor_idx
                 ON event_dependencies(ancestor_event_id, child_event_id);

             CREATE TABLE IF NOT EXISTS outbox (
                 event_id TEXT PRIMARY KEY REFERENCES events(event_id) ON DELETE CASCADE,
                 state TEXT NOT NULL DEFAULT 'pending'
                     CHECK (state IN ('pending', 'claimed', 'delivered', 'quarantined')),
                 delivery_path TEXT NOT NULL DEFAULT 'exact'
                     CHECK (delivery_path IN ('exact', 'digest')),
                 demotion_reason TEXT,
                 claim_token TEXT,
                 lease_owner_pubkey TEXT,
                 lease_expires_at INTEGER,
                 attempts INTEGER NOT NULL DEFAULT 0,
                 last_error TEXT,
                 updated_at INTEGER NOT NULL
             );

             CREATE INDEX IF NOT EXISTS outbox_drain_idx
                 ON outbox(state, lease_expires_at, updated_at, event_id);

             CREATE TABLE IF NOT EXISTS authorization_snapshot (
                 singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                 edge_pubkey TEXT NOT NULL,
                 verified_at INTEGER NOT NULL,
                 expires_at INTEGER NOT NULL,
                 snapshot_event_id TEXT NOT NULL UNIQUE,
                 snapshot_event_bytes BLOB NOT NULL
             );

             CREATE TABLE IF NOT EXISTS digest_batches (
                 batch_id TEXT PRIMARY KEY,
                 channel_id TEXT NOT NULL,
                 state TEXT NOT NULL DEFAULT 'materialized'
                     CHECK (state IN ('materialized', 'delivered', 'quarantined')),
                 total_parts INTEGER NOT NULL CHECK (total_parts > 0),
                 created_at INTEGER NOT NULL,
                 updated_at INTEGER NOT NULL
             );

             CREATE TABLE IF NOT EXISTS digest_parts (
                 batch_id TEXT NOT NULL REFERENCES digest_batches(batch_id) ON DELETE CASCADE,
                 part_index INTEGER NOT NULL CHECK (part_index > 0),
                 total_parts INTEGER NOT NULL CHECK (total_parts > 0),
                 digest_event_id TEXT NOT NULL UNIQUE,
                 digest_event_bytes BLOB NOT NULL,
                 state TEXT NOT NULL DEFAULT 'pending'
                     CHECK (state IN ('pending', 'delivered', 'quarantined')),
                 attempts INTEGER NOT NULL DEFAULT 0,
                 last_error TEXT,
                 updated_at INTEGER NOT NULL,
                 PRIMARY KEY(batch_id, part_index),
                 CHECK (part_index <= total_parts)
             );

             CREATE TABLE IF NOT EXISTS digest_sources (
                 batch_id TEXT NOT NULL REFERENCES digest_batches(batch_id) ON DELETE CASCADE,
                 event_id TEXT NOT NULL REFERENCES events(event_id) ON DELETE RESTRICT,
                 ordinal INTEGER NOT NULL,
                 PRIMARY KEY(batch_id, event_id),
                 UNIQUE(event_id),
                 UNIQUE(batch_id, ordinal)
             );

             CREATE TABLE IF NOT EXISTS upstream_cursors (
                 channel_id TEXT PRIMARY KEY,
                 last_created_at INTEGER NOT NULL,
                 last_event_id TEXT NOT NULL,
                 updated_at INTEGER NOT NULL
             );

             CREATE TABLE IF NOT EXISTS nip98_replay (
                 event_id TEXT PRIMARY KEY,
                 expires_at INTEGER NOT NULL
             );",
        )?;
        connection.execute(
            "INSERT OR IGNORE INTO community_binding(
                 singleton, canonical_origin, community_id
             ) VALUES (1, ?1, ?2)",
            params![
                binding.canonical_origin(),
                binding.community_id().to_string()
            ],
        )?;
        let persisted: (String, String) = connection.query_row(
            "SELECT canonical_origin, community_id
             FROM community_binding WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if persisted.0 != binding.canonical_origin()
            || persisted.1 != binding.community_id().to_string()
        {
            return Err(StorageError::CommunityMismatch);
        }
        Ok(Self {
            connection: Mutex::new(connection),
            binding,
            authorization_policy,
        })
    }

    /// Return the immutable community binding stored in the database header.
    pub fn binding(&self) -> &CommunityBinding {
        &self.binding
    }

    /// Select or deselect a channel for this single-community edge instance.
    pub fn set_channel_selected(&self, channel_id: Uuid, active: bool) -> Result<(), StorageError> {
        self.connection.lock().execute(
            "INSERT INTO selected_channels(channel_id, active, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(channel_id) DO UPDATE SET
                 active = excluded.active,
                 edge_eligible = CASE WHEN excluded.active = 0 THEN 0 ELSE edge_eligible END,
                 updated_at = excluded.updated_at",
            params![channel_id.to_string(), i64::from(active), unix_seconds()],
        )?;
        Ok(())
    }

    /// Replace the complete selected-channel set from current startup configuration.
    pub fn replace_selected_channels(
        &self,
        selected_channels: &[Uuid],
    ) -> Result<(), StorageError> {
        let mut connection = self.connection.lock();
        let transaction = connection.transaction()?;
        transaction.execute(
            "UPDATE selected_channels
             SET active = 0, edge_eligible = 0, updated_at = ?1",
            [unix_seconds()],
        )?;
        for channel_id in selected_channels {
            transaction.execute(
                "INSERT INTO selected_channels(channel_id, active, edge_eligible, updated_at)
                 VALUES (?1, 1, 0, ?2)
                 ON CONFLICT(channel_id) DO UPDATE SET
                     active = 1,
                     edge_eligible = 0,
                     updated_at = excluded.updated_at",
                params![channel_id.to_string(), unix_seconds()],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Return all channels currently selected for this edge instance.
    pub fn selected_channels(&self) -> Result<Vec<Uuid>, StorageError> {
        let connection = self.connection.lock();
        let mut statement = connection.prepare(
            "SELECT channel_id FROM selected_channels WHERE active = 1 ORDER BY channel_id",
        )?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        rows.map(|row| {
            let value = row?;
            Uuid::parse_str(&value).map_err(|error| StorageError::Corrupt(error.to_string()))
        })
        .collect()
    }

    /// Return selected channels currently covered by an unexpired authorization snapshot.
    pub fn eligible_channels(&self) -> Result<Vec<Uuid>, StorageError> {
        let connection = self.connection.lock();
        let now = unix_seconds();
        let mut statement = connection.prepare(
            "SELECT s.channel_id
             FROM selected_channels s
             JOIN authorization_snapshot a ON a.singleton = 1
             WHERE s.active = 1 AND s.edge_eligible = 1
               AND ?1 >= a.verified_at AND ?1 <= a.expires_at
             ORDER BY s.channel_id",
        )?;
        let rows = statement.query_map([now], |row| row.get::<_, String>(0))?;
        rows.map(|row| {
            let value = row?;
            Uuid::parse_str(&value).map_err(|error| StorageError::Corrupt(error.to_string()))
        })
        .collect()
    }

    /// Return whether one channel is selected and covered by the current lease.
    pub fn channel_is_edge_eligible(&self, channel_id: Uuid) -> Result<bool, StorageError> {
        let eligible: Option<i64> = self
            .connection
            .lock()
            .query_row(
                "SELECT 1
                 FROM selected_channels s
                 JOIN authorization_snapshot a ON a.singleton = 1
                 WHERE s.channel_id = ?1 AND s.active = 1 AND s.edge_eligible = 1
                   AND ?2 >= a.verified_at AND ?2 <= a.expires_at",
                params![channel_id.to_string(), unix_seconds()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(eligible == Some(1))
    }

    /// Disable all local edge routing after a definitive denial or expired lease.
    pub fn clear_edge_eligibility(&self) -> Result<(), StorageError> {
        self.connection
            .lock()
            .execute("UPDATE selected_channels SET edge_eligible = 0", [])?;
        Ok(())
    }

    /// Permanently invalidate the offline lease after a definitive upstream denial.
    pub fn revoke_authorization_snapshot(&self) -> Result<(), StorageError> {
        let mut connection = self.connection.lock();
        let transaction = connection.transaction()?;
        transaction.execute("UPDATE selected_channels SET edge_eligible = 0", [])?;
        transaction.execute("DELETE FROM authorization_snapshot", [])?;
        transaction.execute("DELETE FROM channel_members", [])?;
        transaction.commit()?;
        Ok(())
    }

    /// Restore the eligible set from a verified signed offline snapshot.
    pub fn restore_edge_eligibility(
        &self,
        channels: &[VerifiedChannelAuthorization],
    ) -> Result<(), StorageError> {
        let mut connection = self.connection.lock();
        let transaction = connection.transaction()?;
        transaction.execute("UPDATE selected_channels SET edge_eligible = 0", [])?;
        for channel in channels {
            transaction.execute(
                "UPDATE selected_channels
                 SET edge_eligible = CASE WHEN active = 1 THEN 1 ELSE 0 END
                 WHERE channel_id = ?1",
                [channel.channel_id.to_string()],
            )?;
            replace_channel_members_transaction(&transaction, channel)?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Persist a freshly upstream-verified, edge-signed authorization snapshot.
    ///
    /// The upstream verifier supplies the exact eligible channel set. This method
    /// signs and commits the snapshot and updates routing eligibility atomically.
    pub fn persist_verified_authorization_snapshot(
        &self,
        channels: &[VerifiedChannelAuthorization],
        verified_at: i64,
        edge_keys: &Keys,
    ) -> Result<Event, StorageError> {
        let expires_at = verified_at
            .checked_add(self.authorization_policy.lease_seconds())
            .ok_or_else(|| {
                StorageError::AuthorizationSnapshot("lease timestamp overflow".to_string())
            })?;
        let mut connection = self.connection.lock();
        let transaction = connection.transaction()?;
        let existing = load_signed_snapshot_payload(&transaction, &edge_keys.public_key())?;
        let existing_by_channel = existing
            .map(|payload| {
                payload
                    .channels
                    .into_iter()
                    .map(|channel| (channel.channel_id, channel))
                    .collect::<HashMap<_, _>>()
            })
            .unwrap_or_default();
        let mut merged = Vec::with_capacity(channels.len());
        for channel in channels {
            let mut candidate = channel.clone();
            candidate.active_authors.sort_by_key(PublicKey::to_hex);
            candidate.active_authors.dedup();
            candidate.removed_authors.sort_by_key(PublicKey::to_hex);
            candidate.removed_authors.dedup();
            if candidate.membership_fetch_cursor.is_none() {
                candidate.membership_fetch_cursor = Some(UpstreamCursor {
                    created_at: candidate.membership_event_created_at,
                    event_id: candidate.membership_event_id,
                });
            }
            validate_channel_authorization(&candidate)?;
            if let Some(previous) = existing_by_channel.get(&candidate.channel_id) {
                if !membership_projection_is_newer(&candidate, previous) {
                    candidate = previous.clone();
                } else {
                    candidate.signal_cursor = previous.signal_cursor;
                    candidate.edge_notification_cursor = previous.edge_notification_cursor;
                    candidate.removed_authors.clear();
                }
            }
            merged.push(candidate);
        }
        merged.sort_by_key(|channel| channel.channel_id);
        merged.dedup_by_key(|channel| channel.channel_id);
        let payload = AuthorizationSnapshotPayload {
            canonical_origin: self.binding.canonical_origin().to_string(),
            community_id: self.binding.community_id(),
            verified_at,
            expires_at,
            channels: merged,
        };
        let snapshot = persist_snapshot_transaction(
            &transaction,
            &payload,
            edge_keys,
            self.binding.community_id(),
        )?;
        transaction.commit()?;
        Ok(snapshot)
    }

    /// Verify and load the persisted authorization snapshot for offline startup.
    pub fn load_authorization_lease(
        &self,
        edge_pubkey: &PublicKey,
        now: i64,
    ) -> Result<AuthorizationLease, StorageError> {
        let row: Option<(String, i64, i64, String, Vec<u8>)> = self
            .connection
            .lock()
            .query_row(
                "SELECT edge_pubkey, verified_at, expires_at,
                        snapshot_event_id, snapshot_event_bytes
                 FROM authorization_snapshot WHERE singleton = 1",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()?;
        let Some((stored_pubkey, verified_at, expires_at, stored_event_id, bytes)) = row else {
            return Ok(AuthorizationLease::Missing);
        };
        if stored_pubkey != edge_pubkey.to_hex() {
            return Err(StorageError::AuthorizationSnapshot(
                "snapshot signer does not match the configured edge identity".to_string(),
            ));
        }
        let event: Event = serde_json::from_slice(&bytes)
            .map_err(|error| StorageError::AuthorizationSnapshot(error.to_string()))?;
        if event.id.to_hex() != stored_event_id
            || event.pubkey != *edge_pubkey
            || event.kind != Kind::Custom(AUTHORIZATION_SNAPSHOT_KIND)
            || buzz_core::verification::verify_event(&event).is_err()
        {
            return Err(StorageError::AuthorizationSnapshot(
                "snapshot signature verification failed".to_string(),
            ));
        }
        let payload: AuthorizationSnapshotPayload = serde_json::from_str(&event.content)
            .map_err(|error| StorageError::AuthorizationSnapshot(error.to_string()))?;
        if payload.canonical_origin != self.binding.canonical_origin()
            || payload.community_id != self.binding.community_id()
            || payload.verified_at != verified_at
            || payload.expires_at != expires_at
            || expires_at.checked_sub(verified_at)
                != Some(self.authorization_policy.lease_seconds())
        {
            return Err(StorageError::AuthorizationSnapshot(
                "snapshot content does not match its database header".to_string(),
            ));
        }
        for channel in &payload.channels {
            validate_channel_authorization(channel)?;
        }
        if now < verified_at {
            return Err(StorageError::AuthorizationSnapshot(
                "snapshot verification time is in the future".to_string(),
            ));
        }
        if now > expires_at {
            Ok(AuthorizationLease::Expired { expires_at })
        } else {
            Ok(AuthorizationLease::Valid {
                channels: payload.channels,
                verified_at,
                expires_at,
            })
        }
    }

    /// Return the relay signing identity pinned by the current roster sources.
    pub fn relay_identity(&self, edge_pubkey: &PublicKey) -> Result<PublicKey, StorageError> {
        let connection = self.connection.lock();
        let payload = load_signed_snapshot_payload(&connection, edge_pubkey)?.ok_or_else(|| {
            StorageError::AuthorizationSnapshot("authorization snapshot is missing".to_string())
        })?;
        snapshot_relay_identity(&payload)
    }

    /// Return the durable authorization-signal cursor for one channel.
    pub fn authorization_signal_cursor(
        &self,
        edge_pubkey: &PublicKey,
        channel_id: Uuid,
    ) -> Result<Option<UpstreamCursor>, StorageError> {
        let connection = self.connection.lock();
        let payload = load_signed_snapshot_payload(&connection, edge_pubkey)?.ok_or_else(|| {
            StorageError::AuthorizationSnapshot("authorization snapshot is missing".to_string())
        })?;
        Ok(payload
            .channels
            .into_iter()
            .find(|channel| channel.channel_id == channel_id)
            .and_then(|channel| channel.signal_cursor))
    }

    /// Return the relay-signed roster cursor that bounds historical signals.
    pub fn authorization_roster_cursor(
        &self,
        edge_pubkey: &PublicKey,
        channel_id: Uuid,
    ) -> Result<UpstreamCursor, StorageError> {
        let connection = self.connection.lock();
        let payload = load_signed_snapshot_payload(&connection, edge_pubkey)?.ok_or_else(|| {
            StorageError::AuthorizationSnapshot("authorization snapshot is missing".to_string())
        })?;
        let channel = payload
            .channels
            .into_iter()
            .find(|channel| channel.channel_id == channel_id)
            .ok_or_else(|| {
                StorageError::AuthorizationSnapshot(
                    "authorization snapshot does not contain the channel".to_string(),
                )
            })?;
        Ok(channel.membership_fetch_cursor.unwrap_or(UpstreamCursor {
            created_at: channel.membership_event_created_at,
            event_id: channel.membership_event_id,
        }))
    }

    /// Return the durable edge-self membership-notification cursor for one channel.
    pub fn edge_notification_cursor(
        &self,
        edge_pubkey: &PublicKey,
        channel_id: Uuid,
    ) -> Result<Option<UpstreamCursor>, StorageError> {
        let connection = self.connection.lock();
        let payload = load_signed_snapshot_payload(&connection, edge_pubkey)?.ok_or_else(|| {
            StorageError::AuthorizationSnapshot("authorization snapshot is missing".to_string())
        })?;
        Ok(payload
            .channels
            .into_iter()
            .find(|channel| channel.channel_id == channel_id)
            .and_then(|channel| channel.edge_notification_cursor))
    }

    /// Validate and atomically apply one relay-signed roster/removal signal.
    ///
    /// kind-40099 may remove other authors. kind-44100/44101 is accepted only
    /// when addressed to the edge identity itself and never mutates another
    /// author's working-roster entry.
    pub fn apply_authorization_signal(
        &self,
        event: &Event,
        edge_keys: &Keys,
    ) -> Result<AuthorizationSignalOutcome, StorageError> {
        if buzz_core::verification::verify_event(event).is_err() {
            return Err(StorageError::AuthorizationSignal(
                "signal signature verification failed".to_string(),
            ));
        }
        let cursor = event_cursor(event)?;
        let mut connection = self.connection.lock();
        let transaction = connection.transaction()?;
        let mut payload = load_signed_snapshot_payload(&transaction, &edge_keys.public_key())?
            .ok_or_else(|| {
                StorageError::AuthorizationSnapshot("authorization snapshot is missing".to_string())
            })?;
        if event.pubkey != snapshot_relay_identity(&payload)? {
            return Err(StorageError::AuthorizationSignal(
                "signal signer does not match the roster-source relay".to_string(),
            ));
        }

        let outcome = match event.kind.as_u16() {
            39_002 => {
                apply_roster_projection(&mut payload, event, cursor, &edge_keys.public_key())?
            }
            40_099 => apply_system_removal(&mut payload, event, cursor)?,
            44_100 | 44_101 => apply_edge_membership_notification(
                &mut payload,
                event,
                cursor,
                &edge_keys.public_key(),
            )?,
            kind => {
                return Err(StorageError::AuthorizationSignal(format!(
                    "unsupported signal kind {kind}"
                )))
            }
        };
        if outcome != AuthorizationSignalOutcome::Duplicate {
            persist_snapshot_transaction(
                &transaction,
                &payload,
                edge_keys,
                self.binding.community_id(),
            )?;
        }
        transaction.commit()?;
        Ok(outcome)
    }

    /// Apply an authoritative canonical membership rejection to the working roster.
    pub fn revoke_author_after_canonical_rejection(
        &self,
        channel_id: Uuid,
        author: PublicKey,
        edge_keys: &Keys,
    ) -> Result<AuthorizationSignalOutcome, StorageError> {
        let mut connection = self.connection.lock();
        let transaction = connection.transaction()?;
        let mut payload = load_signed_snapshot_payload(&transaction, &edge_keys.public_key())?
            .ok_or_else(|| {
                StorageError::AuthorizationSnapshot("authorization snapshot is missing".to_string())
            })?;
        let channel = payload
            .channels
            .iter_mut()
            .find(|channel| channel.channel_id == channel_id)
            .ok_or_else(|| {
                StorageError::AuthorizationSignal("rejected channel is not eligible".to_string())
            })?;
        let changed =
            channel.active_authors.contains(&author) && !channel.removed_authors.contains(&author);
        if changed {
            channel.removed_authors.push(author);
            channel.removed_authors.sort_by_key(PublicKey::to_hex);
        }
        if changed {
            persist_snapshot_transaction(
                &transaction,
                &payload,
                edge_keys,
                self.binding.community_id(),
            )?;
        }
        transaction.commit()?;
        Ok(AuthorizationSignalOutcome::AuthorRemoved { channel_id, author })
    }

    /// Upsert a cached channel membership decision in protocol tests.
    #[cfg(test)]
    pub(crate) fn set_channel_member(
        &self,
        channel_id: Uuid,
        pubkey: &PublicKey,
        active: bool,
    ) -> Result<(), StorageError> {
        self.connection.lock().execute(
            "INSERT INTO channel_members(channel_id, pubkey, active, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(channel_id, pubkey) DO UPDATE SET
                 active = excluded.active,
                 updated_at = excluded.updated_at",
            params![
                channel_id.to_string(),
                pubkey.to_hex(),
                i64::from(active),
                unix_seconds()
            ],
        )?;
        Ok(())
    }

    /// Atomically persist a digest source set and its exact pre-signed parts.
    ///
    /// This is intentionally materialization-only. M4 owns deterministic
    /// construction and acknowledgments; M2 guarantees that no retry needs to
    /// rebuild or re-sign bytes after this transaction commits.
    pub fn materialize_digest_batch(
        &self,
        batch_id: &str,
        channel_id: Uuid,
        source_event_ids: &[EventId],
        parts: &[Event],
        edge_pubkey: &PublicKey,
    ) -> Result<(), StorageError> {
        if batch_id.is_empty() || source_event_ids.is_empty() || parts.is_empty() {
            return Err(StorageError::InvalidDigest(
                "batch id, source set, and parts must be non-empty".to_string(),
            ));
        }
        let total_parts = i64::try_from(parts.len())
            .map_err(|error| StorageError::InvalidDigest(error.to_string()))?;
        for (index, event) in parts.iter().enumerate() {
            validate_digest_part(event, channel_id, edge_pubkey, index + 1, parts.len())?;
        }
        let mut connection = self.connection.lock();
        let transaction = connection.transaction()?;
        transaction.execute(
            "INSERT INTO digest_batches(
                 batch_id, channel_id, state, total_parts, created_at, updated_at
             ) VALUES (?1, ?2, 'materialized', ?3, ?4, ?4)",
            params![
                batch_id,
                channel_id.to_string(),
                total_parts,
                unix_seconds()
            ],
        )?;
        for (ordinal, event_id) in source_event_ids.iter().enumerate() {
            let source_valid: Option<i64> = transaction
                .query_row(
                    "SELECT 1 FROM events e
                     JOIN outbox o ON o.event_id = e.event_id
                     WHERE e.event_id = ?1 AND e.channel_id = ?2
                       AND o.state = 'pending' AND o.delivery_path = 'exact'",
                    params![event_id.to_hex(), channel_id.to_string()],
                    |row| row.get(0),
                )
                .optional()?;
            if source_valid.is_none() {
                return Err(StorageError::InvalidDigest(format!(
                    "source event {event_id} is missing, belongs to another channel, or is not pending exact delivery"
                )));
            }
            transaction.execute(
                "INSERT INTO digest_sources(batch_id, event_id, ordinal)
                 VALUES (?1, ?2, ?3)",
                params![
                    batch_id,
                    event_id.to_hex(),
                    i64::try_from(ordinal)
                        .map_err(|error| StorageError::InvalidDigest(error.to_string()))?
                ],
            )?;
            let changed = transaction.execute(
                "UPDATE outbox
                 SET delivery_path = 'digest', updated_at = ?2
                 WHERE event_id = ?1 AND state = 'pending' AND delivery_path = 'exact'",
                params![event_id.to_hex(), unix_seconds()],
            )?;
            if changed != 1 {
                return Err(StorageError::InvalidDigest(format!(
                    "source event {event_id} changed while the digest was materialized"
                )));
            }
        }
        for (index, event) in parts.iter().enumerate() {
            let part_index = i64::try_from(index + 1)
                .map_err(|error| StorageError::InvalidDigest(error.to_string()))?;
            transaction.execute(
                "INSERT INTO digest_parts(
                     batch_id, part_index, total_parts, digest_event_id,
                     digest_event_bytes, state, updated_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6)",
                params![
                    batch_id,
                    part_index,
                    total_parts,
                    event.id.to_hex(),
                    event.as_json().as_bytes(),
                    unix_seconds()
                ],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Load exact signed digest bytes in deterministic part order.
    pub fn digest_parts(
        &self,
        batch_id: &str,
    ) -> Result<Vec<MaterializedDigestPart>, StorageError> {
        let connection = self.connection.lock();
        let mut statement = connection.prepare(
            "SELECT part_index, total_parts, digest_event_id, digest_event_bytes
             FROM digest_parts WHERE batch_id = ?1 ORDER BY part_index",
        )?;
        let rows = statement.query_map([batch_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Vec<u8>>(3)?,
            ))
        })?;
        rows.map(|row| {
            let (part_index, total_parts, event_id, event_bytes) = row?;
            Ok(MaterializedDigestPart {
                part_index: u32::try_from(part_index)
                    .map_err(|error| StorageError::Corrupt(error.to_string()))?,
                total_parts: u32::try_from(total_parts)
                    .map_err(|error| StorageError::Corrupt(error.to_string()))?,
                event_id: EventId::from_hex(&event_id)
                    .map_err(|error| StorageError::Corrupt(error.to_string()))?,
                event_bytes,
            })
        })
        .collect()
    }

    /// Record a verified NIP-OA relationship using first-write-wins semantics.
    pub fn record_nip_oa_owner(
        &self,
        agent: &PublicKey,
        owner: &PublicKey,
    ) -> Result<(), StorageError> {
        let connection = self.connection.lock();
        connection.execute(
            "INSERT OR IGNORE INTO nip_oa_owners(agent_pubkey, owner_pubkey, updated_at)
             VALUES (?1, ?2, ?3)",
            params![agent.to_hex(), owner.to_hex(), unix_seconds()],
        )?;
        let cached: Option<String> = connection
            .query_row(
                "SELECT owner_pubkey FROM nip_oa_owners WHERE agent_pubkey = ?1",
                [agent.to_hex()],
                |row| row.get(0),
            )
            .optional()?;
        if cached.as_deref() == Some(owner.to_hex().as_str()) {
            Ok(())
        } else {
            Err(StorageError::OwnerConflict)
        }
    }

    /// Return whether the exact authenticated principal has an active signed-roster lease.
    pub fn principal_can_access(
        &self,
        channel_id: Uuid,
        principal: &PublicKey,
    ) -> Result<bool, StorageError> {
        self.principal_can_access_at(channel_id, principal, unix_seconds())
    }

    fn principal_can_access_at(
        &self,
        channel_id: Uuid,
        principal: &PublicKey,
        now: i64,
    ) -> Result<bool, StorageError> {
        let connection = self.connection.lock();
        let selected_and_leased: Option<i64> = connection
            .query_row(
                "SELECT 1
                 FROM selected_channels s
                 JOIN authorization_snapshot a ON a.singleton = 1
                 WHERE s.channel_id = ?1 AND s.active = 1 AND s.edge_eligible = 1
                   AND ?2 >= a.verified_at AND ?2 <= a.expires_at",
                params![channel_id.to_string(), now],
                |row| row.get(0),
            )
            .optional()?;
        if selected_and_leased != Some(1) {
            return Ok(false);
        }
        member_is_active(&connection, channel_id, principal)
    }

    /// Atomically reject replayed NIP-98 auth events within their 60-second lifetime.
    pub fn mark_nip98_auth(&self, event_id: &EventId) -> Result<bool, StorageError> {
        let now = unix_seconds();
        let mut connection = self.connection.lock();
        let transaction = connection.transaction()?;
        transaction.execute("DELETE FROM nip98_replay WHERE expires_at <= ?1", [now])?;
        let inserted = transaction.execute(
            "INSERT OR IGNORE INTO nip98_replay(event_id, expires_at) VALUES (?1, ?2)",
            params![event_id.to_hex(), now + 60],
        )?;
        transaction.commit()?;
        Ok(inserted == 1)
    }

    /// Atomically store a signed event, a device-signed receipt, and an outbox row.
    pub fn insert_local_event(
        &self,
        event: &Event,
        exact_event_bytes: &[u8],
        channel_id: Uuid,
        edge_keys: &Keys,
    ) -> Result<InsertOutcome, StorageError> {
        let decoded: Event = serde_json::from_slice(exact_event_bytes)
            .map_err(|error| StorageError::Corrupt(error.to_string()))?;
        if decoded != *event {
            return Err(StorageError::Corrupt(
                "event bytes do not match the verified event".to_string(),
            ));
        }
        let event_id = event.id.to_hex();
        let mut connection = self.connection.lock();
        let transaction = connection.transaction()?;
        if event_exists(&transaction, &event_id)? {
            return Ok(InsertOutcome::Duplicate);
        }

        let received_at = unix_seconds();
        let receipt = build_receipt(event, channel_id, received_at, edge_keys)?;
        transaction.execute(
            "INSERT INTO events(
                 event_id, author_pubkey, channel_id, created_at, received_at,
                 event_json, source, receipt_id, receipt_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'local', ?7, ?8)",
            params![
                event_id,
                event.pubkey.to_hex(),
                channel_id.to_string(),
                i64::try_from(event.created_at.as_secs())
                    .map_err(|error| StorageError::Corrupt(error.to_string()))?,
                received_at,
                exact_event_bytes,
                receipt.id.to_hex(),
                receipt.as_json(),
            ],
        )?;
        persist_event_dependencies(&transaction, event)?;
        transaction.execute(
            "INSERT INTO outbox(event_id, state, attempts, updated_at)
             VALUES (?1, 'pending', 0, ?2)",
            params![event.id.to_hex(), received_at],
        )?;
        transaction.commit()?;
        Ok(InsertOutcome::Inserted)
    }

    /// Atomically cache one canonical kind-9 event without advancing catch-up state.
    ///
    /// The mirror commits its channel cursor only after a complete HTTP bridge
    /// backfill page sequence. A crash or disconnect before then therefore
    /// replays identical bytes instead of permanently skipping unseen history.
    pub fn insert_upstream_event(
        &self,
        event: &Event,
        exact_event_bytes: &[u8],
        channel_id: Uuid,
    ) -> Result<InsertOutcome, StorageError> {
        let decoded: Event = serde_json::from_slice(exact_event_bytes)
            .map_err(|error| StorageError::Corrupt(error.to_string()))?;
        if decoded != *event || buzz_core::verification::verify_event(event).is_err() {
            return Err(StorageError::Corrupt(
                "upstream event bytes or signature are invalid".to_string(),
            ));
        }
        let created_at = i64::try_from(event.created_at.as_secs())
            .map_err(|error| StorageError::Corrupt(error.to_string()))?;
        let event_id = event.id.to_hex();
        let mut connection = self.connection.lock();
        let transaction = connection.transaction()?;
        let outcome = if event_exists(&transaction, &event_id)? {
            InsertOutcome::Duplicate
        } else {
            transaction.execute(
                "INSERT INTO events(
                     event_id, author_pubkey, channel_id, created_at, received_at,
                     event_json, source, receipt_id, receipt_json
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'upstream', NULL, NULL)",
                params![
                    event_id,
                    event.pubkey.to_hex(),
                    channel_id.to_string(),
                    created_at,
                    unix_seconds(),
                    exact_event_bytes,
                ],
            )?;
            persist_event_dependencies(&transaction, event)?;
            InsertOutcome::Inserted
        };
        transaction.commit()?;
        Ok(outcome)
    }

    /// Advance the durable high-water mark after a gap-free channel backfill.
    pub fn advance_upstream_cursor(
        &self,
        channel_id: Uuid,
        cursor: &UpstreamCursor,
    ) -> Result<(), StorageError> {
        self.connection.lock().execute(
            "INSERT INTO upstream_cursors(
                 channel_id, last_created_at, last_event_id, updated_at
             ) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(channel_id) DO UPDATE SET
                 last_created_at = excluded.last_created_at,
                 last_event_id = excluded.last_event_id,
                 updated_at = excluded.updated_at
             WHERE excluded.last_created_at > upstream_cursors.last_created_at
                OR (excluded.last_created_at = upstream_cursors.last_created_at
                    AND excluded.last_event_id > upstream_cursors.last_event_id)",
            params![
                channel_id.to_string(),
                cursor.created_at,
                cursor.event_id.to_hex(),
                unix_seconds()
            ],
        )?;
        Ok(())
    }

    /// Load the durable mirror cursor for one selected channel.
    pub fn upstream_cursor(
        &self,
        channel_id: Uuid,
    ) -> Result<Option<UpstreamCursor>, StorageError> {
        let row: Option<(i64, String)> = self
            .connection
            .lock()
            .query_row(
                "SELECT last_created_at, last_event_id
                 FROM upstream_cursors WHERE channel_id = ?1",
                [channel_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        row.map(|(created_at, event_id)| {
            Ok(UpstreamCursor {
                created_at,
                event_id: EventId::from_hex(&event_id)
                    .map_err(|error| StorageError::Corrupt(error.to_string()))?,
            })
        })
        .transpose()
    }

    /// Return locally known ancestor references in event-tag order.
    pub fn dependencies(&self, event_id: &EventId) -> Result<Vec<EventId>, StorageError> {
        let connection = self.connection.lock();
        let mut statement = connection.prepare(
            "SELECT ancestor_event_id FROM event_dependencies
             WHERE child_event_id = ?1 ORDER BY ordinal",
        )?;
        let rows = statement.query_map([event_id.to_hex()], |row| row.get::<_, String>(0))?;
        let mut dependencies = Vec::new();
        for row in rows {
            let raw = row?;
            let event_id = EventId::from_hex(&raw)
                .map_err(|error| StorageError::Corrupt(error.to_string()))?;
            dependencies.push(event_id);
        }
        Ok(dependencies)
    }

    /// Query locally stored events using the shared NIP-01 matcher.
    pub fn query(&self, filters: &[Filter]) -> Result<Vec<Event>, StorageError> {
        self.query_inner(filters, false)
    }

    fn query_inner(
        &self,
        filters: &[Filter],
        exact_count: bool,
    ) -> Result<Vec<Event>, StorageError> {
        let mut by_id = HashMap::<EventId, Event>::new();
        for filter in filters {
            let limit = if exact_count {
                MAX_QUERY_ROWS
            } else {
                filter.limit.unwrap_or(MAX_QUERY_ROWS).min(MAX_QUERY_ROWS)
            };
            let channel_only = filter_is_channel_only(filter);
            let scan_budget = if channel_only && !exact_count {
                limit
            } else {
                MAX_QUERY_ROWS
            };
            let (stored, truncated) = self.load_filter_events(filter, scan_budget)?;
            let matched = stored
                .iter()
                .filter(|item| filters_match(std::slice::from_ref(filter), item))
                .take(limit)
                .collect::<Vec<_>>();
            if truncated && (exact_count || matched.len() < limit) {
                return Err(StorageError::QueryTooLarge);
            }
            for item in matched {
                by_id
                    .entry(item.event.id)
                    .or_insert_with(|| item.event.clone());
            }
        }
        let mut events: Vec<Event> = by_id.into_values().collect();
        events.sort_by(|left, right| {
            right
                .created_at
                .cmp(&left.created_at)
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(events)
    }

    /// Count the union of events matching the supplied filters, ignoring filter limits.
    pub fn count(&self, filters: &[Filter]) -> Result<u64, StorageError> {
        let filters_without_limits: Vec<Filter> = filters
            .iter()
            .cloned()
            .map(|mut filter| {
                filter.limit = None;
                filter
            })
            .collect();
        Ok(self.query_inner(&filters_without_limits, true)?.len() as u64)
    }

    /// Return the number of durable events, for readiness and tests.
    pub fn event_count(&self) -> Result<u64, StorageError> {
        let count: i64 =
            self.connection
                .lock()
                .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))?;
        Ok(count as u64)
    }

    /// Return the number of pending outbox rows.
    pub fn pending_count(&self) -> Result<u64, StorageError> {
        let count: i64 = self.connection.lock().query_row(
            "SELECT COUNT(*) FROM outbox WHERE state = 'pending'",
            [],
            |row| row.get(0),
        )?;
        Ok(count as u64)
    }

    /// Load the device-signed receipt associated with an event.
    pub fn receipt(&self, event_id: &EventId) -> Result<Option<Event>, StorageError> {
        let raw: Option<String> = self
            .connection
            .lock()
            .query_row(
                "SELECT receipt_json FROM events
                 WHERE event_id = ?1 AND receipt_json IS NOT NULL",
                [event_id.to_hex()],
                |row| row.get(0),
            )
            .optional()?;
        raw.map(|json| {
            Event::from_json(json).map_err(|error| StorageError::Corrupt(error.to_string()))
        })
        .transpose()
    }

    // ── Author-drain state machine (§11) ────────────────────────────────────
    //
    // `pending → claimed(lease) → delivered | quarantined`, with
    // `claimed → pending` on lease expiry.
    //
    // There is deliberately no `submitted` state. A row stays `claimed` until
    // the author's per-event acknowledgment arrives, so a crash between
    // upstream accepting an event and the sidecar hearing about it cannot
    // strand the row: the lease expires, the row returns to `pending`, the
    // next drain re-submits the identical signed bytes, and upstream's
    // event-ID dedup answers duplicate — which the protocol records as
    // `delivered`.

    /// Return expired claims to `pending` and report how many were reclaimed.
    ///
    /// Called before every claim so a crashed or vanished author cannot hold
    /// rows hostage. Idempotent.
    pub fn expire_outbox_leases(&self, now: i64) -> Result<u64, StorageError> {
        let changed = self.connection.lock().execute(
            "UPDATE outbox
                SET state = 'pending',
                    claim_token = NULL,
                    lease_owner_pubkey = NULL,
                    lease_expires_at = NULL,
                    updated_at = ?1
              WHERE state = 'claimed' AND lease_expires_at IS NOT NULL
                AND lease_expires_at <= ?1",
            [now],
        )?;
        Ok(changed as u64)
    }

    /// Claim an ordered batch of this author's drainable rows under a lease.
    ///
    /// **Ownership is enforced here, not by the caller** (§11): only rows whose
    /// author equals `author` are claimable. The sidecar cannot submit another
    /// identity's events upstream — canonical ingest rejects that — so a claim
    /// that crossed identities could never be drained.
    ///
    /// **Ordering is globally dependency-gated, not merely per-author.** A row
    /// is claimable only when every locally known ancestor in its thread chain
    /// has already reached `delivered`, because canonical ingest rejects a
    /// reply whose parent is not stored, and threads routinely cross
    /// identities. Within the claimable set, ordering is FIFO by local arrival.
    ///
    /// Rows already demoted to the digest path are never claimed: they are not
    /// going upstream under their own IDs.
    pub fn claim_outbox_batch(
        &self,
        author: &PublicKey,
        claim_token: &str,
        limit: usize,
        now: i64,
        lease_seconds: i64,
    ) -> Result<Vec<ClaimedOutboxRow>, StorageError> {
        if claim_token.trim().is_empty() {
            return Err(StorageError::Corrupt(
                "claim token must not be empty".to_string(),
            ));
        }
        self.expire_outbox_leases(now)?;

        let mut connection = self.connection.lock();
        let transaction = connection.transaction()?;
        let author_hex = author.to_hex();
        let expires_at = now.saturating_add(lease_seconds);

        let candidates: Vec<(String, Vec<u8>)> = {
            let mut statement = transaction.prepare(
                "SELECT o.event_id, e.event_json
                   FROM outbox o
                   JOIN events e ON e.event_id = o.event_id
                  WHERE o.state = 'pending'
                    AND o.delivery_path = 'exact'
                    AND e.author_pubkey = ?1
                    AND NOT EXISTS (
                        SELECT 1
                          FROM event_dependencies d
                          JOIN outbox po ON po.event_id = d.ancestor_event_id
                         WHERE d.child_event_id = o.event_id
                           AND po.state <> 'delivered'
                    )
                  ORDER BY e.received_at, o.event_id
                  LIMIT ?2",
            )?;
            let rows = statement.query_map(params![author_hex, limit as i64], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };

        let mut claimed = Vec::with_capacity(candidates.len());
        for (event_id_hex, event_json) in candidates {
            transaction.execute(
                "UPDATE outbox
                    SET state = 'claimed',
                        claim_token = ?2,
                        lease_owner_pubkey = ?3,
                        lease_expires_at = ?4,
                        attempts = attempts + 1,
                        updated_at = ?5
                  WHERE event_id = ?1 AND state = 'pending'",
                params![event_id_hex, claim_token, author_hex, expires_at, now],
            )?;
            let event = Event::from_json(event_json)
                .map_err(|error| StorageError::Corrupt(error.to_string()))?;
            claimed.push(ClaimedOutboxRow {
                event_id: EventId::from_hex(&event_id_hex)
                    .map_err(|error| StorageError::Corrupt(error.to_string()))?,
                event,
                lease_expires_at: expires_at,
            });
        }
        transaction.commit()?;
        Ok(claimed)
    }

    /// Extend a live lease so a slow but healthy drain is not preempted.
    ///
    /// Only the holder of `claim_token` can renew, so a stale author that lost
    /// its rows to expiry cannot silently take them back mid-flight.
    pub fn renew_outbox_lease(
        &self,
        claim_token: &str,
        now: i64,
        lease_seconds: i64,
    ) -> Result<u64, StorageError> {
        let changed = self.connection.lock().execute(
            "UPDATE outbox
                SET lease_expires_at = ?2, updated_at = ?3
              WHERE state = 'claimed' AND claim_token = ?1",
            params![claim_token, now.saturating_add(lease_seconds), now],
        )?;
        Ok(changed as u64)
    }

    /// Record the author's per-event result for a claimed row.
    ///
    /// `Duplicate` is success, not an error: upstream event-ID dedup is what
    /// makes canonical storage exactly-once across retries, so a duplicate
    /// proves the event is already in canonical history.
    ///
    /// A transient failure is deliberately NOT recorded as a state change —
    /// the row stays `claimed` and returns to `pending` when the lease expires.
    /// Writing a state here would risk contradicting an upstream acceptance
    /// the author has not yet observed.
    pub fn acknowledge_outbox_row(
        &self,
        claim_token: &str,
        event_id: &EventId,
        outcome: DrainOutcome,
    ) -> Result<bool, StorageError> {
        let now = unix_seconds();
        let connection = self.connection.lock();
        let changed = match outcome {
            DrainOutcome::Delivered | DrainOutcome::Duplicate => connection.execute(
                "UPDATE outbox
                    SET state = 'delivered',
                        claim_token = NULL,
                        lease_owner_pubkey = NULL,
                        lease_expires_at = NULL,
                        last_error = NULL,
                        updated_at = ?3
                  WHERE event_id = ?1 AND state = 'claimed' AND claim_token = ?2",
                params![event_id.to_hex(), claim_token, now],
            )?,
            DrainOutcome::Rejected(ref reason) => connection.execute(
                "UPDATE outbox
                    SET state = 'quarantined',
                        claim_token = NULL,
                        lease_owner_pubkey = NULL,
                        lease_expires_at = NULL,
                        last_error = ?3,
                        updated_at = ?4
                  WHERE event_id = ?1 AND state = 'claimed' AND claim_token = ?2",
                params![event_id.to_hex(), claim_token, reason.as_str(), now],
            )?,
            // Transient: leave it claimed and let the lease lapse.
            DrainOutcome::Transient => 0,
        };
        Ok(changed > 0)
    }

    /// Counts by state, for the Desktop delivery surfaces (§13).
    pub fn outbox_summary(&self) -> Result<OutboxSummary, StorageError> {
        let connection = self.connection.lock();
        let mut summary = OutboxSummary::default();
        let mut statement = connection
            .prepare("SELECT state, delivery_path, COUNT(*) FROM outbox GROUP BY 1, 2")?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)? as u64,
            ))
        })?;
        for row in rows {
            let (state, path, count) = row?;
            match (state.as_str(), path.as_str()) {
                ("pending", _) => summary.pending += count,
                ("claimed", _) => summary.claimed += count,
                ("quarantined", _) => summary.quarantined += count,
                ("delivered", "digest") => summary.delivered_via_digest += count,
                ("delivered", _) => summary.delivered_exact += count,
                _ => {}
            }
        }
        Ok(summary)
    }

    fn load_filter_events(
        &self,
        filter: &Filter,
        scan_budget: usize,
    ) -> Result<(Vec<StoredEvent>, bool), StorageError> {
        let h = SingleLetterTag::lowercase(Alphabet::H);
        let channels = filter
            .generic_tags
            .get(&h)
            .ok_or_else(|| StorageError::Corrupt("query filter has no channel".to_string()))?;
        if channels.is_empty() {
            return Err(StorageError::Corrupt(
                "query filter has an empty channel set".to_string(),
            ));
        }
        let placeholders = (1..=channels.len())
            .map(|index| format!("?{index}"))
            .collect::<Vec<_>>()
            .join(", ");
        let limit_parameter = channels.len() + 1;
        let sql = format!(
            "SELECT event_json, channel_id, received_at
             FROM events
             WHERE channel_id IN ({placeholders})
             ORDER BY created_at DESC, event_id ASC
             LIMIT ?{limit_parameter}"
        );
        let mut parameters = channels
            .iter()
            .map(|channel| SqlValue::Text(channel.as_str().to_string()))
            .collect::<Vec<_>>();
        parameters.push(SqlValue::Integer(
            i64::try_from(scan_budget + 1)
                .map_err(|error| StorageError::Corrupt(error.to_string()))?,
        ));
        let connection = self.connection.lock();
        let mut statement = connection.prepare(&sql)?;
        let rows = statement.query_map(params_from_iter(parameters), |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;
        let mut events = Vec::new();
        let mut truncated = false;
        for row in rows {
            let (raw, channel_raw, received_at) = row?;
            if events.len() == scan_budget {
                truncated = true;
                break;
            }
            let event: Event = serde_json::from_slice(&raw)
                .map_err(|error| StorageError::Corrupt(error.to_string()))?;
            let channel_id = Uuid::parse_str(&channel_raw)
                .map_err(|error| StorageError::Corrupt(error.to_string()))?;
            let received_at: DateTime<Utc> = DateTime::from_timestamp(received_at, 0)
                .ok_or_else(|| StorageError::Corrupt("invalid received_at".to_string()))?;
            events.push(StoredEvent::with_received_at(
                event,
                received_at,
                Some(channel_id),
                true,
            ));
        }
        Ok((events, truncated))
    }
}

fn load_signed_snapshot_payload(
    connection: &Connection,
    edge_pubkey: &PublicKey,
) -> Result<Option<AuthorizationSnapshotPayload>, StorageError> {
    let row: Option<(String, i64, i64, String, Vec<u8>)> = connection
        .query_row(
            "SELECT edge_pubkey, verified_at, expires_at,
                    snapshot_event_id, snapshot_event_bytes
             FROM authorization_snapshot WHERE singleton = 1",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()?;
    let Some((stored_pubkey, verified_at, expires_at, stored_event_id, bytes)) = row else {
        return Ok(None);
    };
    if stored_pubkey != edge_pubkey.to_hex() {
        return Err(StorageError::AuthorizationSnapshot(
            "snapshot signer does not match the configured edge identity".to_string(),
        ));
    }
    let event: Event = serde_json::from_slice(&bytes)
        .map_err(|error| StorageError::AuthorizationSnapshot(error.to_string()))?;
    if event.id.to_hex() != stored_event_id
        || event.pubkey != *edge_pubkey
        || event.kind != Kind::Custom(AUTHORIZATION_SNAPSHOT_KIND)
        || buzz_core::verification::verify_event(&event).is_err()
    {
        return Err(StorageError::AuthorizationSnapshot(
            "snapshot signature verification failed".to_string(),
        ));
    }
    let payload: AuthorizationSnapshotPayload = serde_json::from_str(&event.content)
        .map_err(|error| StorageError::AuthorizationSnapshot(error.to_string()))?;
    if payload.verified_at != verified_at || payload.expires_at != expires_at {
        return Err(StorageError::AuthorizationSnapshot(
            "snapshot timestamps do not match the database record".to_string(),
        ));
    }
    for channel in &payload.channels {
        validate_channel_authorization(channel)?;
    }
    Ok(Some(payload))
}

fn persist_snapshot_transaction(
    transaction: &Transaction<'_>,
    payload: &AuthorizationSnapshotPayload,
    edge_keys: &Keys,
    community_id: Uuid,
) -> Result<Event, StorageError> {
    for channel in &payload.channels {
        validate_channel_authorization(channel)?;
    }
    let content = serde_json::to_string(payload)
        .map_err(|error| StorageError::AuthorizationSnapshot(error.to_string()))?;
    let verified_at = u64::try_from(payload.verified_at)
        .map_err(|error| StorageError::AuthorizationSnapshot(error.to_string()))?;
    let snapshot = EventBuilder::new(Kind::Custom(AUTHORIZATION_SNAPSHOT_KIND), content)
        .tags([
            Tag::identifier("edge-authorization-snapshot"),
            Tag::parse(["community", community_id.to_string().as_str()])
                .map_err(|error| StorageError::AuthorizationSnapshot(error.to_string()))?,
        ])
        .custom_created_at(Timestamp::from(verified_at))
        .sign_with_keys(edge_keys)
        .map_err(|error| StorageError::AuthorizationSnapshot(error.to_string()))?;

    transaction.execute("UPDATE selected_channels SET edge_eligible = 0", [])?;
    for channel in &payload.channels {
        transaction.execute(
            "UPDATE selected_channels
             SET edge_eligible = CASE WHEN active = 1 THEN 1 ELSE 0 END,
                 eligibility_checked_at = ?2,
                 updated_at = ?2
             WHERE channel_id = ?1",
            params![channel.channel_id.to_string(), payload.verified_at],
        )?;
        replace_channel_members_transaction(transaction, channel)?;
    }
    transaction.execute(
        "INSERT INTO authorization_snapshot(
             singleton, edge_pubkey, verified_at, expires_at,
             snapshot_event_id, snapshot_event_bytes
         ) VALUES (1, ?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(singleton) DO UPDATE SET
             edge_pubkey = excluded.edge_pubkey,
             verified_at = excluded.verified_at,
             expires_at = excluded.expires_at,
             snapshot_event_id = excluded.snapshot_event_id,
             snapshot_event_bytes = excluded.snapshot_event_bytes",
        params![
            edge_keys.public_key().to_hex(),
            payload.verified_at,
            payload.expires_at,
            snapshot.id.to_hex(),
            snapshot.as_json().as_bytes(),
        ],
    )?;
    Ok(snapshot)
}

fn snapshot_relay_identity(
    payload: &AuthorizationSnapshotPayload,
) -> Result<PublicKey, StorageError> {
    let mut relay_identity = None;
    for channel in &payload.channels {
        let event: Event = serde_json::from_slice(&channel.membership_event_bytes)
            .map_err(|error| StorageError::AuthorizationSnapshot(error.to_string()))?;
        if relay_identity.is_some_and(|identity| identity != event.pubkey) {
            return Err(StorageError::AuthorizationSnapshot(
                "roster sources have inconsistent relay signers".to_string(),
            ));
        }
        relay_identity = Some(event.pubkey);
    }
    relay_identity.ok_or_else(|| {
        StorageError::AuthorizationSnapshot("snapshot contains no relay roster source".to_string())
    })
}

fn event_cursor(event: &Event) -> Result<UpstreamCursor, StorageError> {
    Ok(UpstreamCursor {
        created_at: i64::try_from(event.created_at.as_secs())
            .map_err(|error| StorageError::AuthorizationSignal(error.to_string()))?,
        event_id: event.id,
    })
}

fn cursor_is_newer(candidate: UpstreamCursor, current: UpstreamCursor) -> bool {
    candidate.created_at > current.created_at
        || (candidate.created_at == current.created_at && candidate.event_id > current.event_id)
}

fn membership_projection_is_newer(
    candidate: &VerifiedChannelAuthorization,
    current: &VerifiedChannelAuthorization,
) -> bool {
    candidate.membership_event_created_at > current.membership_event_created_at
        || (candidate.membership_event_created_at == current.membership_event_created_at
            && candidate.membership_event_id < current.membership_event_id)
}

fn exact_uuid_tag(event: &Event, name: &str) -> Result<Uuid, StorageError> {
    let mut tags = event
        .tags
        .iter()
        .filter(|tag| tag.as_slice().first().map(String::as_str) == Some(name));
    let value = tags
        .next()
        .and_then(|tag| tag.as_slice().get(1))
        .ok_or_else(|| StorageError::AuthorizationSignal(format!("missing {name} tag")))?;
    if tags.next().is_some() {
        return Err(StorageError::AuthorizationSignal(format!(
            "signal has multiple {name} tags"
        )));
    }
    Uuid::parse_str(value).map_err(|error| StorageError::AuthorizationSignal(error.to_string()))
}

fn exact_public_key_tag(event: &Event, name: &str) -> Result<PublicKey, StorageError> {
    let mut tags = event
        .tags
        .iter()
        .filter(|tag| tag.as_slice().first().map(String::as_str) == Some(name));
    let value = tags
        .next()
        .and_then(|tag| tag.as_slice().get(1))
        .ok_or_else(|| StorageError::AuthorizationSignal(format!("missing {name} tag")))?;
    if tags.next().is_some() {
        return Err(StorageError::AuthorizationSignal(format!(
            "signal has multiple {name} tags"
        )));
    }
    PublicKey::from_hex(value).map_err(|error| StorageError::AuthorizationSignal(error.to_string()))
}

fn apply_roster_projection(
    payload: &mut AuthorizationSnapshotPayload,
    event: &Event,
    cursor: UpstreamCursor,
    edge_pubkey: &PublicKey,
) -> Result<AuthorizationSignalOutcome, StorageError> {
    let channel_id = exact_uuid_tag(event, "d")?;
    let active_authors = event
        .tags
        .iter()
        .filter(|tag| tag.as_slice().first().map(String::as_str) == Some("p"))
        .filter_map(|tag| tag.as_slice().get(1))
        .map(|value| {
            PublicKey::from_hex(value)
                .map_err(|error| StorageError::AuthorizationSignal(error.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if !active_authors.contains(edge_pubkey) {
        return Err(StorageError::AuthorizationSignal(
            "roster projection does not authorize the edge identity".to_string(),
        ));
    }
    let Some(previous) = payload
        .channels
        .iter_mut()
        .find(|channel| channel.channel_id == channel_id)
    else {
        return Ok(AuthorizationSignalOutcome::RefreshRequired { channel_id });
    };
    let mut replacement = VerifiedChannelAuthorization {
        channel_id,
        membership_event_id: event.id,
        membership_event_created_at: cursor.created_at,
        membership_event_bytes: event.as_json().into_bytes(),
        membership_fetch_cursor: Some(cursor),
        signal_cursor: previous.signal_cursor,
        edge_notification_cursor: previous.edge_notification_cursor,
        active_authors,
        removed_authors: Vec::new(),
    };
    replacement.active_authors.sort_by_key(PublicKey::to_hex);
    replacement.active_authors.dedup();
    validate_channel_authorization(&replacement)?;
    if !membership_projection_is_newer(&replacement, previous) {
        return Ok(AuthorizationSignalOutcome::Duplicate);
    }
    *previous = replacement;
    Ok(AuthorizationSignalOutcome::RosterReplaced { channel_id })
}

fn apply_system_removal(
    payload: &mut AuthorizationSnapshotPayload,
    event: &Event,
    cursor: UpstreamCursor,
) -> Result<AuthorizationSignalOutcome, StorageError> {
    let channel_id = exact_uuid_tag(event, "h")?;
    let channel = payload
        .channels
        .iter_mut()
        .find(|channel| channel.channel_id == channel_id)
        .ok_or_else(|| {
            StorageError::AuthorizationSignal("system-message channel is not eligible".to_string())
        })?;
    if channel
        .signal_cursor
        .is_some_and(|current| !cursor_is_newer(cursor, current))
    {
        return Ok(AuthorizationSignalOutcome::Duplicate);
    }
    if cursor.created_at < channel.membership_event_created_at {
        channel.signal_cursor = Some(cursor);
        return Ok(AuthorizationSignalOutcome::SignalObserved { channel_id });
    }
    let content: serde_json::Value = serde_json::from_str(&event.content)
        .map_err(|error| StorageError::AuthorizationSignal(error.to_string()))?;
    let message_type = content.get("type").and_then(serde_json::Value::as_str);
    let field = match message_type {
        Some("member_removed") => "target",
        Some("member_left") => "actor",
        _ => {
            channel.signal_cursor = Some(cursor);
            return Ok(AuthorizationSignalOutcome::SignalObserved { channel_id });
        }
    };
    let author = content
        .get(field)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| StorageError::AuthorizationSignal(format!("missing {field} pubkey")))
        .and_then(|value| {
            PublicKey::from_hex(value)
                .map_err(|error| StorageError::AuthorizationSignal(error.to_string()))
        })?;
    channel.signal_cursor = Some(cursor);
    if channel.active_authors.contains(&author) && !channel.removed_authors.contains(&author) {
        channel.removed_authors.push(author);
        channel.removed_authors.sort_by_key(PublicKey::to_hex);
    }
    Ok(AuthorizationSignalOutcome::AuthorRemoved { channel_id, author })
}

fn apply_edge_membership_notification(
    payload: &mut AuthorizationSnapshotPayload,
    event: &Event,
    cursor: UpstreamCursor,
    edge_pubkey: &PublicKey,
) -> Result<AuthorizationSignalOutcome, StorageError> {
    if exact_public_key_tag(event, "p")? != *edge_pubkey {
        return Err(StorageError::AuthorizationSignal(
            "membership notification is not addressed only to the edge identity".to_string(),
        ));
    }
    let channel_id = exact_uuid_tag(event, "h")?;
    let Some(index) = payload
        .channels
        .iter()
        .position(|channel| channel.channel_id == channel_id)
    else {
        return Ok(if event.kind.as_u16() == 44_100 {
            AuthorizationSignalOutcome::RefreshRequired { channel_id }
        } else {
            AuthorizationSignalOutcome::EdgeRemoved { channel_id }
        });
    };
    if payload.channels[index]
        .edge_notification_cursor
        .is_some_and(|current| !cursor_is_newer(cursor, current))
    {
        return Ok(AuthorizationSignalOutcome::Duplicate);
    }
    if cursor.created_at < payload.channels[index].membership_event_created_at {
        payload.channels[index].edge_notification_cursor = Some(cursor);
        return Ok(AuthorizationSignalOutcome::SignalObserved { channel_id });
    }
    if event.kind.as_u16() == 44_101 {
        payload.channels.remove(index);
        Ok(AuthorizationSignalOutcome::EdgeRemoved { channel_id })
    } else {
        payload.channels[index].edge_notification_cursor = Some(cursor);
        Ok(AuthorizationSignalOutcome::RefreshRequired { channel_id })
    }
}

fn validate_channel_authorization(
    channel: &VerifiedChannelAuthorization,
) -> Result<(), StorageError> {
    let event: Event = serde_json::from_slice(&channel.membership_event_bytes)
        .map_err(|error| StorageError::AuthorizationSnapshot(error.to_string()))?;
    let created_at = i64::try_from(event.created_at.as_secs())
        .map_err(|error| StorageError::AuthorizationSnapshot(error.to_string()))?;
    if event.id != channel.membership_event_id
        || event.kind != Kind::Custom(39_002)
        || created_at != channel.membership_event_created_at
        || buzz_core::verification::verify_event(&event).is_err()
    {
        return Err(StorageError::AuthorizationSnapshot(
            "channel roster source event failed verification".to_string(),
        ));
    }
    let event_channel = event.tags.iter().find_map(|tag| {
        let values = tag.as_slice();
        (values.first().map(String::as_str) == Some("d"))
            .then(|| values.get(1))
            .flatten()
            .and_then(|value| Uuid::parse_str(value).ok())
    });
    if event_channel != Some(channel.channel_id) {
        return Err(StorageError::AuthorizationSnapshot(
            "channel roster source has the wrong community channel".to_string(),
        ));
    }
    let mut source_authors = event
        .tags
        .iter()
        .filter_map(|tag| {
            let values = tag.as_slice();
            (values.first().map(String::as_str) == Some("p"))
                .then(|| values.get(1))
                .flatten()
                .and_then(|value| PublicKey::from_hex(value).ok())
        })
        .collect::<Vec<_>>();
    source_authors.sort_by_key(PublicKey::to_hex);
    source_authors.dedup();
    if source_authors != channel.active_authors {
        return Err(StorageError::AuthorizationSnapshot(
            "signed active-author roster differs from its source event".to_string(),
        ));
    }
    if channel.membership_fetch_cursor.is_some_and(|cursor| {
        cursor.created_at != channel.membership_event_created_at
            || cursor.event_id != channel.membership_event_id
    }) {
        return Err(StorageError::AuthorizationSnapshot(
            "roster fetch cursor differs from its source event".to_string(),
        ));
    }
    if channel
        .removed_authors
        .iter()
        .any(|author| !channel.active_authors.contains(author))
    {
        return Err(StorageError::AuthorizationSnapshot(
            "working-roster removal is absent from the source roster".to_string(),
        ));
    }
    Ok(())
}

fn filter_is_channel_only(filter: &Filter) -> bool {
    let Ok(serde_json::Value::Object(fields)) = serde_json::to_value(filter) else {
        return false;
    };
    fields
        .keys()
        .all(|key| matches!(key.as_str(), "kinds" | "#h" | "limit"))
}

fn replace_channel_members_transaction(
    transaction: &Transaction<'_>,
    channel: &VerifiedChannelAuthorization,
) -> Result<(), StorageError> {
    transaction.execute(
        "UPDATE channel_members SET active = 0, updated_at = ?2 WHERE channel_id = ?1",
        params![channel.channel_id.to_string(), unix_seconds()],
    )?;
    for member in channel
        .active_authors
        .iter()
        .filter(|member| !channel.removed_authors.contains(member))
    {
        transaction.execute(
            "INSERT INTO channel_members(channel_id, pubkey, active, updated_at)
             VALUES (?1, ?2, 1, ?3)
             ON CONFLICT(channel_id, pubkey) DO UPDATE SET
                 active = 1,
                 updated_at = excluded.updated_at",
            params![
                channel.channel_id.to_string(),
                member.to_hex(),
                unix_seconds()
            ],
        )?;
    }
    Ok(())
}

fn member_is_active(
    connection: &Connection,
    channel_id: Uuid,
    pubkey: &PublicKey,
) -> Result<bool, StorageError> {
    let active: Option<i64> = connection
        .query_row(
            "SELECT active FROM channel_members WHERE channel_id = ?1 AND pubkey = ?2",
            params![channel_id.to_string(), pubkey.to_hex()],
            |row| row.get(0),
        )
        .optional()?;
    Ok(active == Some(1))
}

fn validate_digest_part(
    event: &Event,
    channel_id: Uuid,
    edge_pubkey: &PublicKey,
    part_index: usize,
    total_parts: usize,
) -> Result<(), StorageError> {
    if event.pubkey != *edge_pubkey
        || event.kind != Kind::Custom(9)
        || buzz_core::verification::verify_event(event).is_err()
        || event.content.len() > MAX_DIGEST_CONTENT_BYTES
    {
        return Err(StorageError::InvalidDigest(
            "digest part must be a valid edge-signed kind-9 event within the content limit"
                .to_string(),
        ));
    }
    let exact_tag = |name: &str, expected: &str| {
        let mut tags = event
            .tags
            .iter()
            .filter(|tag| tag.as_slice().first().map(String::as_str) == Some(name));
        let matches = tags
            .next()
            .is_some_and(|tag| tag.as_slice().get(1).map(String::as_str) == Some(expected));
        matches && tags.next().is_none()
    };
    if !exact_tag("h", &channel_id.to_string())
        || !exact_tag("part", &part_index.to_string())
        || !exact_tag("total", &total_parts.to_string())
    {
        return Err(StorageError::InvalidDigest(
            "digest part has inconsistent channel or part/total tags".to_string(),
        ));
    }
    Ok(())
}

fn event_exists(transaction: &Transaction<'_>, event_id: &str) -> Result<bool, StorageError> {
    let exists: Option<i64> = transaction
        .query_row(
            "SELECT 1 FROM events WHERE event_id = ?1",
            [event_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(exists.is_some())
}

fn persist_event_dependencies(
    transaction: &Transaction<'_>,
    event: &Event,
) -> Result<(), StorageError> {
    for (ordinal, (ancestor_id, relation)) in event_dependencies(event).into_iter().enumerate() {
        transaction.execute(
            "INSERT OR IGNORE INTO event_dependencies(
                 child_event_id, ancestor_event_id, relation, ordinal
             ) VALUES (?1, ?2, ?3, ?4)",
            params![
                event.id.to_hex(),
                ancestor_id,
                relation,
                i64::try_from(ordinal).map_err(|error| StorageError::Corrupt(error.to_string()))?
            ],
        )?;
    }
    Ok(())
}

fn build_receipt(
    event: &Event,
    channel_id: Uuid,
    received_at: i64,
    edge_keys: &Keys,
) -> Result<Event, StorageError> {
    let received_at = u64::try_from(received_at)
        .map_err(|error| StorageError::ReceiptSigning(error.to_string()))?;
    EventBuilder::new(Kind::Custom(RECEIPT_KIND), "delivered locally")
        .tags([
            Tag::event(event.id),
            Tag::parse(["h", channel_id.to_string().as_str()])
                .map_err(|error| StorageError::ReceiptSigning(error.to_string()))?,
            Tag::public_key(event.pubkey),
        ])
        .custom_created_at(Timestamp::from(received_at))
        .sign_with_keys(edge_keys)
        .map_err(|error| StorageError::ReceiptSigning(error.to_string()))
}

fn event_dependencies(event: &Event) -> Vec<(String, &'static str)> {
    let mut dependencies = Vec::new();
    for tag in event.tags.iter() {
        let values = tag.as_slice();
        if values.first().map(String::as_str) != Some("e") {
            continue;
        }
        let Some(event_id) = values.get(1) else {
            continue;
        };
        if EventId::from_hex(event_id).is_err() {
            continue;
        }
        let relation = match values.get(3).map(String::as_str) {
            Some("root") => Some("root"),
            Some("reply") => Some("reply"),
            Some("mention") => None,
            Some(_) => None,
            None => Some("legacy"),
        };
        if let Some(relation) = relation {
            dependencies.push((event_id.clone(), relation));
        }
    }
    dependencies
}

fn unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding() -> CommunityBinding {
        CommunityBinding::new("wss://relay.example.com", Uuid::new_v4()).expect("binding")
    }

    fn policy() -> AuthorizationPolicy {
        AuthorizationPolicy::new(Duration::from_secs(72 * 60 * 60)).expect("policy")
    }

    fn authorization(
        channel_id: Uuid,
        active_authors: &[PublicKey],
        created_at: i64,
    ) -> VerifiedChannelAuthorization {
        let relay = Keys::generate();
        authorization_from_relay(&relay, channel_id, active_authors, created_at)
    }

    fn authorization_from_relay(
        relay: &Keys,
        channel_id: Uuid,
        active_authors: &[PublicKey],
        created_at: i64,
    ) -> VerifiedChannelAuthorization {
        let mut tags = vec![Tag::parse(["d", channel_id.to_string().as_str()]).expect("d tag")];
        for author in active_authors {
            tags.push(Tag::parse(["p", author.to_hex().as_str()]).expect("p tag"));
        }
        let event = EventBuilder::new(Kind::Custom(39_002), "")
            .tags(tags)
            .custom_created_at(Timestamp::from(created_at as u64))
            .sign_with_keys(relay)
            .expect("membership event");
        VerifiedChannelAuthorization {
            channel_id,
            membership_event_id: event.id,
            membership_event_created_at: created_at,
            membership_event_bytes: event.as_json().into_bytes(),
            membership_fetch_cursor: None,
            signal_cursor: None,
            edge_notification_cursor: None,
            active_authors: active_authors.to_vec(),
            removed_authors: Vec::new(),
        }
    }

    fn system_removal(relay: &Keys, channel_id: Uuid, author: PublicKey, created_at: i64) -> Event {
        EventBuilder::new(
            Kind::Custom(40_099),
            serde_json::json!({
                "type": "member_removed",
                "actor": relay.public_key().to_hex(),
                "target": author.to_hex(),
            })
            .to_string(),
        )
        .tags([Tag::parse(["h", channel_id.to_string().as_str()]).expect("h tag")])
        .custom_created_at(Timestamp::from(created_at as u64))
        .sign_with_keys(relay)
        .expect("system removal")
    }

    fn message(keys: &Keys, channel: Uuid, content: &str) -> Event {
        EventBuilder::new(Kind::Custom(9), content)
            .tags([Tag::parse(["h", channel.to_string().as_str()]).expect("h tag")])
            .sign_with_keys(keys)
            .expect("sign")
    }

    // ── Author-drain state machine (§11) ───────────────────────────────────

    /// A reply to `parent`, carrying the thread tags the dependency graph reads.
    fn reply(keys: &Keys, channel: Uuid, parent: &Event, content: &str) -> Event {
        EventBuilder::new(Kind::Custom(9), content)
            .tags([
                Tag::parse(["h", channel.to_string().as_str()]).expect("h tag"),
                Tag::parse(["e", parent.id.to_hex().as_str(), "", "root"]).expect("root tag"),
                Tag::parse(["e", parent.id.to_hex().as_str(), "", "reply"]).expect("reply tag"),
            ])
            .sign_with_keys(keys)
            .expect("sign")
    }

    fn store_with_event(store: &EdgeStore, event: &Event, channel: Uuid, edge: &Keys) {
        store
            .insert_local_event(event, event.as_json().as_bytes(), channel, edge)
            .expect("insert");
    }

    #[test]
    fn an_author_can_only_claim_its_own_rows() {
        // The sidecar cannot submit another identity's events upstream —
        // canonical ingest refuses — so a cross-identity claim could never be
        // drained. Ownership is enforced here, not left to the caller.
        let store = EdgeStore::open_in_memory(binding(), policy()).expect("store");
        let (mine, theirs, edge) = (Keys::generate(), Keys::generate(), Keys::generate());
        let channel = Uuid::new_v4();
        store_with_event(&store, &message(&mine, channel, "mine"), channel, &edge);
        store_with_event(&store, &message(&theirs, channel, "theirs"), channel, &edge);

        let claimed = store
            .claim_outbox_batch(&mine.public_key(), "token-a", 10, 1_000, 60)
            .expect("claim");
        assert_eq!(claimed.len(), 1, "must not claim another author's row");
        assert_eq!(claimed[0].event.pubkey, mine.public_key());
    }

    #[test]
    fn a_reply_is_not_claimable_until_its_parent_is_delivered() {
        // Canonical ingest rejects a reply whose parent is not stored, so
        // draining a child first would guarantee a rejection. Gating is on the
        // dependency graph, not on authorship — threads cross identities.
        let store = EdgeStore::open_in_memory(binding(), policy()).expect("store");
        let (author, edge) = (Keys::generate(), Keys::generate());
        let channel = Uuid::new_v4();
        let root = message(&author, channel, "root");
        store_with_event(&store, &root, channel, &edge);
        let child = reply(&author, channel, &root, "child");
        store_with_event(&store, &child, channel, &edge);

        let first = store
            .claim_outbox_batch(&author.public_key(), "t1", 10, 1_000, 60)
            .expect("claim");
        assert_eq!(first.len(), 1, "only the root is claimable");
        assert_eq!(first[0].event_id, root.id);

        // Still blocked while the parent is merely claimed, not delivered.
        let blocked = store
            .claim_outbox_batch(&author.public_key(), "t2", 10, 1_000, 60)
            .expect("claim");
        assert!(blocked.is_empty(), "child must wait for the parent");

        store
            .acknowledge_outbox_row("t1", &root.id, DrainOutcome::Delivered)
            .expect("ack");
        let now_free = store
            .claim_outbox_batch(&author.public_key(), "t3", 10, 1_000, 60)
            .expect("claim");
        assert_eq!(now_free.len(), 1);
        assert_eq!(now_free[0].event_id, child.id);
    }

    #[test]
    fn an_expired_lease_returns_the_row_and_a_retry_gets_identical_bytes() {
        // The crash window between upstream accepting and the sidecar hearing
        // about it must not strand a row. Expiry returns it; the retry must
        // re-submit the SAME signed bytes so upstream's event-ID dedup makes
        // storage exactly-once.
        let store = EdgeStore::open_in_memory(binding(), policy()).expect("store");
        let (author, edge) = (Keys::generate(), Keys::generate());
        let channel = Uuid::new_v4();
        let event = message(&author, channel, "hello");
        store_with_event(&store, &event, channel, &edge);

        let first = store
            .claim_outbox_batch(&author.public_key(), "lost", 10, 1_000, 60)
            .expect("claim");
        assert_eq!(first.len(), 1);
        // Author vanishes without acknowledging.
        assert!(store
            .claim_outbox_batch(&author.public_key(), "other", 10, 1_010, 60)
            .expect("claim")
            .is_empty());

        assert_eq!(store.expire_outbox_leases(1_100).expect("expire"), 1);
        let retry = store
            .claim_outbox_batch(&author.public_key(), "fresh", 10, 1_100, 60)
            .expect("claim");
        assert_eq!(retry.len(), 1);
        assert_eq!(
            retry[0].event.as_json(),
            first[0].event.as_json(),
            "retry must re-submit byte-identical event"
        );
    }

    #[test]
    fn duplicate_counts_as_delivered() {
        // Upstream already having the event proves it reached canonical
        // history. Treating duplicate as failure would retry forever.
        let store = EdgeStore::open_in_memory(binding(), policy()).expect("store");
        let (author, edge) = (Keys::generate(), Keys::generate());
        let channel = Uuid::new_v4();
        let event = message(&author, channel, "hello");
        store_with_event(&store, &event, channel, &edge);
        store
            .claim_outbox_batch(&author.public_key(), "t", 10, 1_000, 60)
            .expect("claim");

        assert!(store
            .acknowledge_outbox_row("t", &event.id, DrainOutcome::Duplicate)
            .expect("ack"));
        assert_eq!(store.outbox_summary().expect("summary").delivered_exact, 1);
        assert_eq!(store.pending_count().expect("pending"), 0);
    }

    #[test]
    fn a_transient_failure_changes_nothing_and_waits_for_expiry() {
        // Recording a state here could contradict an upstream acceptance the
        // author has not observed. The lease is the only safe arbiter.
        let store = EdgeStore::open_in_memory(binding(), policy()).expect("store");
        let (author, edge) = (Keys::generate(), Keys::generate());
        let channel = Uuid::new_v4();
        let event = message(&author, channel, "hello");
        store_with_event(&store, &event, channel, &edge);
        store
            .claim_outbox_batch(&author.public_key(), "t", 10, 1_000, 60)
            .expect("claim");

        assert!(!store
            .acknowledge_outbox_row("t", &event.id, DrainOutcome::Transient)
            .expect("ack"));
        let summary = store.outbox_summary().expect("summary");
        assert_eq!(summary.claimed, 1, "row stays claimed");
        assert_eq!(summary.delivered_exact, 0);
        assert_eq!(summary.quarantined, 0);
    }

    #[test]
    fn a_permanent_rejection_quarantines_with_its_reason() {
        let store = EdgeStore::open_in_memory(binding(), policy()).expect("store");
        let (author, edge) = (Keys::generate(), Keys::generate());
        let channel = Uuid::new_v4();
        let event = message(&author, channel, "hello");
        store_with_event(&store, &event, channel, &edge);
        store
            .claim_outbox_batch(&author.public_key(), "t", 10, 1_000, 60)
            .expect("claim");

        assert!(store
            .acknowledge_outbox_row(
                "t",
                &event.id,
                DrainOutcome::Rejected("membership revoked".into())
            )
            .expect("ack"));
        assert_eq!(store.outbox_summary().expect("summary").quarantined, 1);
        // Quarantined rows are never handed out again.
        assert!(store
            .claim_outbox_batch(&author.public_key(), "t2", 10, 2_000, 60)
            .expect("claim")
            .is_empty());
    }

    #[test]
    fn a_stale_claim_token_cannot_acknowledge_or_renew() {
        // An author whose lease lapsed must not be able to reach back in and
        // overwrite the state of rows another drain now owns.
        let store = EdgeStore::open_in_memory(binding(), policy()).expect("store");
        let (author, edge) = (Keys::generate(), Keys::generate());
        let channel = Uuid::new_v4();
        let event = message(&author, channel, "hello");
        store_with_event(&store, &event, channel, &edge);
        store
            .claim_outbox_batch(&author.public_key(), "current", 10, 1_000, 60)
            .expect("claim");

        assert!(!store
            .acknowledge_outbox_row("stale", &event.id, DrainOutcome::Delivered)
            .expect("ack"));
        assert_eq!(
            store.renew_outbox_lease("stale", 1_010, 60).expect("renew"),
            0
        );
        assert_eq!(
            store
                .renew_outbox_lease("current", 1_010, 60)
                .expect("renew"),
            1
        );
        assert_eq!(store.outbox_summary().expect("summary").claimed, 1);
    }

    #[test]
    fn summary_separates_the_two_delivered_states() {
        // §13: "delivered locally" and "synced to canonical history" must be
        // labelled separately everywhere they surface.
        let summary = OutboxSummary {
            delivered_exact: 3,
            delivered_via_digest: 2,
            ..OutboxSummary::default()
        };
        assert_ne!(summary.delivered_exact, summary.delivered_via_digest);
        assert_eq!(summary.delivered_exact + summary.delivered_via_digest, 5);
    }

    #[test]
    fn persists_event_receipt_and_outbox_atomically() {
        let store = EdgeStore::open_in_memory(binding(), policy()).expect("store");
        let author = Keys::generate();
        let edge = Keys::generate();
        let channel = Uuid::new_v4();
        let event = message(&author, channel, "hello");

        assert_eq!(
            store
                .insert_local_event(&event, event.as_json().as_bytes(), channel, &edge)
                .expect("insert"),
            InsertOutcome::Inserted
        );
        assert_eq!(store.event_count().expect("count"), 1);
        assert_eq!(store.pending_count().expect("pending"), 1);
        let receipt = store
            .receipt(&event.id)
            .expect("receipt query")
            .expect("receipt");
        assert_eq!(receipt.pubkey, edge.public_key());
        assert!(receipt.verify().is_ok());
    }

    #[test]
    fn duplicate_event_is_idempotent() {
        let store = EdgeStore::open_in_memory(binding(), policy()).expect("store");
        let author = Keys::generate();
        let edge = Keys::generate();
        let channel = Uuid::new_v4();
        let event = message(&author, channel, "hello");
        store
            .insert_local_event(&event, event.as_json().as_bytes(), channel, &edge)
            .expect("first insert");

        assert_eq!(
            store
                .insert_local_event(&event, event.as_json().as_bytes(), channel, &edge)
                .expect("duplicate"),
            InsertOutcome::Duplicate
        );
        assert_eq!(store.event_count().expect("count"), 1);
        assert_eq!(store.pending_count().expect("pending"), 1);
    }

    #[test]
    fn owner_attestation_never_grants_agent_channel_access() {
        let store = EdgeStore::open_in_memory(binding(), policy()).expect("store");
        let agent = Keys::generate();
        let owner = Keys::generate();
        let edge = Keys::generate();
        let different_owner = Keys::generate();
        let channel = Uuid::new_v4();
        store
            .set_channel_selected(channel, true)
            .expect("selection");
        store
            .persist_verified_authorization_snapshot(
                &[authorization(
                    channel,
                    &[edge.public_key(), owner.public_key()],
                    unix_seconds(),
                )],
                unix_seconds(),
                &edge,
            )
            .expect("snapshot");

        store
            .record_nip_oa_owner(&agent.public_key(), &owner.public_key())
            .expect("record owner");
        assert!(store
            .principal_can_access(channel, &owner.public_key())
            .expect("owner access"));
        assert!(!store
            .principal_can_access(channel, &agent.public_key())
            .expect("agent access"));
        assert!(matches!(
            store.record_nip_oa_owner(&agent.public_key(), &different_owner.public_key()),
            Err(StorageError::OwnerConflict)
        ));
    }

    #[test]
    fn unselected_channel_is_denied_even_for_cached_member() {
        let store = EdgeStore::open_in_memory(binding(), policy()).expect("store");
        let member = Keys::generate();
        let channel = Uuid::new_v4();
        store
            .persist_verified_authorization_snapshot(
                &[authorization(
                    channel,
                    &[member.public_key()],
                    unix_seconds(),
                )],
                unix_seconds(),
                &member,
            )
            .expect("snapshot");

        assert!(!store
            .principal_can_access(channel, &member.public_key())
            .expect("access"));
    }

    #[test]
    fn startup_selection_replacement_drops_removed_channels() {
        let store = EdgeStore::open_in_memory(binding(), policy()).expect("store");
        let retained = Uuid::new_v4();
        let removed = Uuid::new_v4();
        store
            .replace_selected_channels(&[retained, removed])
            .expect("initial selection");
        store
            .replace_selected_channels(&[retained])
            .expect("replacement");
        assert_eq!(store.selected_channels().expect("selected"), vec![retained]);
    }

    #[test]
    fn nip98_replay_is_rejected_atomically() {
        let store = EdgeStore::open_in_memory(binding(), policy()).expect("store");
        let event_id = EventId::from_byte_array([7; 32]);
        assert!(store.mark_nip98_auth(&event_id).expect("first"));
        assert!(!store.mark_nip98_auth(&event_id).expect("replay"));
    }

    #[test]
    fn database_reopen_preserves_pending_event() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("edge.sqlite3");
        let author = Keys::generate();
        let edge = Keys::generate();
        let channel = Uuid::new_v4();
        let event = message(&author, channel, "survives restart");
        let binding = binding();
        EdgeStore::open(&path, binding.clone(), policy())
            .expect("open")
            .insert_local_event(&event, event.as_json().as_bytes(), channel, &edge)
            .expect("insert");

        let reopened = EdgeStore::open(&path, binding, policy()).expect("reopen");
        assert_eq!(reopened.event_count().expect("events"), 1);
        assert_eq!(reopened.pending_count().expect("pending"), 1);
        assert!(reopened.receipt(&event.id).expect("receipt").is_some());
    }

    #[test]
    fn interrupted_upstream_backfill_is_cached_without_committing_its_cursor() {
        let store = EdgeStore::open_in_memory(binding(), policy()).expect("store");
        let author = Keys::generate();
        let channel = Uuid::new_v4();
        let first = EventBuilder::new(Kind::Custom(9), "canonical one")
            .tags([Tag::parse(["h", channel.to_string().as_str()]).expect("h")])
            .custom_created_at(Timestamp::from(1_800_000_000_u64))
            .sign_with_keys(&author)
            .expect("first");
        let older = EventBuilder::new(Kind::Custom(9), "canonical older")
            .tags([Tag::parse(["h", channel.to_string().as_str()]).expect("h")])
            .custom_created_at(Timestamp::from(1_799_999_999_u64))
            .sign_with_keys(&author)
            .expect("older");

        assert_eq!(
            store
                .insert_upstream_event(&first, first.as_json().as_bytes(), channel)
                .expect("insert first"),
            InsertOutcome::Inserted
        );
        assert_eq!(
            store
                .insert_upstream_event(&older, older.as_json().as_bytes(), channel)
                .expect("insert older"),
            InsertOutcome::Inserted
        );
        assert_eq!(store.event_count().expect("events"), 2);
        assert_eq!(store.pending_count().expect("pending"), 0);
        assert!(store.receipt(&first.id).expect("receipt").is_none());
        assert_eq!(store.upstream_cursor(channel).expect("cursor"), None);
        let completed = UpstreamCursor {
            created_at: 1_800_000_000,
            event_id: first.id,
        };
        store
            .advance_upstream_cursor(channel, &completed)
            .expect("complete backfill");
        assert_eq!(
            store.upstream_cursor(channel).expect("cursor"),
            Some(completed)
        );
        assert_eq!(
            store
                .insert_upstream_event(&first, first.as_json().as_bytes(), channel)
                .expect("duplicate"),
            InsertOutcome::Duplicate
        );
    }

    #[test]
    fn digest_materialization_survives_restart_with_identical_signed_bytes() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("edge.sqlite3");
        let binding = binding();
        let author = Keys::generate();
        let edge = Keys::generate();
        let channel = Uuid::new_v4();
        let source = message(&author, channel, "old local message");
        let part = EventBuilder::new(Kind::Custom(9), "catch-up digest")
            .tags([
                Tag::parse(["h", channel.to_string().as_str()]).expect("h tag"),
                Tag::parse(["part", "1"]).expect("part tag"),
                Tag::parse(["total", "1"]).expect("total tag"),
            ])
            .sign_with_keys(&edge)
            .expect("sign digest");
        let expected_bytes = part.as_json().into_bytes();
        let store = EdgeStore::open(&path, binding.clone(), policy()).expect("open");
        store
            .insert_local_event(&source, source.as_json().as_bytes(), channel, &edge)
            .expect("insert source");
        store
            .materialize_digest_batch(
                "batch-1",
                channel,
                &[source.id],
                std::slice::from_ref(&part),
                &edge.public_key(),
            )
            .expect("materialize");
        drop(store);

        let reopened = EdgeStore::open(&path, binding, policy()).expect("reopen");
        let loaded = reopened.digest_parts("batch-1").expect("load parts");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].event_id, part.id);
        assert_eq!(loaded[0].event_bytes, expected_bytes);
    }

    #[test]
    fn database_header_rejects_equal_channel_ids_from_another_community() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("edge.sqlite3");
        let first = binding();
        EdgeStore::open(&path, first, policy()).expect("initial bind");
        let second = CommunityBinding::new("wss://relay.example.com", Uuid::new_v4())
            .expect("second binding");
        assert!(matches!(
            EdgeStore::open(&path, second, policy()),
            Err(StorageError::CommunityMismatch)
        ));
    }

    #[test]
    fn canonical_binding_requires_a_plain_origin() {
        let community = Uuid::new_v4();
        assert!(CommunityBinding::new("wss://relay.example.com", community).is_ok());
        assert!(CommunityBinding::new("wss://relay.example.com/path", community).is_err());
        assert!(CommunityBinding::new("wss://user@relay.example.com", community).is_err());
    }

    #[test]
    fn signed_authorization_lease_is_bounded_and_survives_restart() {
        let store = EdgeStore::open_in_memory(binding(), policy()).expect("store");
        let edge = Keys::generate();
        let channel = Uuid::new_v4();
        store
            .set_channel_selected(channel, true)
            .expect("selection");
        let verified_at = 1_800_000_000;
        let lease_seconds = policy().lease_seconds();
        let channel_authorization = authorization(channel, &[edge.public_key()], verified_at);
        let mut expected_authorization = channel_authorization.clone();
        expected_authorization.membership_fetch_cursor = Some(UpstreamCursor {
            created_at: verified_at,
            event_id: channel_authorization.membership_event_id,
        });
        let snapshot = store
            .persist_verified_authorization_snapshot(
                std::slice::from_ref(&channel_authorization),
                verified_at,
                &edge,
            )
            .expect("snapshot");
        assert!(snapshot.verify().is_ok());
        assert_eq!(
            store
                .load_authorization_lease(&edge.public_key(), verified_at + 1)
                .expect("valid lease"),
            AuthorizationLease::Valid {
                channels: vec![expected_authorization],
                verified_at,
                expires_at: verified_at + lease_seconds,
            }
        );
        assert_eq!(
            store
                .load_authorization_lease(&edge.public_key(), verified_at + lease_seconds + 1)
                .expect("expired lease"),
            AuthorizationLease::Expired {
                expires_at: verified_at + lease_seconds,
            }
        );
    }

    #[test]
    fn principal_access_expires_at_the_signed_lease_boundary() {
        let store = EdgeStore::open_in_memory(binding(), policy()).expect("store");
        let edge = Keys::generate();
        let member = Keys::generate();
        let channel = Uuid::new_v4();
        let verified_at = 1_800_000_000;
        let lease_seconds = policy().lease_seconds();
        store
            .set_channel_selected(channel, true)
            .expect("selection");
        store
            .persist_verified_authorization_snapshot(
                &[authorization(
                    channel,
                    &[edge.public_key(), member.public_key()],
                    verified_at,
                )],
                verified_at,
                &edge,
            )
            .expect("snapshot");
        assert!(store
            .principal_can_access_at(channel, &member.public_key(), verified_at + lease_seconds)
            .expect("boundary access"));
        assert!(!store
            .principal_can_access_at(
                channel,
                &member.public_key(),
                verified_at + lease_seconds + 1
            )
            .expect("expired access"));
    }

    #[test]
    fn digest_sources_are_unique_across_batches_and_inputs_are_validated() {
        let store = EdgeStore::open_in_memory(binding(), policy()).expect("store");
        let author = Keys::generate();
        let edge = Keys::generate();
        let channel = Uuid::new_v4();
        let source = message(&author, channel, "source");
        let part = EventBuilder::new(Kind::Custom(9), "digest")
            .tags([
                Tag::parse(["h", channel.to_string().as_str()]).expect("h"),
                Tag::parse(["part", "1"]).expect("part"),
                Tag::parse(["total", "1"]).expect("total"),
            ])
            .sign_with_keys(&edge)
            .expect("digest");
        store
            .insert_local_event(&source, source.as_json().as_bytes(), channel, &edge)
            .expect("source");
        store
            .materialize_digest_batch(
                "batch-a",
                channel,
                &[source.id],
                std::slice::from_ref(&part),
                &edge.public_key(),
            )
            .expect("first batch");
        assert!(store
            .materialize_digest_batch(
                "batch-b",
                channel,
                &[source.id],
                &[part],
                &edge.public_key(),
            )
            .is_err());
        assert!(store.digest_parts("batch-b").expect("rollback").is_empty());
    }

    #[test]
    fn digest_materialization_rejects_oversize_or_ambiguous_parts() {
        let store = EdgeStore::open_in_memory(binding(), policy()).expect("store");
        let author = Keys::generate();
        let edge = Keys::generate();
        let channel = Uuid::new_v4();
        let source = message(&author, channel, "source");
        store
            .insert_local_event(&source, source.as_json().as_bytes(), channel, &edge)
            .expect("source");

        let oversize = EventBuilder::new(Kind::Custom(9), "x".repeat(MAX_DIGEST_CONTENT_BYTES + 1))
            .tags([
                Tag::parse(["h", channel.to_string().as_str()]).expect("h"),
                Tag::parse(["part", "1"]).expect("part"),
                Tag::parse(["total", "1"]).expect("total"),
            ])
            .sign_with_keys(&edge)
            .expect("oversize");
        assert!(store
            .materialize_digest_batch(
                "oversize",
                channel,
                &[source.id],
                &[oversize],
                &edge.public_key(),
            )
            .is_err());

        let ambiguous = EventBuilder::new(Kind::Custom(9), "digest")
            .tags([
                Tag::parse(["h", channel.to_string().as_str()]).expect("h"),
                Tag::parse(["part", "1"]).expect("part"),
                Tag::parse(["part", "99"]).expect("ambiguous part"),
                Tag::parse(["total", "1"]).expect("total"),
            ])
            .sign_with_keys(&edge)
            .expect("ambiguous");
        assert!(store
            .materialize_digest_batch(
                "ambiguous",
                channel,
                &[source.id],
                &[ambiguous],
                &edge.public_key(),
            )
            .is_err());
        assert!(store.digest_parts("oversize").expect("rollback").is_empty());
        assert!(store
            .digest_parts("ambiguous")
            .expect("rollback")
            .is_empty());
    }

    #[test]
    fn system_removal_survives_an_unchanged_roster_refresh() {
        let store = EdgeStore::open_in_memory(binding(), policy()).expect("store");
        let edge = Keys::generate();
        let relay = Keys::generate();
        let author = Keys::generate();
        let channel = Uuid::new_v4();
        let now = unix_seconds();
        store.set_channel_selected(channel, true).expect("select");
        let projection = authorization_from_relay(
            &relay,
            channel,
            &[edge.public_key(), author.public_key()],
            now,
        );
        store
            .persist_verified_authorization_snapshot(std::slice::from_ref(&projection), now, &edge)
            .expect("snapshot");

        let removal = system_removal(&relay, channel, author.public_key(), now + 1);
        assert_eq!(
            store
                .apply_authorization_signal(&removal, &edge)
                .expect("removal"),
            AuthorizationSignalOutcome::AuthorRemoved {
                channel_id: channel,
                author: author.public_key(),
            }
        );
        assert!(!store
            .principal_can_access(channel, &author.public_key())
            .expect("removed"));
        let signal_cursor = store
            .authorization_signal_cursor(&edge.public_key(), channel)
            .expect("cursor")
            .expect("stored cursor");

        store
            .persist_verified_authorization_snapshot(&[projection], now + 2, &edge)
            .expect("unchanged refresh");
        assert!(!store
            .principal_can_access(channel, &author.public_key())
            .expect("unchanged roster cannot reauthorize"));
        assert_eq!(
            store
                .authorization_signal_cursor(&edge.public_key(), channel)
                .expect("cursor"),
            Some(signal_cursor)
        );
        let AuthorizationLease::Valid { channels, .. } = store
            .load_authorization_lease(&edge.public_key(), now + 2)
            .expect("lease")
        else {
            panic!("expected valid lease");
        };
        assert_eq!(channels[0].membership_event_created_at, now);
        assert_eq!(
            channels[0].membership_fetch_cursor,
            Some(UpstreamCursor {
                created_at: now,
                event_id: channels[0].membership_event_id,
            })
        );
        assert_eq!(channels[0].removed_authors, vec![author.public_key()]);
    }

    #[test]
    fn pre_snapshot_system_removal_cannot_override_a_newer_roster() {
        let store = EdgeStore::open_in_memory(binding(), policy()).expect("store");
        let edge = Keys::generate();
        let relay = Keys::generate();
        let author = Keys::generate();
        let channel = Uuid::new_v4();
        let now = unix_seconds();
        store.set_channel_selected(channel, true).expect("select");
        store
            .persist_verified_authorization_snapshot(
                &[authorization_from_relay(
                    &relay,
                    channel,
                    &[edge.public_key(), author.public_key()],
                    now,
                )],
                now,
                &edge,
            )
            .expect("snapshot");

        assert_eq!(
            store
                .apply_authorization_signal(
                    &system_removal(&relay, channel, author.public_key(), now - 1),
                    &edge,
                )
                .expect("historical signal"),
            AuthorizationSignalOutcome::SignalObserved {
                channel_id: channel
            }
        );
        assert!(store
            .principal_can_access(channel, &author.public_key())
            .expect("newer roster remains authoritative"));
    }

    #[test]
    fn changed_roster_projection_can_reauthorize_a_removed_author() {
        let store = EdgeStore::open_in_memory(binding(), policy()).expect("store");
        let edge = Keys::generate();
        let relay = Keys::generate();
        let author = Keys::generate();
        let channel = Uuid::new_v4();
        let now = unix_seconds();
        store.set_channel_selected(channel, true).expect("select");
        store
            .persist_verified_authorization_snapshot(
                &[authorization_from_relay(
                    &relay,
                    channel,
                    &[edge.public_key(), author.public_key()],
                    now,
                )],
                now,
                &edge,
            )
            .expect("snapshot");
        store
            .apply_authorization_signal(
                &system_removal(&relay, channel, author.public_key(), now + 1),
                &edge,
            )
            .expect("removal");

        let changed = authorization_from_relay(
            &relay,
            channel,
            &[edge.public_key(), author.public_key()],
            now + 2,
        );
        assert_eq!(
            store
                .apply_authorization_signal(
                    &Event::from_json(&changed.membership_event_bytes).expect("event"),
                    &edge,
                )
                .expect("changed roster"),
            AuthorizationSignalOutcome::RosterReplaced {
                channel_id: channel
            }
        );
        assert!(store
            .principal_can_access(channel, &author.public_key())
            .expect("reauthorized"));
    }

    #[test]
    fn suppressed_removal_carriers_leave_local_access_until_canonical_rejection() {
        let store = EdgeStore::open_in_memory(binding(), policy()).expect("store");
        let edge = Keys::generate();
        let relay = Keys::generate();
        let author = Keys::generate();
        let channel = Uuid::new_v4();
        let now = unix_seconds();
        store.set_channel_selected(channel, true).expect("select");
        let unchanged = authorization_from_relay(
            &relay,
            channel,
            &[edge.public_key(), author.public_key()],
            now,
        );
        store
            .persist_verified_authorization_snapshot(std::slice::from_ref(&unchanged), now, &edge)
            .expect("snapshot");
        store
            .persist_verified_authorization_snapshot(&[unchanged], now + 1, &edge)
            .expect("unchanged refresh");
        assert!(store
            .principal_can_access_at(channel, &author.public_key(), now + 1)
            .expect("disclosed residual"));

        store
            .revoke_author_after_canonical_rejection(channel, author.public_key(), &edge)
            .expect("canonical rejection");
        assert!(!store
            .principal_can_access_at(channel, &author.public_key(), now + 1)
            .expect("canonical rejection revokes locally"));
    }

    #[test]
    fn membership_notifications_are_edge_self_only() {
        let store = EdgeStore::open_in_memory(binding(), policy()).expect("store");
        let edge = Keys::generate();
        let relay = Keys::generate();
        let other = Keys::generate();
        let channel = Uuid::new_v4();
        let now = unix_seconds();
        store.set_channel_selected(channel, true).expect("select");
        store
            .persist_verified_authorization_snapshot(
                &[authorization_from_relay(
                    &relay,
                    channel,
                    &[edge.public_key()],
                    now,
                )],
                now,
                &edge,
            )
            .expect("snapshot");
        let notification = |target: PublicKey, created_at: i64| {
            EventBuilder::new(Kind::Custom(44_101), "")
                .tags([
                    Tag::parse(["p", target.to_hex().as_str()]).expect("p"),
                    Tag::parse(["h", channel.to_string().as_str()]).expect("h"),
                ])
                .custom_created_at(Timestamp::from(created_at as u64))
                .sign_with_keys(&relay)
                .expect("notification")
        };
        assert!(matches!(
            store.apply_authorization_signal(&notification(other.public_key(), now + 1), &edge),
            Err(StorageError::AuthorizationSignal(_))
        ));
        assert!(store
            .channel_is_edge_eligible(channel)
            .expect("still eligible"));
        assert_eq!(
            store
                .apply_authorization_signal(&notification(edge.public_key(), now - 1), &edge)
                .expect("historical self removal"),
            AuthorizationSignalOutcome::SignalObserved {
                channel_id: channel
            }
        );
        assert!(store
            .channel_is_edge_eligible(channel)
            .expect("newer roster remains eligible"));
        assert_eq!(
            store
                .apply_authorization_signal(&notification(edge.public_key(), now + 2), &edge)
                .expect("self removal"),
            AuthorizationSignalOutcome::EdgeRemoved {
                channel_id: channel
            }
        );
        assert!(!store.channel_is_edge_eligible(channel).expect("revoked"));
    }

    #[test]
    fn forged_system_removal_is_rejected() {
        let store = EdgeStore::open_in_memory(binding(), policy()).expect("store");
        let edge = Keys::generate();
        let relay = Keys::generate();
        let impostor = Keys::generate();
        let author = Keys::generate();
        let channel = Uuid::new_v4();
        let now = unix_seconds();
        store.set_channel_selected(channel, true).expect("select");
        store
            .persist_verified_authorization_snapshot(
                &[authorization_from_relay(
                    &relay,
                    channel,
                    &[edge.public_key(), author.public_key()],
                    now,
                )],
                now,
                &edge,
            )
            .expect("snapshot");
        assert!(matches!(
            store.apply_authorization_signal(
                &system_removal(&impostor, channel, author.public_key(), now + 1),
                &edge,
            ),
            Err(StorageError::AuthorizationSignal(_))
        ));
        assert!(store
            .principal_can_access(channel, &author.public_key())
            .expect("forgery ignored"));
    }

    #[test]
    fn outbox_schema_has_no_submitted_state() {
        let store = EdgeStore::open_in_memory(binding(), policy()).expect("store");
        let author = Keys::generate();
        let edge = Keys::generate();
        let channel = Uuid::new_v4();
        let event = message(&author, channel, "state check");
        store
            .insert_local_event(&event, event.as_json().as_bytes(), channel, &edge)
            .expect("insert event");
        let result = store.connection.lock().execute(
            "UPDATE outbox SET state = 'submitted' WHERE event_id = ?1",
            [event.id.to_hex()],
        );
        assert!(result.is_err());
    }

    #[test]
    fn reply_dependencies_are_persisted_globally() {
        let store = EdgeStore::open_in_memory(binding(), policy()).expect("store");
        let author = Keys::generate();
        let edge = Keys::generate();
        let channel = Uuid::new_v4();
        let root = message(&author, channel, "root");
        let reply = EventBuilder::new(Kind::Custom(9), "reply")
            .tags([
                Tag::parse(["h", channel.to_string().as_str()]).expect("h tag"),
                Tag::parse(["e", root.id.to_hex().as_str(), "", "reply"]).expect("reply tag"),
            ])
            .sign_with_keys(&Keys::generate())
            .expect("sign reply");
        store
            .insert_local_event(&reply, reply.as_json().as_bytes(), channel, &edge)
            .expect("insert reply");
        assert_eq!(
            store.dependencies(&reply.id).expect("dependencies"),
            vec![root.id]
        );
    }
}
