//! Delivery-state, quarantine, and waiting-for-author read surfaces (§11, §13).
//!
//! These are the queries behind the Desktop status UI. They are read-only
//! except for [`EdgeStore::requeue_quarantined`], which is the one operator
//! action the quarantine list offers (§11, "manual retry from the quarantine
//! UI").
//!
//! This module lives beside `storage.rs` rather than inside it because
//! `storage.rs` is already far past the repository's 1000-line file budget and
//! files over the budget may not grow.

use nostr::{EventId, PublicKey};
use rusqlite::{params, params_from_iter, types::Value as SqlValue};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{EdgeStore, StorageError};

/// Upper bound on one quarantine page.
///
/// The quarantine list is an operator triage surface, not an export. Capping
/// the page keeps a corrupt or hostile `limit` from pulling the whole table
/// into memory behind the storage mutex.
pub const MAX_QUARANTINE_PAGE: usize = 500;

/// How many event IDs go into one `IN (...)` list.
///
/// SQLite has a hard per-statement host-parameter limit (999 on older builds),
/// so a message list asking about a full screen of events must be chunked
/// rather than trusted to fit.
const DELIVERY_STATE_CHUNK: usize = 500;

/// Stand-in reason for a quarantined row whose `last_error` is null.
///
/// The quarantine UI must never render an empty reason: a blank cell reads as
/// "nothing is wrong here", which is the opposite of what quarantine means.
const UNRECORDED_QUARANTINE_REASON: &str = "upstream rejection reason was not recorded";

/// Where one event stands between "delivered locally" and "synced to
/// canonical history" — the two states the spec requires be labelled
/// separately (§13).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EventDeliveryState {
    /// In the local store, queued for an author to push upstream.
    Pending,
    /// Leased to an author right now.
    Claimed,
    /// In canonical history under its own event ID.
    SyncedExact,
    /// Represented upstream by an edge-authored digest instead.
    SyncedViaDigest,
    /// Upstream refused it permanently.
    Quarantined,
}

impl EventDeliveryState {
    /// Map one `(state, delivery_path)` outbox pair onto a label.
    ///
    /// `delivered` is the only state the delivery path splits, because it is
    /// the only state where "which history holds this event" differs (§13).
    fn from_row(state: &str, delivery_path: &str) -> Option<Self> {
        match (state, delivery_path) {
            ("pending", _) => Some(Self::Pending),
            ("claimed", _) => Some(Self::Claimed),
            ("delivered", "digest") => Some(Self::SyncedViaDigest),
            ("delivered", _) => Some(Self::SyncedExact),
            ("quarantined", _) => Some(Self::Quarantined),
            _ => None,
        }
    }
}

/// One permanently refused outbox row, as the operator's quarantine list shows it.
///
/// This row deliberately carries **no message content**. The quarantine list is
/// community-wide metadata shown to whoever is operating the edge, but the rows
/// in it belong to every local identity, not just the viewer's. Putting another
/// identity's message body in an operator list would leak a private message
/// through a status surface — the same boundary that stops the drain protocol
/// from letting one author touch another author's rows. The event ID is enough
/// to open the message through the normal, permission-checked message path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuarantinedRow {
    /// Identifies the refused event; the retry action keys on this.
    pub event_id: EventId,
    /// Channel the event was authored in.
    pub channel_id: Uuid,
    /// Identity that authored it, and the only identity allowed to requeue it.
    pub author: PublicKey,
    /// The event's own `created_at` in Unix seconds — when it was written, not
    /// when it was refused.
    pub created_at: i64,
    /// How many drain attempts this row has already cost. Never reset, so this
    /// is the operator's evidence that an event keeps failing.
    pub attempts: u32,
    /// Why upstream refused it. Never empty; see [`UNRECORDED_QUARANTINE_REASON`].
    pub reason: String,
    /// When the row last changed state, in Unix seconds.
    pub updated_at: i64,
}

/// One identity the edge is waiting on, for the "waiting for author" surface (§11).
///
/// The sidecar cannot submit another identity's events upstream, so when an
/// author's process is absent its rows simply sit in the outbox. This is that
/// condition made visible: an identity with drainable rows and no live claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitingAuthor {
    /// The identity that has to come back and drain.
    pub author: PublicKey,
    /// How many of its rows are waiting.
    pub pending: u64,
    /// Local arrival time of the oldest waiting row, in Unix seconds. This is
    /// how long the author has kept the edge waiting.
    pub oldest_pending_at: i64,
}

impl EdgeStore {
    /// Load the newest quarantined rows for the operator's triage list (§11).
    ///
    /// Newest-updated first, with `event_id` breaking ties so repeated calls
    /// return a stable order instead of shuffling rows under the operator's
    /// cursor. `limit` is clamped to [`MAX_QUARANTINE_PAGE`].
    pub fn quarantined_rows(&self, limit: usize) -> Result<Vec<QuarantinedRow>, StorageError> {
        let capped = limit.min(MAX_QUARANTINE_PAGE);
        if capped == 0 {
            return Ok(Vec::new());
        }
        let connection = self.connection.lock();
        let mut statement = connection.prepare(
            "SELECT o.event_id, e.channel_id, e.author_pubkey, e.created_at,
                    o.attempts, o.last_error, o.updated_at
               FROM outbox o
               JOIN events e ON e.event_id = o.event_id
              WHERE o.state = 'quarantined'
              ORDER BY o.updated_at DESC, o.event_id ASC
              LIMIT ?1",
        )?;
        let rows = statement.query_map([capped as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, i64>(6)?,
            ))
        })?;

        let mut quarantined = Vec::new();
        for row in rows {
            let (event_id, channel_id, author, created_at, attempts, last_error, updated_at) = row?;
            let reason = last_error
                .filter(|reason| !reason.trim().is_empty())
                .unwrap_or_else(|| UNRECORDED_QUARANTINE_REASON.to_string());
            quarantined.push(QuarantinedRow {
                event_id: EventId::from_hex(&event_id)
                    .map_err(|error| StorageError::Corrupt(error.to_string()))?,
                channel_id: Uuid::parse_str(&channel_id)
                    .map_err(|error| StorageError::Corrupt(error.to_string()))?,
                author: PublicKey::from_hex(&author)
                    .map_err(|error| StorageError::Corrupt(error.to_string()))?,
                created_at,
                attempts: u32::try_from(attempts)
                    .map_err(|error| StorageError::Corrupt(error.to_string()))?,
                reason,
                updated_at,
            });
        }
        Ok(quarantined)
    }

    /// Identities with drainable rows and nobody currently draining them (§11).
    ///
    /// An expired lease counts as waiting: it means the author claimed a batch
    /// and then went away mid-drain, which is exactly the absent-author
    /// condition Desktop has to show. `expire_outbox_leases` will eventually
    /// return those rows to `pending`, but the operator should not have to wait
    /// for a drain cycle before the UI tells the truth.
    ///
    /// Rows already demoted to the digest path are excluded: no author will
    /// ever claim them, because the edge identity carries them upstream instead
    /// (§12). Listing them here would ask the operator to chase an author who
    /// has nothing to do.
    ///
    /// Sorted oldest-waiting first, then by author, so the order is stable.
    pub fn waiting_authors(&self, now: i64) -> Result<Vec<WaitingAuthor>, StorageError> {
        let connection = self.connection.lock();
        let mut statement = connection.prepare(
            "SELECT e.author_pubkey, COUNT(*), MIN(e.received_at)
               FROM outbox o
               JOIN events e ON e.event_id = o.event_id
              WHERE o.delivery_path = 'exact'
                AND (
                    o.state = 'pending'
                    OR (
                        o.state = 'claimed'
                        AND o.lease_expires_at IS NOT NULL
                        AND o.lease_expires_at <= ?1
                    )
                )
              GROUP BY e.author_pubkey
              ORDER BY 3 ASC, 1 ASC",
        )?;
        let rows = statement.query_map([now], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;

        let mut waiting = Vec::new();
        for row in rows {
            let (author, pending, oldest_pending_at) = row?;
            waiting.push(WaitingAuthor {
                author: PublicKey::from_hex(&author)
                    .map_err(|error| StorageError::Corrupt(error.to_string()))?,
                pending: pending as u64,
                oldest_pending_at,
            });
        }
        Ok(waiting)
    }

    /// Look up delivery labels for a batch of events, for the message list (§13).
    ///
    /// Event IDs with no outbox row are simply **absent** from the result rather
    /// than reported as unknown: an event with no outbox row was mirrored down
    /// from canonical history, so it was never something this edge had to push
    /// upstream and has no local delivery state to label.
    ///
    /// The `IN` list is chunked so a long message list cannot exceed SQLite's
    /// per-statement host-parameter limit. Results come back in the caller's
    /// order, first occurrence wins, so a repeated ID cannot duplicate a row.
    pub fn event_delivery_states(
        &self,
        event_ids: &[EventId],
    ) -> Result<Vec<(EventId, EventDeliveryState)>, StorageError> {
        if event_ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut found = std::collections::HashMap::new();
        let connection = self.connection.lock();
        for chunk in event_ids.chunks(DELIVERY_STATE_CHUNK) {
            let placeholders = (1..=chunk.len())
                .map(|index| format!("?{index}"))
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "SELECT event_id, state, delivery_path
                   FROM outbox
                  WHERE event_id IN ({placeholders})"
            );
            let parameters = chunk
                .iter()
                .map(|event_id| SqlValue::Text(event_id.to_hex()))
                .collect::<Vec<_>>();
            let mut statement = connection.prepare(&sql)?;
            let rows = statement.query_map(params_from_iter(parameters), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?;
            for row in rows {
                let (event_id, state, delivery_path) = row?;
                let label =
                    EventDeliveryState::from_row(&state, &delivery_path).ok_or_else(|| {
                        StorageError::Corrupt(format!("unknown outbox state: '{state}'"))
                    })?;
                found.insert(event_id, label);
            }
        }
        drop(connection);

        let mut states = Vec::with_capacity(found.len());
        let mut emitted = std::collections::HashSet::new();
        for event_id in event_ids {
            let hex = event_id.to_hex();
            let Some(label) = found.get(&hex) else {
                continue;
            };
            if emitted.insert(hex) {
                states.push((*event_id, *label));
            }
        }
        Ok(states)
    }

    /// Return one quarantined row to `pending` on the operator's manual retry (§11).
    ///
    /// **Ownership is enforced here, not by the caller.** The row must be owned
    /// by `author`, for the same reason `claim_outbox_batch` refuses
    /// cross-identity claims: only the authoring identity can submit its own
    /// event upstream, so letting one identity requeue another's would queue
    /// work nobody is able to drain — and would let any local identity act on
    /// another's messages. A row owned by someone else is left completely
    /// untouched and the call reports `Ok(false)`.
    ///
    /// `attempts` is deliberately **not** reset. The attempt count is the
    /// operator's evidence that this event keeps failing; zeroing it on every
    /// retry would hide a permanent rejection behind a fresh-looking row.
    ///
    /// A row already demoted to the digest path keeps that path: requeueing
    /// undoes the quarantine, not the mixed-age thread decision of §11.
    ///
    /// Returns `Ok(true)` when a row moved and `Ok(false)` when no matching
    /// quarantined row existed.
    pub fn requeue_quarantined(
        &self,
        event_id: &EventId,
        author: &PublicKey,
        now: i64,
    ) -> Result<bool, StorageError> {
        let changed = self.connection.lock().execute(
            "UPDATE outbox
                SET state = 'pending',
                    claim_token = NULL,
                    lease_owner_pubkey = NULL,
                    lease_expires_at = NULL,
                    last_error = NULL,
                    updated_at = ?3
              WHERE event_id = ?1
                AND state = 'quarantined'
                AND event_id IN (
                    SELECT event_id FROM events WHERE author_pubkey = ?2
                )",
            params![event_id.to_hex(), author.to_hex(), now],
        )?;
        Ok(changed > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{AuthorizationPolicy, CommunityBinding, DrainOutcome};
    use nostr::{EventBuilder, JsonUtil, Keys, Kind, Tag};
    use std::time::Duration;

    fn binding() -> CommunityBinding {
        CommunityBinding::new("wss://relay.example.com", Uuid::new_v4()).expect("binding")
    }

    fn policy() -> AuthorizationPolicy {
        AuthorizationPolicy::new(Duration::from_secs(72 * 60 * 60)).expect("policy")
    }

    fn store() -> EdgeStore {
        EdgeStore::open_in_memory(binding(), policy()).expect("store")
    }

    fn message(keys: &Keys, channel: Uuid, content: &str) -> nostr::Event {
        EventBuilder::new(Kind::Custom(9), content)
            .tags([Tag::parse(["h", channel.to_string().as_str()]).expect("h tag")])
            .sign_with_keys(keys)
            .expect("sign")
    }

    fn insert(store: &EdgeStore, event: &nostr::Event, channel: Uuid, edge: &Keys) {
        store
            .insert_local_event(event, event.as_json().as_bytes(), channel, edge)
            .expect("insert");
    }

    /// Drive one row straight into `quarantined` through the real state machine.
    fn quarantine(store: &EdgeStore, author: &Keys, event_id: &EventId, reason: &str) {
        let token = format!("token-{}", event_id.to_hex());
        let claimed = store
            .claim_outbox_batch(&author.public_key(), &token, 100, 1_000, 60)
            .expect("claim");
        assert!(
            claimed.iter().any(|row| row.event_id == *event_id),
            "row must be claimable before it can be quarantined"
        );
        assert!(store
            .acknowledge_outbox_row(&token, event_id, DrainOutcome::Rejected(reason.to_string()))
            .expect("ack"));
        // The claim above swept up every drainable row; release the ones this
        // helper did not mean to touch so it changes exactly one row's state.
        store.expire_outbox_leases(1_061).expect("release the rest");
    }

    /// Force a deterministic `updated_at`; the real path stamps wall-clock time.
    fn set_updated_at(store: &EdgeStore, event_id: &EventId, updated_at: i64) {
        store
            .connection
            .lock()
            .execute(
                "UPDATE outbox SET updated_at = ?2 WHERE event_id = ?1",
                params![event_id.to_hex(), updated_at],
            )
            .expect("set updated_at");
    }

    fn outbox_state(store: &EdgeStore, event_id: &EventId) -> String {
        store
            .connection
            .lock()
            .query_row(
                "SELECT state FROM outbox WHERE event_id = ?1",
                [event_id.to_hex()],
                |row| row.get(0),
            )
            .expect("state")
    }

    fn outbox_attempts(store: &EdgeStore, event_id: &EventId) -> i64 {
        store
            .connection
            .lock()
            .query_row(
                "SELECT attempts FROM outbox WHERE event_id = ?1",
                [event_id.to_hex()],
                |row| row.get(0),
            )
            .expect("attempts")
    }

    fn last_error(store: &EdgeStore, event_id: &EventId) -> Option<String> {
        store
            .connection
            .lock()
            .query_row(
                "SELECT last_error FROM outbox WHERE event_id = ?1",
                [event_id.to_hex()],
                |row| row.get(0),
            )
            .expect("last_error")
    }

    #[test]
    fn quarantined_rows_are_newest_first_with_a_stable_tie_break() {
        // The operator triages the newest failures first, and repeated polls
        // must not reshuffle rows under the cursor.
        let store = store();
        let (author, edge) = (Keys::generate(), Keys::generate());
        let channel = Uuid::new_v4();
        let old = message(&author, channel, "old");
        let mid = message(&author, channel, "mid");
        let new = message(&author, channel, "new");
        for event in [&old, &mid, &new] {
            insert(&store, event, channel, &edge);
            quarantine(&store, &author, &event.id, "refused");
        }
        set_updated_at(&store, &old.id, 100);
        set_updated_at(&store, &mid.id, 200);
        set_updated_at(&store, &new.id, 200);

        let rows = store.quarantined_rows(10).expect("rows");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[2].event_id, old.id, "oldest update sorts last");

        // The 200-second tie breaks on event ID, ascending.
        let mut tied = [mid.id, new.id];
        tied.sort_by_key(|id| id.to_hex());
        assert_eq!([rows[0].event_id, rows[1].event_id], tied);
        assert_eq!(rows[0].channel_id, channel);
        assert_eq!(rows[0].author, author.public_key());
    }

    #[test]
    fn quarantined_rows_clamps_the_page_to_the_maximum() {
        let store = store();
        let (author, edge) = (Keys::generate(), Keys::generate());
        let channel = Uuid::new_v4();
        for index in 0..3 {
            let event = message(&author, channel, &format!("refused {index}"));
            insert(&store, &event, channel, &edge);
            quarantine(&store, &author, &event.id, "refused");
        }

        assert_eq!(MAX_QUARANTINE_PAGE, 500);
        assert_eq!(store.quarantined_rows(1).expect("one").len(), 1);
        assert!(store.quarantined_rows(0).expect("zero").is_empty());
        // A caller asking for everything must be clamped, not allowed to pull
        // the whole table in behind the storage mutex.
        assert_eq!(store.quarantined_rows(usize::MAX).expect("all").len(), 3);
    }

    #[test]
    fn a_quarantined_row_without_a_recorded_error_still_has_a_reason() {
        // A blank reason in the quarantine UI reads as "nothing is wrong",
        // which is the opposite of what the row means.
        let store = store();
        let (author, edge) = (Keys::generate(), Keys::generate());
        let channel = Uuid::new_v4();
        let event = message(&author, channel, "no reason");
        insert(&store, &event, channel, &edge);
        quarantine(&store, &author, &event.id, "refused");
        store
            .connection
            .lock()
            .execute(
                "UPDATE outbox SET last_error = NULL WHERE event_id = ?1",
                [event.id.to_hex()],
            )
            .expect("clear last_error");

        let rows = store.quarantined_rows(10).expect("rows");
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].reason.trim().is_empty());
        assert_eq!(rows[0].reason, UNRECORDED_QUARANTINE_REASON);
    }

    #[test]
    fn waiting_authors_includes_an_expired_lease_and_excludes_a_live_one() {
        // An expired lease means the author went away mid-drain — the absent
        // author condition Desktop has to show, without waiting for the next
        // drain cycle to sweep the row back to `pending`.
        let store = store();
        let (gone, present, edge) = (Keys::generate(), Keys::generate(), Keys::generate());
        let channel = Uuid::new_v4();
        let abandoned = message(&gone, channel, "abandoned");
        let in_flight = message(&present, channel, "in flight");
        insert(&store, &abandoned, channel, &edge);
        insert(&store, &in_flight, channel, &edge);

        // Both claim at t=1000 with a 60-second lease.
        store
            .claim_outbox_batch(&gone.public_key(), "gone", 10, 1_000, 60)
            .expect("claim gone");
        store
            .claim_outbox_batch(&present.public_key(), "present", 10, 1_000, 60)
            .expect("claim present");
        // The present author renews; the vanished one does not.
        store
            .renew_outbox_lease("present", 1_050, 600)
            .expect("renew");

        let waiting = store.waiting_authors(1_100).expect("waiting");
        assert_eq!(waiting.len(), 1, "only the vanished author is waiting");
        assert_eq!(waiting[0].author, gone.public_key());
        assert_eq!(waiting[0].pending, 1);
    }

    #[test]
    fn waiting_authors_counts_pending_rows_oldest_first() {
        let store = store();
        let (early, late, edge) = (Keys::generate(), Keys::generate(), Keys::generate());
        let channel = Uuid::new_v4();
        let first = message(&early, channel, "first");
        let second = message(&early, channel, "second");
        let third = message(&late, channel, "third");
        for event in [&first, &second, &third] {
            insert(&store, event, channel, &edge);
        }
        // `received_at` is wall-clock on insert, so pin it for determinism.
        let connection = store.connection.lock();
        for (event, received_at) in [(&first, 100), (&second, 400), (&third, 300)] {
            connection
                .execute(
                    "UPDATE events SET received_at = ?2 WHERE event_id = ?1",
                    params![event.id.to_hex(), received_at],
                )
                .expect("pin received_at");
        }
        drop(connection);

        let waiting = store.waiting_authors(9_999).expect("waiting");
        assert_eq!(waiting.len(), 2);
        assert_eq!(waiting[0].author, early.public_key());
        assert_eq!(waiting[0].pending, 2);
        assert_eq!(waiting[0].oldest_pending_at, 100);
        assert_eq!(waiting[1].author, late.public_key());
        assert_eq!(waiting[1].oldest_pending_at, 300);
    }

    #[test]
    fn event_delivery_states_maps_every_state() {
        let store = store();
        let (author, edge) = (Keys::generate(), Keys::generate());
        let channel = Uuid::new_v4();
        let pending = message(&author, channel, "pending");
        let claimed = message(&author, channel, "claimed");
        let exact = message(&author, channel, "exact");
        let digest = message(&author, channel, "digest");
        let refused = message(&author, channel, "refused");
        for event in [&pending, &claimed, &exact, &digest, &refused] {
            insert(&store, event, channel, &edge);
        }

        quarantine(&store, &author, &refused.id, "refused upstream");
        let batch = store
            .claim_outbox_batch(&author.public_key(), "t", 10, 1_000, 600)
            .expect("claim");
        assert_eq!(batch.len(), 4, "the quarantined row is no longer claimable");
        store
            .acknowledge_outbox_row("t", &exact.id, DrainOutcome::Delivered)
            .expect("ack exact");
        store
            .acknowledge_outbox_row("t", &digest.id, DrainOutcome::Duplicate)
            .expect("ack digest");
        store
            .connection
            .lock()
            .execute(
                "UPDATE outbox SET delivery_path = 'digest' WHERE event_id = ?1",
                [digest.id.to_hex()],
            )
            .expect("demote to digest");
        // Return one row to `pending` so both live states are represented.
        store
            .connection
            .lock()
            .execute(
                "UPDATE outbox
                    SET state = 'pending', claim_token = NULL,
                        lease_owner_pubkey = NULL, lease_expires_at = NULL
                  WHERE event_id = ?1",
                [pending.id.to_hex()],
            )
            .expect("reset to pending");

        let unknown = EventId::from_hex(&"ab".repeat(32)).expect("unknown id");
        let states = store
            .event_delivery_states(&[
                pending.id, claimed.id, exact.id, digest.id, refused.id, unknown,
            ])
            .expect("states");
        assert_eq!(
            states,
            vec![
                (pending.id, EventDeliveryState::Pending),
                (claimed.id, EventDeliveryState::Claimed),
                (exact.id, EventDeliveryState::SyncedExact),
                (digest.id, EventDeliveryState::SyncedViaDigest),
                (refused.id, EventDeliveryState::Quarantined),
            ],
            "an upstream-sourced ID is absent, not reported as unknown"
        );
    }

    #[test]
    fn event_delivery_states_survives_more_ids_than_one_sql_chunk() {
        // SQLite caps host parameters per statement, so a long message list
        // must be chunked rather than trusted to fit.
        let store = store();
        let (author, edge) = (Keys::generate(), Keys::generate());
        let channel = Uuid::new_v4();
        let event = message(&author, channel, "the only real row");
        insert(&store, &event, channel, &edge);

        let mut ids: Vec<EventId> = (0..(DELIVERY_STATE_CHUNK * 2 + 7))
            .map(|index| EventId::from_hex(&format!("{index:064x}")).expect("synthetic id"))
            .collect();
        ids.push(event.id);
        assert!(ids.len() > DELIVERY_STATE_CHUNK);

        let states = store.event_delivery_states(&ids).expect("states");
        assert_eq!(states, vec![(event.id, EventDeliveryState::Pending)]);
    }

    #[test]
    fn event_delivery_states_is_empty_for_an_empty_input() {
        assert!(store()
            .event_delivery_states(&[])
            .expect("states")
            .is_empty());
    }

    #[test]
    fn requeue_quarantined_refuses_another_authors_row() {
        // One local identity must never be able to act on another's messages —
        // the same boundary that stops `claim_outbox_batch` from crossing
        // identities.
        let store = store();
        let (owner, stranger, edge) = (Keys::generate(), Keys::generate(), Keys::generate());
        let channel = Uuid::new_v4();
        let event = message(&owner, channel, "not yours");
        insert(&store, &event, channel, &edge);
        quarantine(&store, &owner, &event.id, "refused upstream");

        assert!(!store
            .requeue_quarantined(&event.id, &stranger.public_key(), 2_000)
            .expect("requeue"));
        // The return value is not the whole guarantee: the row must be intact.
        assert_eq!(outbox_state(&store, &event.id), "quarantined");
        assert_eq!(
            last_error(&store, &event.id).as_deref(),
            Some("refused upstream")
        );
        assert_eq!(store.quarantined_rows(10).expect("rows").len(), 1);
    }

    #[test]
    fn requeue_quarantined_only_moves_a_quarantined_row() {
        let store = store();
        let (author, edge) = (Keys::generate(), Keys::generate());
        let channel = Uuid::new_v4();
        let still_pending = message(&author, channel, "pending");
        let delivered = message(&author, channel, "delivered");
        insert(&store, &still_pending, channel, &edge);
        insert(&store, &delivered, channel, &edge);
        let claimed = store
            .claim_outbox_batch(&author.public_key(), "t", 10, 1_000, 600)
            .expect("claim");
        assert_eq!(claimed.len(), 2);
        store
            .acknowledge_outbox_row("t", &delivered.id, DrainOutcome::Delivered)
            .expect("ack");
        store.expire_outbox_leases(9_999).expect("expire");

        assert!(!store
            .requeue_quarantined(&still_pending.id, &author.public_key(), 2_000)
            .expect("requeue pending"));
        assert_eq!(outbox_state(&store, &still_pending.id), "pending");

        assert!(!store
            .requeue_quarantined(&delivered.id, &author.public_key(), 2_000)
            .expect("requeue delivered"));
        assert_eq!(
            outbox_state(&store, &delivered.id),
            "delivered",
            "a synced event must never be dragged back into the outbox"
        );
    }

    #[test]
    fn requeue_quarantined_preserves_the_attempt_history() {
        // Attempts are the operator's evidence that an event keeps failing.
        // Zeroing them on retry would hide a permanent rejection.
        let store = store();
        let (author, edge) = (Keys::generate(), Keys::generate());
        let channel = Uuid::new_v4();
        let event = message(&author, channel, "keeps failing");
        insert(&store, &event, channel, &edge);
        quarantine(&store, &author, &event.id, "refused once");
        let before = outbox_attempts(&store, &event.id);
        assert_eq!(before, 1);

        assert!(store
            .requeue_quarantined(&event.id, &author.public_key(), 2_000)
            .expect("requeue"));
        assert_eq!(outbox_attempts(&store, &event.id), before);
        assert_eq!(outbox_state(&store, &event.id), "pending");
        assert!(last_error(&store, &event.id).is_none());
        assert!(store.quarantined_rows(10).expect("rows").is_empty());
    }

    #[test]
    fn a_requeued_row_is_claimable_again() {
        // The point of the manual retry is a real second attempt, so the row
        // has to come back through the ordinary drain path.
        let store = store();
        let (author, edge) = (Keys::generate(), Keys::generate());
        let channel = Uuid::new_v4();
        let event = message(&author, channel, "retry me");
        insert(&store, &event, channel, &edge);
        quarantine(&store, &author, &event.id, "transient-looking rejection");
        assert!(store
            .claim_outbox_batch(&author.public_key(), "blocked", 10, 2_000, 60)
            .expect("claim")
            .is_empty());

        assert!(store
            .requeue_quarantined(&event.id, &author.public_key(), 2_000)
            .expect("requeue"));
        let claimed = store
            .claim_outbox_batch(&author.public_key(), "retry", 10, 2_100, 60)
            .expect("claim");
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].event_id, event.id);
        assert_eq!(
            outbox_attempts(&store, &event.id),
            2,
            "the retry adds to the attempt history rather than resetting it"
        );
    }

    #[test]
    fn requeue_quarantined_reports_false_for_an_unknown_event() {
        let store = store();
        let unknown = EventId::from_hex(&"cd".repeat(32)).expect("unknown id");
        let result = store.requeue_quarantined(&unknown, &Keys::generate().public_key(), 1_000);
        assert!(matches!(result, Ok(false)), "no row, no error");
    }

    #[test]
    fn delivery_state_labels_serialize_in_camel_case() {
        // The Desktop status surface reads these labels verbatim (§13).
        let json = serde_json::to_string(&EventDeliveryState::SyncedViaDigest).expect("serialize");
        assert_eq!(json, "\"syncedViaDigest\"");
        let round_trip: EventDeliveryState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round_trip, EventDeliveryState::SyncedViaDigest);
    }

    #[test]
    fn an_unrecognised_outbox_state_has_no_label() {
        // The schema CHECK constraint keeps an unknown state out of the table,
        // but the mapping still fails closed rather than guessing "synced" —
        // mislabelling an unsynced event as synced is the worst outcome here.
        assert!(EventDeliveryState::from_row("submitted", "exact").is_none());
        assert_eq!(
            EventDeliveryState::from_row("delivered", "exact"),
            Some(EventDeliveryState::SyncedExact)
        );
    }
}
