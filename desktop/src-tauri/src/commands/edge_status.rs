//! Frontend access to the edge sidecar's delivery state (SPEC-2026-08-05 §11).
//!
//! Success criterion (3) of the spec requires the two delivery states —
//! **delivered locally** and **synced to canonical history** — to be labelled
//! separately everywhere they surface. These commands are what makes that
//! possible in the UI: they carry the sidecar's real outbox state, never a
//! guess derived from whether a send call returned.
//!
//! The wire vocabulary is the sidecar's (`crates/buzz-edge/src/protocol.rs`).
//! Canonical history is spelled `synced*` there — in the summary counters and
//! in `EventDeliveryState` both — so it is spelled `synced*` here. One word for
//! one concept, end to end.
//!
//! Every command fails with [`relay::edge::EDGE_UNAVAILABLE`] when no sidecar
//! is reachable. That is the normal state — edge routing is opt-in and off by
//! default — so the frontend treats it as "render nothing", not as an error.

use serde::{Deserialize, Deserializer, Serialize};

use crate::app_state::AppState;
use crate::relay::edge;

/// Quarantine rows returned when the caller does not ask for a specific page.
const DEFAULT_QUARANTINE_LIMIT: usize = 200;

/// Deserialize a nullable field that is nonetheless REQUIRED to be present.
///
/// Serde's default for `Option<T>` silently turns an absent key into `None`,
/// which is exactly the version-skew hole the rest of this module is built to
/// avoid: an older sidecar that never learned to send `demotionReason` would be
/// indistinguishable from a current one saying "this row was never demoted",
/// and the operator would read a blank where a reason belongs. Pointing
/// `deserialize_with` at this makes an absent key a hard error while still
/// accepting the explicit `null` the sidecar sends for an undemoted row.
///
/// This is deliberately NOT `#[serde(default)]` — see [`EdgeStatusPayload`].
fn required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

/// Outbox counts behind the delivery labels.
///
/// `synced_exact` and `synced_via_digest` stay separate all the way to the UI:
/// an event replayed under its own ID and an event collapsed into an
/// edge-authored digest are both "synced", but they are not the same thing,
/// and the spec forbids merging them. `pending_via_digest` is the same split
/// one step earlier — a pending row nobody's author will ever claim, because
/// the edge identity carries it — and it is separate for the same reason: a
/// count with no explanation on screen is a count the operator cannot act on.
///
/// No `Default`: an all-zero summary must never be constructible by accident.
/// "Everything is fine" is the most dangerous thing this struct can say, so it
/// may only ever come from counters the sidecar actually sent.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EdgeDeliverySummary {
    pending: u64,
    pending_via_digest: u64,
    claimed: u64,
    synced_exact: u64,
    synced_via_digest: u64,
    quarantined: u64,
}

/// One permanently-refused event, as shown in the quarantine list.
///
/// Carries identifiers and the failure reason only. Message content is
/// deliberately absent: the quarantine list is community-wide so the operator
/// can see an agent's stuck events, and widening it to content would turn a
/// status surface into a way to read another identity's messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EdgeQuarantinedEvent {
    event_id: String,
    channel_id: String,
    author: String,
    created_at: i64,
    attempts: u32,
    reason: String,
    /// True when the edge identity is already carrying this row upstream in a
    /// catch-up digest. The sidecar refuses a retry on such a row
    /// (`outcome: "carriedByDigest"`), so a UI that offers one is offering a
    /// button that cannot work. Required, never defaulted: a missing key
    /// defaulting to `false` would re-arm exactly that button.
    carried_by_digest: bool,
    /// Why the row left the exact path, when it has. Present but `null` on a
    /// row that was never demoted.
    #[serde(deserialize_with = "required_nullable")]
    demotion_reason: Option<String>,
    updated_at: i64,
}

/// An identity with queued events and nobody online to push them upstream.
///
/// The three counts are three different problems, and only the first one is
/// solved by the author coming back. See `WaitingAuthor` in
/// `crates/buzz-edge/src/storage_status.rs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EdgeWaitingAuthor {
    author: String,
    /// Rows this author's drain client can claim right now.
    pending: u64,
    /// Rows the author *cannot* claim, because an ancestor of theirs never
    /// reached canonical history. Author uptime does not move these; retrying
    /// or discarding the ancestor does.
    ancestor_blocked: u64,
    /// Rows the edge identity carries upstream in a digest. No author is
    /// coming for them, so they must never be summed into a "waiting" figure.
    pending_via_digest: u64,
    oldest_pending_at: i64,
}

/// One event's delivery label, plus the reason it left the exact path.
///
/// Objects rather than `[id, state]` pairs, matching the sidecar's
/// `delivery_states_payload`: `pendingViaDigest` on its own tells the operator
/// *where* an event went but not *why*, and why is the actionable half —
/// "older than the relay drift window" is routine, "permanently rejected
/// upstream" is not.
///
/// `state` is a free `String` on purpose. The sidecar ships separately and
/// gains states before this build knows them; an unknown one must cost one
/// neutral badge in the timeline, not the whole batch, so the narrowing
/// happens in the frontend's `edgeDeliveryStateKey` and not in a Rust enum
/// that would reject the entire response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EdgeEventDeliveryState {
    event_id: String,
    state: String,
    #[serde(deserialize_with = "required_nullable")]
    demotion_reason: Option<String>,
}

/// What the sidecar actually did with a manual retry.
///
/// The boolean alone cannot separate "no such row of yours" from "the edge is
/// already carrying this row upstream, stop pressing Retry" — both are `false`
/// and they are completely different advice. `outcome` is that second half.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EdgeRequeueResult {
    requeued: bool,
    /// `"requeued" | "notFound" | "carriedByDigest"` today. Kept as a `String`
    /// so a newer sidecar's fourth outcome reaches the UI as an unrecognised
    /// outcome rather than failing the whole retry.
    outcome: String,
}

/// The one outcome that means a row actually moved.
const REQUEUE_OUTCOME_REQUEUED: &str = "requeued";

/// The whole status payload, parsed once.
///
/// Every field is REQUIRED on purpose. The sidecar emits all three keys
/// unconditionally (empty arrays when there is nothing to report), so
/// `#[serde(default)]` here would buy nothing and cost everything: a sidecar
/// that renamed `summary` to `counts` would parse cleanly into all-zero
/// counters, and the operator would read "nothing pending, nothing
/// quarantined" while five hundred events sat stuck. A version skew has to be
/// an error the operator sees, not a reassuring zero.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EdgeStatusPayload {
    summary: EdgeDeliverySummary,
    quarantined: Vec<EdgeQuarantinedEvent>,
    waiting_authors: Vec<EdgeWaitingAuthor>,
}

async fn status(state: &AppState, limit: usize) -> Result<EdgeStatusPayload, String> {
    let value = edge::fetch_edge_status(state, limit).await?;
    // A version-skewed sidecar returning an unexpected shape must fail loudly
    // here rather than reaching the frontend as a half-parsed object.
    serde_json::from_value(value).map_err(|error| format!("edge status was unreadable: {error}"))
}

#[tauri::command]
pub async fn edge_delivery_summary(
    state: tauri::State<'_, AppState>,
) -> Result<EdgeDeliverySummary, String> {
    // The summary needs no quarantine rows, so ask for the smallest page the
    // sidecar accepts instead of paying for 200 rows nobody reads.
    Ok(status(&state, 1).await?.summary)
}

#[tauri::command]
pub async fn edge_quarantined_events(
    limit: Option<usize>,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<EdgeQuarantinedEvent>, String> {
    let limit = limit.unwrap_or(DEFAULT_QUARANTINE_LIMIT).max(1);
    Ok(status(&state, limit).await?.quarantined)
}

#[tauri::command]
pub async fn edge_waiting_authors(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<EdgeWaitingAuthor>, String> {
    Ok(status(&state, 1).await?.waiting_authors)
}

/// Per-event delivery state, in the caller's own order.
///
/// IDs the sidecar does not know about are absent from the result rather than
/// reported as an error — those events came from upstream and were never in
/// the local outbox at all.
#[tauri::command]
pub async fn edge_event_delivery_states(
    event_ids: Vec<String>,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<EdgeEventDeliveryState>, String> {
    if event_ids.is_empty() {
        return Ok(Vec::new());
    }
    let value = edge::fetch_edge_delivery_states(&state, &event_ids).await?;
    serde_json::from_value(value)
        .map_err(|error| format!("edge delivery states were unreadable: {error}"))
}

#[tauri::command]
pub async fn edge_requeue_quarantined(
    event_id: String,
    state: tauri::State<'_, AppState>,
) -> Result<EdgeRequeueResult, String> {
    parse_requeue_reply(edge::requeue_edge_event(&state, &event_id).await?)
}

/// Read the sidecar's answer to a manual retry.
///
/// `requeued: false` means "no row moved" and is a normal answer — the row may
/// already have been retried elsewhere, it may belong to another identity, or
/// the edge may already be carrying it upstream in a digest. Those are three
/// different things to tell the operator, which is what `outcome` is for. A
/// *missing* field means the sidecar never answered the question, which must
/// not be shown to the operator as a failed retry.
fn parse_requeue_reply(value: serde_json::Value) -> Result<EdgeRequeueResult, String> {
    let requeued = value
        .get("requeued")
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(|| "edge requeue reply was unreadable".to_string())?;
    let outcome = value
        .get("outcome")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "edge requeue reply carried no outcome".to_string())?
        .to_string();
    // The sidecar derives the boolean from the outcome (`RequeueOutcome::
    // requeued()`), so the two can never legitimately disagree. If they do,
    // something rewrote one of them in transit, and picking a half to believe
    // would either report a refused retry as done or a done retry as refused —
    // the precise confusion `outcome` was added to end.
    if requeued != (outcome == REQUEUE_OUTCOME_REQUEUED) {
        return Err(format!(
            "edge requeue reply contradicts itself: requeued={requeued} with outcome '{outcome}'"
        ));
    }
    Ok(EdgeRequeueResult { requeued, outcome })
}

#[cfg(test)]
#[path = "edge_status_tests.rs"]
mod tests;
