//! Delivery-state, quarantine, and waiting-for-author read surfaces (§11, §13).
//!
//! These are the queries behind the Desktop status UI. They are read-only
//! except for [`EdgeStore::requeue_quarantined`], which is the one operator
//! action the quarantine list offers (§11, "manual retry from the quarantine
//! UI").
//!
//! **Every read here is scoped to the channels the calling principal may
//! access.** The status surface is deliberately not filtered to the *author* —
//! the operator has to be able to see that an agent's events are stuck — but
//! "not filtered by author" is not "not filtered at all". Each read runs the
//! caller's channel set through exactly the predicate `REQ` uses
//! ([`EdgeStore::principal_can_access_at`]), so an identity scoped to one
//! channel learns nothing about a channel it is not a member of: not the
//! channel ID, not who posts there, not when, not why a post failed.
//!
//! This module lives beside `storage.rs` rather than inside it because
//! `storage.rs` is already far past the repository's 1000-line file budget and
//! files over the budget may not grow.

use std::collections::{HashMap, HashSet};

use nostr::{EventId, PublicKey};
use rusqlite::{params, params_from_iter, types::Value as SqlValue, OptionalExtension};
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

/// How many channel IDs go into one `IN (...)` list.
///
/// Same host-parameter ceiling as [`DELIVERY_STATE_CHUNK`], with headroom for
/// the query's own bound parameters. A community with more accessible channels
/// than this is queried in several passes and the results merged, rather than
/// having the channel set silently truncated — a truncated scope would hide
/// real rows from the operator.
const CHANNEL_FILTER_CHUNK: usize = 400;

/// Longest failure reason the edge will ever store or surface, in bytes.
///
/// The reason arrives as free text from a drain client and is then shown to
/// *every* local identity's operator list, so it is both a content-leak channel
/// and a memory amplifier: one 1.6 MB reason per row would make a single status
/// page hundreds of megabytes. 512 bytes is far more than any real upstream
/// rejection needs and far less than anything that can be abused.
pub const MAX_QUARANTINE_REASON_BYTES: usize = 512;

/// Marker appended to a reason that was cut short, so the operator can tell a
/// truncated message from a naturally short one.
const REASON_ELLIPSIS: &str = "…";

/// Stand-in reason for a quarantined row whose `last_error` is null.
///
/// The quarantine UI must never render an empty reason: a blank cell reads as
/// "nothing is wrong here", which is the opposite of what quarantine means.
const UNRECORDED_QUARANTINE_REASON: &str = "upstream rejection reason was not recorded";

/// Clamp one failure reason to [`MAX_QUARANTINE_REASON_BYTES`].
///
/// Truncation, never rejection: a refused DRAIN-ACK would leave the row leased
/// and the author with nothing to do but retry the same oversized ack forever,
/// which strands the row instead of bounding it.
///
/// The cut walks back to a UTF-8 character boundary, so a multi-byte character
/// straddling the limit is dropped whole rather than split into invalid bytes.
/// The returned string is never longer than the cap, ellipsis included.
pub fn truncate_quarantine_reason(reason: &str) -> String {
    if reason.len() <= MAX_QUARANTINE_REASON_BYTES {
        return reason.to_string();
    }
    let mut cut = MAX_QUARANTINE_REASON_BYTES - REASON_ELLIPSIS.len();
    while cut > 0 && !reason.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}{REASON_ELLIPSIS}", &reason[..cut])
}

/// Where one event stands between "delivered locally" and "synced to
/// canonical history" — the two states the spec requires be labelled
/// separately (§13).
///
/// The `exact` and `digest` delivery paths stay apart in **every** live state,
/// not only the terminal one. A row on the digest path is waiting for the edge
/// identity to carry it upstream inside a catch-up digest; a row on the exact
/// path is waiting for its own author to push it. Collapsing those into one
/// "pending" leaves the operator with a count nothing on screen explains.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EventDeliveryState {
    /// In the local store, queued for its author to push upstream.
    Pending,
    /// In the local store, demoted to the digest path: the edge identity will
    /// carry it upstream inside a catch-up digest, not its author (§12).
    PendingViaDigest,
    /// Leased to an author right now.
    Claimed,
    /// In canonical history under its own event ID.
    SyncedExact,
    /// Represented upstream by an edge-authored digest instead.
    SyncedViaDigest,
    /// Upstream refused it permanently.
    Quarantined,
}

/// The `delivery_path` value that means "the edge identity carries this row
/// upstream inside a catch-up digest, not its author".
///
/// Named once because three surfaces test for it — the quarantine list, the
/// per-event labels, and the requeue refusal — and they must agree on the
/// spelling or one of them silently stops recognising the digest path.
pub(crate) const DIGEST_DELIVERY_PATH: &str = "digest";

impl EventDeliveryState {
    /// Map one `(state, delivery_path)` outbox pair onto a label.
    ///
    /// **The quarantined arm deliberately ignores the path**, and that is not
    /// the old bug returning: the path now travels beside the label on
    /// [`EventDeliveryRow::carried_by_digest`] instead of being folded into it.
    /// See that field for why it is a flag rather than a seventh variant.
    fn from_row(state: &str, delivery_path: &str) -> Option<Self> {
        match (state, delivery_path) {
            ("pending", DIGEST_DELIVERY_PATH) => Some(Self::PendingViaDigest),
            ("pending", _) => Some(Self::Pending),
            ("claimed", _) => Some(Self::Claimed),
            ("delivered", DIGEST_DELIVERY_PATH) => Some(Self::SyncedViaDigest),
            ("delivered", _) => Some(Self::SyncedExact),
            ("quarantined", _) => Some(Self::Quarantined),
            _ => None,
        }
    }
}

/// One event's delivery label plus the reason it left the exact path, if it did.
///
/// `demotion_reason` is the column the mixed-age thread policy already writes
/// ("older than the relay drift window", "permanently rejected upstream",
/// "ancestor cannot be replayed upstream"). Without it a demoted row is a
/// state with no explanation, which is exactly the row an operator asks about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventDeliveryRow {
    /// Which event this label describes.
    pub event_id: EventId,
    /// Where the event stands.
    pub state: EventDeliveryState,
    /// True when this row sits on the digest path, exactly as
    /// [`QuarantinedRow::carried_by_digest`] reports it (BUG-023).
    ///
    /// Without this the two surfaces answered differently about one row: the
    /// quarantine *list* could say a quarantined row was already being carried
    /// upstream by the edge identity — so its refused retry was correct and
    /// there was nothing to do — while the per-message *badge* for that same
    /// event could only say `Quarantined`, which reads as "stuck, act now".
    ///
    /// A **flag**, not a seventh `EventDeliveryState`, for the same reason
    /// `QuarantinedRow` uses one: a quarantined digest row really is
    /// quarantined — its own replay was permanently refused, which is the whole
    /// content of that state — plus one extra fact about who carries it now.
    /// `PendingViaDigest` by contrast is a genuinely different *state*, because
    /// no author will ever claim it and `Pending`'s advice ("wait for its
    /// author") would be wrong. Six wire strings are also pinned by tests on
    /// both the Rust and TypeScript sides; a seventh is a contract change and
    /// this fact does not need one.
    ///
    /// True on every digest-path row, not only the quarantined ones, so the
    /// meaning is the literal column and never a per-state special case:
    /// `carried_by_digest == (delivery_path == "digest")`, always.
    pub carried_by_digest: bool,
    /// Why it left the exact path, when it has.
    pub demotion_reason: Option<String>,
}

/// One permanently refused outbox row, as the operator's quarantine list shows it.
///
/// This row deliberately carries **no message content**. The quarantine list is
/// scoped to the channels the caller may read, but the rows in it belong to
/// every local identity in those channels, not just the viewer's. Putting
/// another identity's message body in an operator list would leak a private
/// message through a status surface — the same boundary that stops the drain
/// protocol from letting one author touch another author's rows. The event ID
/// is enough to open the message through the normal, permission-checked
/// message path.
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
    /// Why upstream refused it. Never empty, never longer than
    /// [`MAX_QUARANTINE_REASON_BYTES`]; see [`UNRECORDED_QUARANTINE_REASON`].
    pub reason: String,
    /// True when this row has already been demoted to the digest path.
    ///
    /// Such a row is *already being carried upstream* by the edge identity, so
    /// retrying it under its own ID is not possible. The operator must be told,
    /// or the retry button becomes a button that only hides the row.
    pub carried_by_digest: bool,
    /// Why it was demoted, when it was.
    pub demotion_reason: Option<String>,
    /// When the row last changed state, in Unix seconds.
    pub updated_at: i64,
}

/// One identity the edge is waiting on, for the "waiting for author" surface (§11).
///
/// The sidecar cannot submit another identity's events upstream, so when an
/// author's process is absent its rows simply sit in the outbox. This is that
/// condition made visible — split by *why* each row is stuck, because "waiting
/// for author" is only actionable when the author can actually act.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitingAuthor {
    /// The identity that has to come back and drain.
    pub author: PublicKey,
    /// Rows this author's drain client can claim right now.
    pub pending: u64,
    /// Rows blocked behind an ancestor that has not reached canonical history.
    ///
    /// `claim_outbox_batch` refuses these, so no amount of author uptime moves
    /// them: the operator has to retry or discard the ancestor. Counting them
    /// as plain "pending" told the operator to wait for an author who was
    /// already running and correctly claiming nothing.
    pub ancestor_blocked: u64,
    /// Rows demoted to the digest path, which the edge identity carries
    /// upstream instead. No author action is possible or needed; they are here
    /// so the pending total has an explanation on screen.
    pub pending_via_digest: u64,
    /// Local arrival time of the oldest unsynced row **across all three
    /// buckets**, in Unix seconds — how long this author has kept the edge
    /// waiting overall, and the sort key for the list.
    ///
    /// Deliberately NOT the age to print beside any one of the three counts:
    /// see [`WaitingAuthor::oldest_claimable_at`].
    pub oldest_pending_at: i64,
    /// Arrival time of the oldest row this author's drain client can claim
    /// **right now**, or `None` when `pending` is 0.
    ///
    /// The aggregate above cannot be used for this (BUG-023). It is one
    /// `MIN(received_at)` over pending, ancestor-blocked and digest rows
    /// together, so an author with one fresh claimable row and one week-old
    /// digest row was reported as "oldest waiting 7d" — an age no waiting row
    /// actually has. A per-bucket minimum is the only figure that matches the
    /// count it is printed beside.
    pub oldest_claimable_at: Option<i64>,
    /// Arrival time of the oldest row blocked behind an unreplayable ancestor,
    /// or `None` when `ancestor_blocked` is 0. Same reason as above, and it
    /// matters more here: this is the bucket whose entire point is that these
    /// rows are *not* waiting for anybody.
    pub oldest_ancestor_blocked_at: Option<i64>,
}

/// The earlier of two optional timestamps, ignoring the absent ones.
///
/// `None` means "this bucket had no rows in this chunk", which must never win a
/// minimum — an author's channels are queried in several passes when the
/// accessible set is large, and treating an empty pass as time zero would
/// report an age no row has.
fn earlier(current: Option<i64>, candidate: Option<i64>) -> Option<i64> {
    match (current, candidate) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(left), None) => Some(left),
        (None, right) => right,
    }
}

/// What a manual quarantine retry actually did.
///
/// A bare boolean could not distinguish "nothing to retry" from "this row is
/// already on its way upstream by another route", and the second answer is the
/// one the operator needs: pressing Retry on a digest-path row used to move it
/// out of the quarantine list without making it drainable, so the button's only
/// visible effect was to delete the operator's view of the problem.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RequeueOutcome {
    /// The row returned to `pending` and is claimable again.
    Requeued,
    /// No quarantined row with that ID is visible to and owned by this caller.
    NotFound,
    /// The row is on the digest path: the edge identity is already carrying it
    /// upstream, so there is nothing for its author to retry.
    CarriedByDigest,
}

impl RequeueOutcome {
    /// Whether the row actually moved. The wire keeps a boolean for the
    /// caller that only branches on success.
    pub fn requeued(self) -> bool {
        matches!(self, Self::Requeued)
    }
}

/// Build `?n, ?n+1, ...` for `count` parameters starting at `first`.
fn placeholders(first: usize, count: usize) -> String {
    (first..first + count)
        .map(|index| format!("?{index}"))
        .collect::<Vec<_>>()
        .join(", ")
}

impl EdgeStore {
    /// Channels with outbox rows that `principal` is allowed to see (§7).
    ///
    /// This is the channel set behind every status read. It runs each candidate
    /// through [`EdgeStore::principal_can_access_at`] — the *same* predicate
    /// `REQ` and `COUNT` use — rather than restating the membership SQL, so the
    /// status surface cannot drift away from the subscription surface and start
    /// showing rows a subscription would refuse.
    ///
    /// An expired authorization lease, a deselected channel, a channel that is
    /// no longer edge-eligible, or a removed member all make the channel
    /// disappear from this list, so a revoked principal fails closed mid-flight
    /// rather than at the next sweep.
    pub fn accessible_channels(
        &self,
        principal: &PublicKey,
        now: i64,
    ) -> Result<Vec<Uuid>, StorageError> {
        let candidates: Vec<Uuid> = {
            let connection = self.connection.lock();
            let mut statement = connection.prepare(
                "SELECT DISTINCT e.channel_id
                   FROM outbox o
                   JOIN events e ON e.event_id = o.event_id
                  ORDER BY 1",
            )?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            let mut candidates = Vec::new();
            for row in rows {
                candidates.push(
                    Uuid::parse_str(&row?)
                        .map_err(|error| StorageError::Corrupt(error.to_string()))?,
                );
            }
            candidates
        };

        let mut accessible = Vec::with_capacity(candidates.len());
        for channel in candidates {
            if self.principal_can_access_at(channel, principal, now)? {
                accessible.push(channel);
            }
        }
        Ok(accessible)
    }

    /// Load the newest quarantined rows for the operator's triage list (§11).
    ///
    /// Scoped to the channels `principal` may access; see the module docs.
    /// Newest-updated first, with `event_id` breaking ties so repeated calls
    /// return a stable order instead of shuffling rows under the operator's
    /// cursor. `limit` is clamped to [`MAX_QUARANTINE_PAGE`].
    ///
    /// Digest-path rows are listed but flagged `carried_by_digest`: they are
    /// already being carried upstream, so hiding them would lose the operator's
    /// only view of them while showing them unmarked invites a retry that
    /// cannot work.
    pub fn quarantined_rows(
        &self,
        principal: &PublicKey,
        now: i64,
        limit: usize,
    ) -> Result<Vec<QuarantinedRow>, StorageError> {
        let capped = limit.min(MAX_QUARANTINE_PAGE);
        if capped == 0 {
            return Ok(Vec::new());
        }
        let channels = self.accessible_channels(principal, now)?;
        if channels.is_empty() {
            return Ok(Vec::new());
        }

        let mut quarantined = Vec::new();
        let connection = self.connection.lock();
        for chunk in channels.chunks(CHANNEL_FILTER_CHUNK) {
            let sql = format!(
                "SELECT o.event_id, e.channel_id, e.author_pubkey, e.created_at,
                        o.attempts, o.last_error, o.delivery_path,
                        o.demotion_reason, o.updated_at
                   FROM outbox o
                   JOIN events e ON e.event_id = o.event_id
                  WHERE o.state = 'quarantined'
                    AND e.channel_id IN ({})
                  ORDER BY o.updated_at DESC, o.event_id ASC
                  LIMIT ?1",
                placeholders(2, chunk.len())
            );
            let mut parameters = vec![SqlValue::Integer(capped as i64)];
            parameters.extend(
                chunk
                    .iter()
                    .map(|channel| SqlValue::Text(channel.to_string())),
            );
            let mut statement = connection.prepare(&sql)?;
            let rows = statement.query_map(params_from_iter(parameters), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, i64>(8)?,
                ))
            })?;
            for row in rows {
                let (
                    event_id,
                    channel_id,
                    author,
                    created_at,
                    attempts,
                    last_error,
                    delivery_path,
                    demotion_reason,
                    updated_at,
                ) = row?;
                // Defensive: a row written by an older build was never capped
                // on the way in, so it is capped on the way out too.
                let reason = last_error
                    .filter(|reason| !reason.trim().is_empty())
                    .map(|reason| truncate_quarantine_reason(&reason))
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
                    carried_by_digest: delivery_path == DIGEST_DELIVERY_PATH,
                    demotion_reason: demotion_reason
                        .map(|reason| truncate_quarantine_reason(&reason)),
                    updated_at,
                });
            }
        }
        drop(connection);

        // Each chunk returned its own top `capped`; the global top `capped` is
        // contained in their union, so re-applying the SQL ordering here is
        // exact rather than approximate.
        quarantined.sort_by(|left, right| {
            right
                .updated_at
                .cmp(&left.updated_at)
                .then_with(|| left.event_id.to_hex().cmp(&right.event_id.to_hex()))
        });
        quarantined.truncate(capped);
        Ok(quarantined)
    }

    /// Identities with unsynced rows, split by what is actually blocking them (§11).
    ///
    /// Scoped to the channels `principal` may access; see the module docs.
    ///
    /// An expired lease counts as waiting: it means the author claimed a batch
    /// and then went away mid-drain, which is exactly the absent-author
    /// condition Desktop has to show. `expire_outbox_leases` will eventually
    /// return those rows to `pending`, but the operator should not have to wait
    /// for a drain cycle before the UI tells the truth.
    ///
    /// The three counts mirror the three reasons a row is not moving, and
    /// `pending` mirrors `claim_outbox_batch` exactly — including its
    /// dependency gate. A row whose ancestor has not reached canonical history
    /// is **not** claimable, so counting it as "waiting for author" told the
    /// operator to wait for an author whose drain client was running and
    /// correctly claiming nothing, forever.
    ///
    /// Each count comes back with **its own** oldest arrival time, because an
    /// age printed beside a count has to be an age some row in that count
    /// actually has. The aggregate `oldest_pending_at` spans all three buckets
    /// and remains the sort key.
    ///
    /// Sorted oldest-waiting first, then by author, so the order is stable.
    pub fn waiting_authors(
        &self,
        principal: &PublicKey,
        now: i64,
    ) -> Result<Vec<WaitingAuthor>, StorageError> {
        let channels = self.accessible_channels(principal, now)?;
        if channels.is_empty() {
            return Ok(Vec::new());
        }

        let mut merged: HashMap<String, WaitingAuthor> = HashMap::new();
        let connection = self.connection.lock();
        for chunk in channels.chunks(CHANNEL_FILTER_CHUNK) {
            let blocked = BLOCKED_BY_ANCESTOR;
            let sql = format!(
                "SELECT e.author_pubkey,
                        SUM(CASE WHEN o.delivery_path = 'exact' AND NOT {blocked}
                                 THEN 1 ELSE 0 END),
                        SUM(CASE WHEN o.delivery_path = 'exact' AND {blocked}
                                 THEN 1 ELSE 0 END),
                        SUM(CASE WHEN o.delivery_path = 'digest' THEN 1 ELSE 0 END),
                        MIN(e.received_at),
                        MIN(CASE WHEN o.delivery_path = 'exact' AND NOT {blocked}
                                 THEN e.received_at END),
                        MIN(CASE WHEN o.delivery_path = 'exact' AND {blocked}
                                 THEN e.received_at END)
                   FROM outbox o
                   JOIN events e ON e.event_id = o.event_id
                  WHERE e.channel_id IN ({channels})
                    AND (
                        o.state = 'pending'
                        OR (
                            o.state = 'claimed'
                            AND o.lease_expires_at IS NOT NULL
                            AND o.lease_expires_at <= ?1
                        )
                    )
                  GROUP BY e.author_pubkey",
                channels = placeholders(2, chunk.len())
            );
            let mut parameters = vec![SqlValue::Integer(now)];
            parameters.extend(
                chunk
                    .iter()
                    .map(|channel| SqlValue::Text(channel.to_string())),
            );
            let mut statement = connection.prepare(&sql)?;
            let rows = statement.query_map(params_from_iter(parameters), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, Option<i64>>(5)?,
                    row.get::<_, Option<i64>>(6)?,
                ))
            })?;
            for row in rows {
                let (
                    author,
                    pending,
                    ancestor_blocked,
                    via_digest,
                    oldest,
                    oldest_claimable,
                    oldest_blocked,
                ) = row?;
                let entry = merged.entry(author.clone()).or_insert(WaitingAuthor {
                    author: PublicKey::from_hex(&author)
                        .map_err(|error| StorageError::Corrupt(error.to_string()))?,
                    pending: 0,
                    ancestor_blocked: 0,
                    pending_via_digest: 0,
                    oldest_pending_at: oldest,
                    oldest_claimable_at: None,
                    oldest_ancestor_blocked_at: None,
                });
                entry.pending += pending as u64;
                entry.ancestor_blocked += ancestor_blocked as u64;
                entry.pending_via_digest += via_digest as u64;
                entry.oldest_pending_at = entry.oldest_pending_at.min(oldest);
                entry.oldest_claimable_at = earlier(entry.oldest_claimable_at, oldest_claimable);
                entry.oldest_ancestor_blocked_at =
                    earlier(entry.oldest_ancestor_blocked_at, oldest_blocked);
            }
        }
        drop(connection);

        let mut waiting: Vec<WaitingAuthor> = merged.into_values().collect();
        waiting.sort_by(|left, right| {
            left.oldest_pending_at
                .cmp(&right.oldest_pending_at)
                .then_with(|| left.author.to_hex().cmp(&right.author.to_hex()))
        });
        Ok(waiting)
    }

    /// Look up delivery labels for a batch of events, for the message list (§13).
    ///
    /// Scoped to the channels `principal` may access; see the module docs. An
    /// event in a channel the caller cannot read is absent from the reply, for
    /// the same reason a `REQ` for it would be refused.
    ///
    /// Event IDs with no outbox row are likewise **absent** rather than
    /// reported as unknown: an event with no outbox row was mirrored down from
    /// canonical history, so it was never something this edge had to push
    /// upstream and has no local delivery state to label.
    ///
    /// The `IN` list is chunked so a long message list cannot exceed SQLite's
    /// per-statement host-parameter limit. Results come back in the caller's
    /// order, first occurrence wins, so a repeated ID cannot duplicate a row.
    pub fn event_delivery_states(
        &self,
        principal: &PublicKey,
        now: i64,
        event_ids: &[EventId],
    ) -> Result<Vec<EventDeliveryRow>, StorageError> {
        if event_ids.is_empty() {
            return Ok(Vec::new());
        }
        let channels: HashSet<Uuid> = self
            .accessible_channels(principal, now)?
            .into_iter()
            .collect();
        if channels.is_empty() {
            return Ok(Vec::new());
        }

        // `(label, carried_by_digest, demotion_reason)` per event id.
        let mut found: HashMap<String, (EventDeliveryState, bool, Option<String>)> = HashMap::new();
        let connection = self.connection.lock();
        for chunk in event_ids.chunks(DELIVERY_STATE_CHUNK) {
            let sql = format!(
                "SELECT o.event_id, o.state, o.delivery_path, o.demotion_reason, e.channel_id
                   FROM outbox o
                   JOIN events e ON e.event_id = o.event_id
                  WHERE o.event_id IN ({})",
                placeholders(1, chunk.len())
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
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })?;
            for row in rows {
                let (event_id, state, delivery_path, demotion_reason, channel_id) = row?;
                let channel = Uuid::parse_str(&channel_id)
                    .map_err(|error| StorageError::Corrupt(error.to_string()))?;
                if !channels.contains(&channel) {
                    continue;
                }
                let label =
                    EventDeliveryState::from_row(&state, &delivery_path).ok_or_else(|| {
                        StorageError::Corrupt(format!("unknown outbox state: '{state}'"))
                    })?;
                found.insert(
                    event_id,
                    (
                        label,
                        delivery_path == DIGEST_DELIVERY_PATH,
                        demotion_reason.map(|reason| truncate_quarantine_reason(&reason)),
                    ),
                );
            }
        }
        drop(connection);

        let mut states = Vec::with_capacity(found.len());
        let mut emitted = HashSet::new();
        for event_id in event_ids {
            let hex = event_id.to_hex();
            let Some((label, carried_by_digest, demotion_reason)) = found.get(&hex) else {
                continue;
            };
            if emitted.insert(hex) {
                states.push(EventDeliveryRow {
                    event_id: *event_id,
                    state: *label,
                    carried_by_digest: *carried_by_digest,
                    demotion_reason: demotion_reason.clone(),
                });
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
    /// another's messages. Channel access is checked too, through the same
    /// predicate as every read here, so an author whose membership was revoked
    /// cannot push its old rows back into the drain queue.
    ///
    /// **A digest-path row is refused, not silently moved.** Such a row is
    /// already being carried upstream by the edge identity
    /// (`digest_candidates` picks up `quarantined` rows on that path), and no
    /// drain will ever claim it. Moving it to `pending` would delete it from
    /// the quarantine list, from waiting-authors, and from the drain queue at
    /// once — the retry button's only visible effect would be to hide the row.
    ///
    /// `attempts` is deliberately **not** reset. The attempt count is the
    /// operator's evidence that this event keeps failing; zeroing it on every
    /// retry would hide a permanent rejection behind a fresh-looking row.
    pub fn requeue_quarantined(
        &self,
        event_id: &EventId,
        author: &PublicKey,
        now: i64,
    ) -> Result<RequeueOutcome, StorageError> {
        let row: Option<(String, String, String, String)> = self
            .connection
            .lock()
            .query_row(
                "SELECT o.state, o.delivery_path, e.author_pubkey, e.channel_id
                   FROM outbox o
                   JOIN events e ON e.event_id = o.event_id
                  WHERE o.event_id = ?1",
                [event_id.to_hex()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()?;
        let Some((state, delivery_path, row_author, channel_id)) = row else {
            return Ok(RequeueOutcome::NotFound);
        };
        if row_author != author.to_hex() || state != "quarantined" {
            return Ok(RequeueOutcome::NotFound);
        }
        let channel = Uuid::parse_str(&channel_id)
            .map_err(|error| StorageError::Corrupt(error.to_string()))?;
        if !self.principal_can_access_at(channel, author, now)? {
            return Ok(RequeueOutcome::NotFound);
        }
        if delivery_path == DIGEST_DELIVERY_PATH {
            return Ok(RequeueOutcome::CarriedByDigest);
        }

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
                AND delivery_path = 'exact'
                AND event_id IN (
                    SELECT event_id FROM events WHERE author_pubkey = ?2
                )",
            params![event_id.to_hex(), author.to_hex(), now],
        )?;
        Ok(if changed > 0 {
            RequeueOutcome::Requeued
        } else {
            RequeueOutcome::NotFound
        })
    }
}

/// Correlated `EXISTS` mirroring `claim_outbox_batch`'s dependency gate.
///
/// Kept as one string used by `waiting_authors` so the two cannot describe
/// different notions of "blocked": if this predicate is true, that claim query
/// will not return the row, full stop.
const BLOCKED_BY_ANCESTOR: &str = "EXISTS (
                        SELECT 1
                          FROM event_dependencies d
                          JOIN outbox po ON po.event_id = d.ancestor_event_id
                         WHERE d.child_event_id = o.event_id
                           AND po.state <> 'delivered'
                    )";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{
        AuthorizationPolicy, CommunityBinding, DrainOutcome, VerifiedChannelAuthorization,
    };
    use nostr::{EventBuilder, JsonUtil, Keys, Kind, Tag, Timestamp};
    use std::time::Duration;

    /// Every unit test here reads through the channel-access gate, so the
    /// fixtures share one authorization epoch: a lease verified at t=0, which
    /// covers every `now` the tests use.
    const VERIFIED_AT: i64 = 0;

    fn binding() -> CommunityBinding {
        CommunityBinding::new("wss://relay.example.com", Uuid::new_v4()).expect("binding")
    }

    fn policy() -> AuthorizationPolicy {
        AuthorizationPolicy::new(Duration::from_secs(72 * 60 * 60)).expect("policy")
    }

    fn store() -> EdgeStore {
        EdgeStore::open_in_memory(binding(), policy()).expect("store")
    }

    /// Select `channels` and give every listed identity an active roster entry.
    ///
    /// This is what production does at startup: a selected, edge-eligible
    /// channel plus a live authorization lease plus a relay-signed roster. The
    /// status reads are all gated on exactly that, so a fixture without it
    /// tests nothing — every row would be filtered out, or, before this gate
    /// existed, every row would be returned to anybody.
    fn authorize(store: &EdgeStore, edge: &Keys, channels: &[(Uuid, Vec<PublicKey>)]) {
        authorize_at(store, edge, VERIFIED_AT, channels);
    }

    /// Same, with an explicit roster-source timestamp. A replacement roster is
    /// only adopted when its source event is newer than the stored one, so a
    /// test that removes a member has to publish a newer source.
    fn authorize_at(
        store: &EdgeStore,
        edge: &Keys,
        roster_created_at: i64,
        channels: &[(Uuid, Vec<PublicKey>)],
    ) {
        let relay = Keys::generate();
        let mut authorizations = Vec::new();
        for (channel, members) in channels {
            store
                .set_channel_selected(*channel, true)
                .expect("selection");
            let mut tags = vec![Tag::parse(["d", channel.to_string().as_str()]).expect("d tag")];
            for member in members {
                tags.push(Tag::parse(["p", member.to_hex().as_str()]).expect("p tag"));
            }
            let event = EventBuilder::new(Kind::Custom(39_002), "")
                .tags(tags)
                .custom_created_at(Timestamp::from(roster_created_at as u64))
                .sign_with_keys(&relay)
                .expect("membership event");
            authorizations.push(VerifiedChannelAuthorization {
                channel_id: *channel,
                membership_event_id: event.id,
                membership_event_created_at: roster_created_at,
                membership_event_bytes: event.as_json().into_bytes(),
                membership_fetch_cursor: None,
                signal_cursor: None,
                edge_notification_cursor: None,
                active_authors: members.clone(),
                removed_authors: Vec::new(),
            });
        }
        store
            .persist_verified_authorization_snapshot(&authorizations, VERIFIED_AT, edge)
            .expect("snapshot");
    }

    /// One channel every listed identity belongs to — the common shape.
    fn authorize_one(store: &EdgeStore, edge: &Keys, channel: Uuid, members: &[&Keys]) {
        authorize(
            store,
            edge,
            &[(channel, members.iter().map(|k| k.public_key()).collect())],
        );
    }

    fn message(keys: &Keys, channel: Uuid, content: &str) -> nostr::Event {
        EventBuilder::new(Kind::Custom(9), content)
            .tags([Tag::parse(["h", channel.to_string().as_str()]).expect("h tag")])
            .sign_with_keys(keys)
            .expect("sign")
    }

    fn reply(keys: &Keys, channel: Uuid, parent: &nostr::Event, content: &str) -> nostr::Event {
        EventBuilder::new(Kind::Custom(9), content)
            .tags([
                Tag::parse(["h", channel.to_string().as_str()]).expect("h tag"),
                Tag::parse(["e", parent.id.to_hex().as_str(), "", "root"]).expect("root tag"),
                Tag::parse(["e", parent.id.to_hex().as_str(), "", "reply"]).expect("reply tag"),
            ])
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

    fn set_received_at(store: &EdgeStore, event_id: &EventId, received_at: i64) {
        store
            .connection
            .lock()
            .execute(
                "UPDATE events SET received_at = ?2 WHERE event_id = ?1",
                params![event_id.to_hex(), received_at],
            )
            .expect("set received_at");
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

    fn delivery_path(store: &EdgeStore, event_id: &EventId) -> String {
        store
            .connection
            .lock()
            .query_row(
                "SELECT delivery_path FROM outbox WHERE event_id = ?1",
                [event_id.to_hex()],
                |row| row.get(0),
            )
            .expect("delivery_path")
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

    // ── Channel scoping (§7) ────────────────────────────────────────────────

    #[test]
    fn a_principal_sees_no_trace_of_a_channel_it_is_not_a_member_of() {
        // The whole point of the status surface is that it is not filtered to
        // the *calling identity* — the operator has to see that an agent's
        // events are stuck. That is not the same as unfiltered. A principal
        // scoped to one channel must not learn another channel's UUID, who
        // posts there, when, or why a post failed.
        let store = store();
        let (alice, bob, edge) = (Keys::generate(), Keys::generate(), Keys::generate());
        let hers = Uuid::new_v4();
        let his = Uuid::new_v4();
        authorize(
            &store,
            &edge,
            &[
                (hers, vec![alice.public_key()]),
                (his, vec![bob.public_key()]),
            ],
        );

        let mine = message(&alice, hers, "mine");
        let theirs = message(&bob, his, "theirs");
        insert(&store, &mine, hers, &edge);
        insert(&store, &theirs, his, &edge);
        quarantine(&store, &alice, &mine.id, "refused");
        quarantine(&store, &bob, &theirs.id, "refused for a private reason");

        let alice_key = alice.public_key();
        let rows = store.quarantined_rows(&alice_key, 2_000, 50).expect("rows");
        assert_eq!(rows.len(), 1, "alice sees only her own channel's row");
        assert_eq!(rows[0].event_id, mine.id);
        assert!(
            !rows.iter().any(|row| row.channel_id == his),
            "another channel's UUID must not appear in the reply"
        );

        // `waitingAuthors` is a presence oracle if it is not scoped: it names
        // every identity with a queued row.
        insert(&store, &message(&bob, his, "still queued"), his, &edge);
        let waiting = store.waiting_authors(&alice_key, 2_000).expect("waiting");
        assert!(
            waiting.iter().all(|row| row.author != bob.public_key()),
            "bob's presence leaked through the waiting-author surface: {waiting:?}"
        );

        let states = store
            .event_delivery_states(&alice_key, 2_000, &[mine.id, theirs.id])
            .expect("states");
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].event_id, mine.id);

        // And the counts, which would otherwise report activity in a channel
        // alice cannot open.
        let scope = store.accessible_channels(&alice_key, 2_000).expect("scope");
        assert_eq!(scope, vec![hers]);
        assert_eq!(
            store.outbox_summary(&scope).expect("summary").quarantined,
            1
        );
    }

    #[test]
    fn a_stranger_with_a_fresh_keypair_sees_nothing_at_all() {
        // Authentication is not authorization: `verified_owner` returns `Ok` for
        // a connection with no NIP-OA tag, so a freshly generated keypair
        // reaches these reads. Membership is what has to stop it.
        let store = store();
        let (author, stranger, edge) = (Keys::generate(), Keys::generate(), Keys::generate());
        let channel = Uuid::new_v4();
        authorize_one(&store, &edge, channel, &[&author]);
        let event = message(&author, channel, "not for you");
        insert(&store, &event, channel, &edge);
        quarantine(&store, &author, &event.id, "refused");

        let intruder = stranger.public_key();
        assert!(store
            .accessible_channels(&intruder, 2_000)
            .expect("scope")
            .is_empty());
        assert!(store
            .quarantined_rows(&intruder, 2_000, 50)
            .expect("rows")
            .is_empty());
        assert!(store
            .waiting_authors(&intruder, 2_000)
            .expect("waiting")
            .is_empty());
        assert!(store
            .event_delivery_states(&intruder, 2_000, &[event.id])
            .expect("states")
            .is_empty());

        // The member still sees it, so the assertions above are about the
        // membership gate and not about an empty database.
        assert_eq!(
            store
                .quarantined_rows(&author.public_key(), 2_000, 50)
                .expect("rows")
                .len(),
            1
        );
    }

    #[test]
    fn an_expired_authorization_lease_closes_every_status_read() {
        // The eligibility sweep closes *subscriptions*, not sessions, so a
        // session that outlives its lease keeps reading. Each read re-evaluates
        // the lease at `now` for exactly that reason.
        let store = store();
        let (author, edge) = (Keys::generate(), Keys::generate());
        let channel = Uuid::new_v4();
        authorize_one(&store, &edge, channel, &[&author]);
        let event = message(&author, channel, "queued");
        insert(&store, &event, channel, &edge);
        quarantine(&store, &author, &event.id, "refused");

        let key = author.public_key();
        let lease_seconds = 72 * 60 * 60;
        assert_eq!(
            store
                .quarantined_rows(&key, lease_seconds, 50)
                .expect("rows")
                .len(),
            1,
            "the last second of the lease is still inside it"
        );
        assert!(
            store
                .quarantined_rows(&key, lease_seconds + 1, 50)
                .expect("rows")
                .is_empty(),
            "one second past expiry the surface must fail closed"
        );
        assert!(matches!(
            store.requeue_quarantined(&event.id, &key, lease_seconds + 1),
            Ok(RequeueOutcome::NotFound)
        ));
        assert_eq!(
            outbox_state(&store, &event.id),
            "quarantined",
            "a refused requeue must leave the row exactly as it was"
        );
    }

    #[test]
    fn a_removed_member_loses_the_surface_even_with_the_channel_still_selected() {
        let store = store();
        let (kept, removed, edge) = (Keys::generate(), Keys::generate(), Keys::generate());
        let channel = Uuid::new_v4();
        authorize_one(&store, &edge, channel, &[&kept, &removed]);
        let event = message(&kept, channel, "queued");
        insert(&store, &event, channel, &edge);

        assert_eq!(
            store
                .accessible_channels(&removed.public_key(), 2_000)
                .expect("scope"),
            vec![channel]
        );
        // A later roster drops the removed identity.
        authorize_at(
            &store,
            &edge,
            VERIFIED_AT + 1,
            &[(channel, vec![kept.public_key()])],
        );
        assert!(store
            .accessible_channels(&removed.public_key(), 2_000)
            .expect("scope")
            .is_empty());
        assert_eq!(
            store
                .accessible_channels(&kept.public_key(), 2_000)
                .expect("scope"),
            vec![channel]
        );
    }

    // ── Quarantine list (§11) ───────────────────────────────────────────────

    #[test]
    fn quarantined_rows_are_newest_first_with_a_stable_tie_break() {
        // The operator triages the newest failures first, and repeated polls
        // must not reshuffle rows under the cursor.
        let store = store();
        let (author, edge) = (Keys::generate(), Keys::generate());
        let channel = Uuid::new_v4();
        authorize_one(&store, &edge, channel, &[&author]);
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

        let rows = store
            .quarantined_rows(&author.public_key(), 2_000, 10)
            .expect("rows");
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
        authorize_one(&store, &edge, channel, &[&author]);
        for index in 0..3 {
            let event = message(&author, channel, &format!("refused {index}"));
            insert(&store, &event, channel, &edge);
            quarantine(&store, &author, &event.id, "refused");
        }

        let key = author.public_key();
        assert_eq!(MAX_QUARANTINE_PAGE, 500);
        assert_eq!(
            store.quarantined_rows(&key, 2_000, 1).expect("one").len(),
            1
        );
        assert!(store
            .quarantined_rows(&key, 2_000, 0)
            .expect("zero")
            .is_empty());
        // A caller asking for everything must be clamped, not allowed to pull
        // the whole table in behind the storage mutex.
        assert_eq!(
            store
                .quarantined_rows(&key, 2_000, usize::MAX)
                .expect("all")
                .len(),
            3
        );
    }

    #[test]
    fn a_quarantined_row_without_a_recorded_error_still_has_a_reason() {
        // A blank reason in the quarantine UI reads as "nothing is wrong",
        // which is the opposite of what the row means.
        let store = store();
        let (author, edge) = (Keys::generate(), Keys::generate());
        let channel = Uuid::new_v4();
        authorize_one(&store, &edge, channel, &[&author]);
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

        let rows = store
            .quarantined_rows(&author.public_key(), 2_000, 10)
            .expect("rows");
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].reason.trim().is_empty());
        assert_eq!(rows[0].reason, UNRECORDED_QUARANTINE_REASON);
    }

    #[test]
    fn a_reason_written_by_an_older_build_is_bounded_on_the_way_out() {
        // Capping the parser protects new writes. A row already in the table
        // was never capped, and one 1.6 MB reason per row is enough to make a
        // single status page hundreds of megabytes — which the desktop reports
        // as "sidecar not running", so the operator sees nothing at all.
        let store = store();
        let (author, edge) = (Keys::generate(), Keys::generate());
        let channel = Uuid::new_v4();
        authorize_one(&store, &edge, channel, &[&author]);
        let event = message(&author, channel, "refused");
        insert(&store, &event, channel, &edge);
        quarantine(&store, &author, &event.id, "refused");
        let huge = "y".repeat(MAX_QUARANTINE_REASON_BYTES * 3_000);
        store
            .connection
            .lock()
            .execute(
                "UPDATE outbox SET last_error = ?2 WHERE event_id = ?1",
                params![event.id.to_hex(), huge],
            )
            .expect("write an unbounded reason the way an older build would");

        let rows = store
            .quarantined_rows(&author.public_key(), 2_000, 10)
            .expect("rows");
        assert_eq!(rows.len(), 1);
        assert!(
            rows[0].reason.len() <= MAX_QUARANTINE_REASON_BYTES,
            "quarantine list surfaced {} bytes of reason",
            rows[0].reason.len()
        );
        assert!(rows[0].reason.ends_with('\u{2026}'));
    }

    #[test]
    fn the_write_path_bounds_the_reason_before_it_ever_reaches_the_table() {
        let store = store();
        let (author, edge) = (Keys::generate(), Keys::generate());
        let channel = Uuid::new_v4();
        authorize_one(&store, &edge, channel, &[&author]);
        let event = message(&author, channel, "refused");
        insert(&store, &event, channel, &edge);
        quarantine(
            &store,
            &author,
            &event.id,
            &"z".repeat(MAX_QUARANTINE_REASON_BYTES * 10),
        );

        let stored = last_error(&store, &event.id).expect("last_error");
        assert!(
            stored.len() <= MAX_QUARANTINE_REASON_BYTES,
            "stored {} bytes",
            stored.len()
        );
    }

    // ── Waiting for author (§11) ────────────────────────────────────────────

    #[test]
    fn waiting_authors_includes_an_expired_lease_and_excludes_a_live_one() {
        // An expired lease means the author went away mid-drain — the absent
        // author condition Desktop has to show, without waiting for the next
        // drain cycle to sweep the row back to `pending`.
        let store = store();
        let (gone, present, edge) = (Keys::generate(), Keys::generate(), Keys::generate());
        let channel = Uuid::new_v4();
        authorize_one(&store, &edge, channel, &[&gone, &present]);
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

        let waiting = store
            .waiting_authors(&gone.public_key(), 1_100)
            .expect("waiting");
        assert_eq!(waiting.len(), 1, "only the vanished author is waiting");
        assert_eq!(waiting[0].author, gone.public_key());
        assert_eq!(waiting[0].pending, 1);
        assert_eq!(waiting[0].ancestor_blocked, 0);
        assert_eq!(waiting[0].pending_via_digest, 0);
    }

    #[test]
    fn waiting_authors_counts_pending_rows_oldest_first() {
        let store = store();
        let (early, late, edge) = (Keys::generate(), Keys::generate(), Keys::generate());
        let channel = Uuid::new_v4();
        authorize_one(&store, &edge, channel, &[&early, &late]);
        let first = message(&early, channel, "first");
        let second = message(&early, channel, "second");
        let third = message(&late, channel, "third");
        for event in [&first, &second, &third] {
            insert(&store, event, channel, &edge);
        }
        // `received_at` is wall-clock on insert, so pin it for determinism.
        for (event, received_at) in [(&first, 100), (&second, 400), (&third, 300)] {
            set_received_at(&store, &event.id, received_at);
        }

        let waiting = store
            .waiting_authors(&early.public_key(), 9_999)
            .expect("waiting");
        assert_eq!(waiting.len(), 2);
        assert_eq!(waiting[0].author, early.public_key());
        assert_eq!(waiting[0].pending, 2);
        assert_eq!(waiting[0].oldest_pending_at, 100);
        assert_eq!(waiting[1].author, late.public_key());
        assert_eq!(waiting[1].oldest_pending_at, 300);
    }

    #[test]
    fn a_row_blocked_behind_an_unreplayable_ancestor_is_not_waiting_for_its_author() {
        // The scenario the old count got wrong: alice's root is quarantined,
        // her reply depends on it, and `claim_outbox_batch` refuses the reply
        // because a locally known ancestor has not reached `delivered`. Her
        // drain client runs every 30 seconds and correctly claims nothing —
        // while the surface said "waiting for alice, 1 pending" forever.
        let store = store();
        let (alice, edge) = (Keys::generate(), Keys::generate());
        let channel = Uuid::new_v4();
        authorize_one(&store, &edge, channel, &[&alice]);
        let root = message(&alice, channel, "root");
        insert(&store, &root, channel, &edge);
        let child = reply(&alice, channel, &root, "reply to root");
        insert(&store, &child, channel, &edge);
        quarantine(&store, &alice, &root.id, "refused upstream");

        let key = alice.public_key();
        assert!(
            store
                .claim_outbox_batch(&key, "drain", 10, 2_000, 60)
                .expect("claim")
                .is_empty(),
            "the reply is not claimable while its ancestor is stuck"
        );

        let waiting = store.waiting_authors(&key, 2_000).expect("waiting");
        assert_eq!(waiting.len(), 1);
        assert_eq!(
            waiting[0].pending, 0,
            "nothing here is claimable, so nothing is waiting for the author"
        );
        assert_eq!(
            waiting[0].ancestor_blocked, 1,
            "the reply is stuck behind its ancestor, which is the actionable state"
        );

        // Clearing the ancestor moves the reply into the claimable column, so
        // the two counts track `claim_outbox_batch` rather than merely
        // splitting a constant.
        assert!(matches!(
            store.requeue_quarantined(&root.id, &key, 2_000),
            Ok(RequeueOutcome::Requeued)
        ));
        let batch = store
            .claim_outbox_batch(&key, "drain-2", 10, 2_100, 60)
            .expect("claim");
        assert_eq!(batch.len(), 1);
        store
            .acknowledge_outbox_row("drain-2", &root.id, DrainOutcome::Delivered)
            .expect("ack root");
        let waiting = store.waiting_authors(&key, 2_200).expect("waiting");
        assert_eq!(waiting[0].pending, 1);
        assert_eq!(waiting[0].ancestor_blocked, 0);
    }

    #[test]
    fn a_digest_path_row_is_counted_separately_rather_than_left_unexplained() {
        // After `materialize_digest_batch` moves rows to the digest path the
        // summary can say `pending: 40` while waiting-authors sums to 5, with
        // 35 rows having no surface and no explanation. The digest column is
        // that explanation.
        let store = store();
        let (author, edge) = (Keys::generate(), Keys::generate());
        let channel = Uuid::new_v4();
        authorize_one(&store, &edge, channel, &[&author]);
        let exact = message(&author, channel, "still exact");
        let demoted = message(&author, channel, "demoted");
        insert(&store, &exact, channel, &edge);
        insert(&store, &demoted, channel, &edge);
        store
            .connection
            .lock()
            .execute(
                "UPDATE outbox
                    SET delivery_path = 'digest',
                        demotion_reason = 'older than the relay drift window'
                  WHERE event_id = ?1",
                [demoted.id.to_hex()],
            )
            .expect("demote");

        let key = author.public_key();
        let waiting = store.waiting_authors(&key, 2_000).expect("waiting");
        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0].pending, 1);
        assert_eq!(waiting[0].pending_via_digest, 1);

        let scope = store.accessible_channels(&key, 2_000).expect("scope");
        let summary = store.outbox_summary(&scope).expect("summary");
        assert_eq!(
            summary.pending + summary.pending_via_digest,
            waiting[0].pending + waiting[0].ancestor_blocked + waiting[0].pending_via_digest,
            "every pending row must be accounted for by a waiting-author column"
        );

        // And per event, with the reason it left the exact path.
        let states = store
            .event_delivery_states(&key, 2_000, &[exact.id, demoted.id])
            .expect("states");
        assert_eq!(states[0].state, EventDeliveryState::Pending);
        assert_eq!(states[1].state, EventDeliveryState::PendingViaDigest);
        assert_eq!(
            states[1].demotion_reason.as_deref(),
            Some("older than the relay drift window"),
            "the column that explains the demotion has to reach the caller"
        );
    }

    /// BUG-023, the waiting-author half: one `MIN(received_at)` across all
    /// three buckets, printed beside whichever count the UI happened to be
    /// drawing. The age shown next to "4 events blocked" could belong to a
    /// perfectly claimable row, and the age next to "2 events queued" could
    /// belong to a digest row no author is coming for — overstating a wait that
    /// no waiting row has.
    #[test]
    fn each_waiting_count_carries_the_age_of_its_own_bucket() {
        let store = store();
        let (author, edge) = (Keys::generate(), Keys::generate());
        let channel = Uuid::new_v4();
        authorize_one(&store, &edge, channel, &[&author]);

        let claimable = message(&author, channel, "claimable");
        let root = message(&author, channel, "root that will be refused");
        insert(&store, &claimable, channel, &edge);
        insert(&store, &root, channel, &edge);
        let blocked = reply(&author, channel, &root, "blocked behind the root");
        insert(&store, &blocked, channel, &edge);
        let demoted = message(&author, channel, "carried by the digest");
        insert(&store, &demoted, channel, &edge);

        // The root leaves the live buckets entirely, and takes its reply's
        // claimability with it.
        quarantine(&store, &author, &root.id, "refused upstream");
        store
            .connection
            .lock()
            .execute(
                "UPDATE outbox SET delivery_path = 'digest' WHERE event_id = ?1",
                [demoted.id.to_hex()],
            )
            .expect("demote");

        // Each bucket gets a distinct arrival time, and the digest row — the
        // one with NO author coming for it — is the oldest of the three.
        for (event, received_at) in [(&claimable, 500), (&blocked, 300), (&demoted, 100)] {
            set_received_at(&store, &event.id, received_at);
        }

        let key = author.public_key();
        let waiting = store.waiting_authors(&key, 2_000).expect("waiting");
        assert_eq!(waiting.len(), 1);
        let row = &waiting[0];
        assert_eq!(
            (row.pending, row.ancestor_blocked, row.pending_via_digest),
            (1, 1, 1)
        );

        assert_eq!(
            row.oldest_pending_at, 100,
            "the aggregate still spans all three buckets"
        );
        assert_eq!(
            row.oldest_claimable_at,
            Some(500),
            "the age beside 'queued' must belong to a row an author can claim, \
             not to the digest row that is oldest overall"
        );
        assert_eq!(
            row.oldest_ancestor_blocked_at,
            Some(300),
            "and the age beside 'blocked' must belong to a blocked row"
        );
        assert_ne!(
            row.oldest_claimable_at,
            Some(row.oldest_pending_at),
            "a per-bucket age that merely echoes the aggregate is the bug"
        );
        assert_ne!(row.oldest_ancestor_blocked_at, Some(row.oldest_pending_at));
    }

    #[test]
    fn an_empty_waiting_bucket_reports_no_age_at_all() {
        // `None`, never 0 and never a neighbouring bucket's timestamp: a 0
        // renders as an age of decades and a borrowed timestamp is the bug
        // above wearing a different hat.
        let store = store();
        let (author, edge) = (Keys::generate(), Keys::generate());
        let channel = Uuid::new_v4();
        authorize_one(&store, &edge, channel, &[&author]);
        let only_pending = message(&author, channel, "nothing is blocked here");
        insert(&store, &only_pending, channel, &edge);
        set_received_at(&store, &only_pending.id, 700);

        let waiting = store
            .waiting_authors(&author.public_key(), 2_000)
            .expect("waiting");
        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0].ancestor_blocked, 0);
        assert_eq!(
            waiting[0].oldest_ancestor_blocked_at, None,
            "an empty bucket has no oldest row, so it must report none"
        );
        assert_eq!(waiting[0].oldest_claimable_at, Some(700));
    }

    // ── Per-event delivery state (§13) ──────────────────────────────────────

    /// BUG-023: the quarantine list and the per-message badge disagreed about
    /// one row. `from_row` kept `delivery_path` for the pending and delivered
    /// arms and dropped it for quarantined, so the list could say "the digest
    /// is already carrying this, the refused retry was correct, do nothing"
    /// while the badge for that same event said only "Sync failed" — which
    /// reads as "stuck, act now".
    #[test]
    fn the_badge_and_the_quarantine_list_agree_about_a_digest_carried_row() {
        let store = store();
        let (author, edge) = (Keys::generate(), Keys::generate());
        let channel = Uuid::new_v4();
        authorize_one(&store, &edge, channel, &[&author]);
        let stuck = message(&author, channel, "refused and genuinely stuck");
        let carried = message(&author, channel, "refused, then demoted");
        insert(&store, &stuck, channel, &edge);
        insert(&store, &carried, channel, &edge);
        quarantine(&store, &author, &stuck.id, "permanently refused");
        quarantine(&store, &author, &carried.id, "permanently refused");

        // Only the second row is moved onto the digest path, by the real
        // demotion pass rather than by hand.
        store
            .connection
            .lock()
            .execute(
                "UPDATE outbox SET delivery_path = 'digest' WHERE event_id = ?1",
                [carried.id.to_hex()],
            )
            .expect("demote");
        assert_eq!(delivery_path(&store, &stuck.id), "exact");
        assert_eq!(delivery_path(&store, &carried.id), "digest");

        let key = author.public_key();
        let list = store.quarantined_rows(&key, 2_000, 10).expect("rows");
        let badges = store
            .event_delivery_states(&key, 2_000, &[stuck.id, carried.id])
            .expect("states");
        assert_eq!(badges.len(), 2);

        // Both rows are quarantined, so the label alone cannot tell them apart
        // — which is exactly why the flag has to travel with it.
        assert_eq!(badges[0].state, EventDeliveryState::Quarantined);
        assert_eq!(badges[1].state, EventDeliveryState::Quarantined);
        assert!(
            !badges[0].carried_by_digest,
            "a genuinely stuck row must not be dressed up as handled"
        );
        assert!(
            badges[1].carried_by_digest,
            "the badge for a digest-carried row still said only 'quarantined'"
        );

        // The property the bug is about: one row, one answer, on both surfaces.
        for badge in &badges {
            let listed = list
                .iter()
                .find(|row| row.event_id == badge.event_id)
                .expect("every quarantined row is in the list");
            assert_eq!(
                listed.carried_by_digest,
                badge.carried_by_digest,
                "the list and the badge disagree about {}",
                badge.event_id.to_hex()
            );
        }

        // And the answer is the one the retry path acts on, so the operator is
        // never told "nothing to do" about a row that would in fact requeue.
        assert_eq!(
            store
                .requeue_quarantined(&carried.id, &key, 2_000)
                .expect("requeue carried"),
            RequeueOutcome::CarriedByDigest
        );
        assert_eq!(
            store
                .requeue_quarantined(&stuck.id, &key, 2_000)
                .expect("requeue stuck"),
            RequeueOutcome::Requeued
        );
    }

    #[test]
    fn the_carried_flag_is_the_delivery_path_in_every_state() {
        // The flag means one thing — "this row is on the digest path" — in
        // every state, so a reader never has to know which states it applies
        // to. A per-state special case is how the two surfaces drifted apart
        // in the first place.
        let store = store();
        let (author, edge) = (Keys::generate(), Keys::generate());
        let channel = Uuid::new_v4();
        authorize_one(&store, &edge, channel, &[&author]);
        let exact = message(&author, channel, "exact");
        let digest = message(&author, channel, "digest");
        insert(&store, &exact, channel, &edge);
        insert(&store, &digest, channel, &edge);
        store
            .connection
            .lock()
            .execute(
                "UPDATE outbox SET delivery_path = 'digest' WHERE event_id = ?1",
                [digest.id.to_hex()],
            )
            .expect("demote");

        let key = author.public_key();
        for state in ["pending", "claimed", "delivered", "quarantined"] {
            store
                .connection
                .lock()
                .execute(
                    "UPDATE outbox SET state = ?1 WHERE event_id IN (?2, ?3)",
                    params![state, exact.id.to_hex(), digest.id.to_hex()],
                )
                .expect("set state");
            let states = store
                .event_delivery_states(&key, 2_000, &[exact.id, digest.id])
                .expect("states");
            assert_eq!(states.len(), 2, "state '{state}'");
            for row in states {
                let expected = delivery_path(&store, &row.event_id) == "digest";
                assert_eq!(
                    row.carried_by_digest, expected,
                    "state '{state}': carried_by_digest must mirror delivery_path"
                );
            }
        }
    }

    #[test]
    fn event_delivery_states_maps_every_state() {
        let store = store();
        let (author, edge) = (Keys::generate(), Keys::generate());
        let channel = Uuid::new_v4();
        authorize_one(&store, &edge, channel, &[&author]);
        let pending = message(&author, channel, "pending");
        let pending_digest = message(&author, channel, "pending on the digest path");
        let claimed = message(&author, channel, "claimed");
        let exact = message(&author, channel, "exact");
        let digest = message(&author, channel, "digest");
        let refused = message(&author, channel, "refused");
        for event in [
            &pending,
            &pending_digest,
            &claimed,
            &exact,
            &digest,
            &refused,
        ] {
            insert(&store, event, channel, &edge);
        }

        quarantine(&store, &author, &refused.id, "refused upstream");
        let batch = store
            .claim_outbox_batch(&author.public_key(), "t", 10, 1_000, 600)
            .expect("claim");
        assert_eq!(batch.len(), 5, "the quarantined row is no longer claimable");
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
        // Return two rows to `pending` so every live state is represented.
        store
            .connection
            .lock()
            .execute(
                "UPDATE outbox
                    SET state = 'pending', claim_token = NULL,
                        lease_owner_pubkey = NULL, lease_expires_at = NULL
                  WHERE event_id IN (?1, ?2)",
                params![pending.id.to_hex(), pending_digest.id.to_hex()],
            )
            .expect("reset to pending");
        store
            .connection
            .lock()
            .execute(
                "UPDATE outbox SET delivery_path = 'digest' WHERE event_id = ?1",
                [pending_digest.id.to_hex()],
            )
            .expect("demote the pending row");

        let unknown = EventId::from_hex(&"ab".repeat(32)).expect("unknown id");
        let states = store
            .event_delivery_states(
                &author.public_key(),
                2_000,
                &[
                    pending.id,
                    pending_digest.id,
                    claimed.id,
                    exact.id,
                    digest.id,
                    refused.id,
                    unknown,
                ],
            )
            .expect("states");
        let labelled: Vec<(EventId, EventDeliveryState)> =
            states.iter().map(|row| (row.event_id, row.state)).collect();
        assert_eq!(
            labelled,
            vec![
                (pending.id, EventDeliveryState::Pending),
                (pending_digest.id, EventDeliveryState::PendingViaDigest),
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
        authorize_one(&store, &edge, channel, &[&author]);
        let event = message(&author, channel, "the only real row");
        insert(&store, &event, channel, &edge);

        let mut ids: Vec<EventId> = (0..(DELIVERY_STATE_CHUNK * 2 + 7))
            .map(|index| EventId::from_hex(&format!("{index:064x}")).expect("synthetic id"))
            .collect();
        ids.push(event.id);
        assert!(ids.len() > DELIVERY_STATE_CHUNK);

        let states = store
            .event_delivery_states(&author.public_key(), 2_000, &ids)
            .expect("states");
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].event_id, event.id);
        assert_eq!(states[0].state, EventDeliveryState::Pending);
    }

    #[test]
    fn event_delivery_states_is_empty_for_an_empty_input() {
        assert!(store()
            .event_delivery_states(&Keys::generate().public_key(), 2_000, &[])
            .expect("states")
            .is_empty());
    }

    // ── Manual retry (§11) ──────────────────────────────────────────────────

    #[test]
    fn requeue_quarantined_refuses_another_authors_row() {
        // One local identity must never be able to act on another's messages —
        // the same boundary that stops `claim_outbox_batch` from crossing
        // identities.
        let store = store();
        let (owner, stranger, edge) = (Keys::generate(), Keys::generate(), Keys::generate());
        let channel = Uuid::new_v4();
        authorize_one(&store, &edge, channel, &[&owner, &stranger]);
        let event = message(&owner, channel, "not yours");
        insert(&store, &event, channel, &edge);
        quarantine(&store, &owner, &event.id, "refused upstream");

        assert_eq!(
            store
                .requeue_quarantined(&event.id, &stranger.public_key(), 2_000)
                .expect("requeue"),
            RequeueOutcome::NotFound
        );
        // The return value is not the whole guarantee: the row must be intact.
        assert_eq!(outbox_state(&store, &event.id), "quarantined");
        assert_eq!(
            last_error(&store, &event.id).as_deref(),
            Some("refused upstream")
        );
        assert_eq!(
            store
                .quarantined_rows(&owner.public_key(), 2_000, 10)
                .expect("rows")
                .len(),
            1
        );
    }

    #[test]
    fn requeue_quarantined_only_moves_a_quarantined_row() {
        let store = store();
        let (author, edge) = (Keys::generate(), Keys::generate());
        let channel = Uuid::new_v4();
        authorize_one(&store, &edge, channel, &[&author]);
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

        let key = author.public_key();
        assert_eq!(
            store
                .requeue_quarantined(&still_pending.id, &key, 2_000)
                .expect("requeue pending"),
            RequeueOutcome::NotFound
        );
        assert_eq!(outbox_state(&store, &still_pending.id), "pending");

        assert_eq!(
            store
                .requeue_quarantined(&delivered.id, &key, 2_000)
                .expect("requeue delivered"),
            RequeueOutcome::NotFound
        );
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
        authorize_one(&store, &edge, channel, &[&author]);
        let event = message(&author, channel, "keeps failing");
        insert(&store, &event, channel, &edge);
        quarantine(&store, &author, &event.id, "refused once");
        let before = outbox_attempts(&store, &event.id);
        assert_eq!(before, 1);

        let key = author.public_key();
        assert_eq!(
            store
                .requeue_quarantined(&event.id, &key, 2_000)
                .expect("requeue"),
            RequeueOutcome::Requeued
        );
        assert_eq!(outbox_attempts(&store, &event.id), before);
        assert_eq!(outbox_state(&store, &event.id), "pending");
        assert!(last_error(&store, &event.id).is_none());
        assert!(store
            .quarantined_rows(&key, 2_000, 10)
            .expect("rows")
            .is_empty());
    }

    #[test]
    fn a_requeued_row_is_claimable_again() {
        // The point of the manual retry is a real second attempt, so the row
        // has to come back through the ordinary drain path.
        let store = store();
        let (author, edge) = (Keys::generate(), Keys::generate());
        let channel = Uuid::new_v4();
        authorize_one(&store, &edge, channel, &[&author]);
        let event = message(&author, channel, "retry me");
        insert(&store, &event, channel, &edge);
        quarantine(&store, &author, &event.id, "transient-looking rejection");
        assert!(store
            .claim_outbox_batch(&author.public_key(), "blocked", 10, 2_000, 60)
            .expect("claim")
            .is_empty());

        assert_eq!(
            store
                .requeue_quarantined(&event.id, &author.public_key(), 2_000)
                .expect("requeue"),
            RequeueOutcome::Requeued
        );
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
    fn retry_is_refused_once_the_demotion_pass_has_moved_the_row_to_the_digest_path() {
        // `demote_unreplayable_threads` demotes every quarantined exact row,
        // and `digest_candidates` already carries `(quarantined, digest)` rows
        // upstream. Setting such a row to `pending` while leaving it on the
        // digest path made it vanish from the quarantine list, from
        // waiting-authors' exact column, and from the drain queue at once — the
        // button's only visible effect was to delete the operator's view of it.
        let store = store();
        let (author, edge) = (Keys::generate(), Keys::generate());
        let channel = Uuid::new_v4();
        authorize_one(&store, &edge, channel, &[&author]);
        let event = message(&author, channel, "refused, then demoted");
        insert(&store, &event, channel, &edge);
        quarantine(&store, &author, &event.id, "permanently refused");
        assert_eq!(
            store
                .demote_unreplayable_threads(900, 2_000)
                .expect("demote"),
            1
        );
        assert_eq!(delivery_path(&store, &event.id), "digest");
        assert_eq!(
            store.digest_candidates(10).expect("candidates").len(),
            1,
            "the edge is already carrying this row upstream"
        );

        let key = author.public_key();
        assert_eq!(
            store
                .requeue_quarantined(&event.id, &key, 2_000)
                .expect("requeue"),
            RequeueOutcome::CarriedByDigest,
            "the operator has to be told why, not handed a bare false"
        );
        assert_eq!(
            outbox_state(&store, &event.id),
            "quarantined",
            "a refused retry must leave the row exactly where it was"
        );

        // And the list says so, so the operator is not invited to press a
        // button that cannot work.
        let rows = store.quarantined_rows(&key, 2_000, 10).expect("rows");
        assert_eq!(rows.len(), 1);
        assert!(rows[0].carried_by_digest);
        assert_eq!(
            rows[0].demotion_reason.as_deref(),
            Some("permanently rejected upstream")
        );
    }

    #[test]
    fn requeue_quarantined_reports_not_found_for_an_unknown_event() {
        let store = store();
        let unknown = EventId::from_hex(&"cd".repeat(32)).expect("unknown id");
        let result = store.requeue_quarantined(&unknown, &Keys::generate().public_key(), 1_000);
        assert!(
            matches!(result, Ok(RequeueOutcome::NotFound)),
            "no row, no error"
        );
    }

    // ── Wire vocabulary (§13) ───────────────────────────────────────────────

    #[test]
    fn every_delivery_state_label_round_trips_under_its_pinned_name() {
        // These strings are the wire, and the frontend switches on them. Pinning
        // only one variant meant renaming any of the others changed the wire
        // with nothing going red.
        let pinned = [
            (EventDeliveryState::Pending, "pending"),
            (EventDeliveryState::PendingViaDigest, "pendingViaDigest"),
            (EventDeliveryState::Claimed, "claimed"),
            (EventDeliveryState::SyncedExact, "syncedExact"),
            (EventDeliveryState::SyncedViaDigest, "syncedViaDigest"),
            (EventDeliveryState::Quarantined, "quarantined"),
        ];
        for (state, name) in pinned {
            let json = serde_json::to_string(&state).expect("serialize");
            assert_eq!(json, format!("\"{name}\""));
            let round_trip: EventDeliveryState = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(round_trip, state);
        }
        // Exhaustiveness: adding a variant without pinning it has to fail here.
        // The match is over the same list the table above enumerates.
        for (state, _) in pinned {
            match state {
                EventDeliveryState::Pending
                | EventDeliveryState::PendingViaDigest
                | EventDeliveryState::Claimed
                | EventDeliveryState::SyncedExact
                | EventDeliveryState::SyncedViaDigest
                | EventDeliveryState::Quarantined => {}
            }
        }
        assert_eq!(
            pinned.len(),
            6,
            "a new EventDeliveryState variant must be added to this table"
        );
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
        assert_eq!(
            EventDeliveryState::from_row("pending", "digest"),
            Some(EventDeliveryState::PendingViaDigest),
            "the exact/digest split does not start at the terminal state"
        );
    }

    #[test]
    fn a_reason_shorter_than_the_cap_is_returned_untouched() {
        let short = "upstream refused: membership revoked";
        assert_eq!(truncate_quarantine_reason(short), short);
        let exact = "q".repeat(MAX_QUARANTINE_REASON_BYTES);
        assert_eq!(truncate_quarantine_reason(&exact), exact);
    }
}
