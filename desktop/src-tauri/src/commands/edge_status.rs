//! Frontend access to the edge sidecar's delivery state (SPEC-2026-08-05 §11).
//!
//! Success criterion (3) of the spec requires the two delivery states —
//! **delivered locally** and **synced to canonical history** — to be labelled
//! separately everywhere they surface. These commands are what makes that
//! possible in the UI: they carry the sidecar's real outbox state, never a
//! guess derived from whether a send call returned.
//!
//! Every command fails with [`relay::edge::EDGE_UNAVAILABLE`] when no sidecar
//! is reachable. That is the normal state — edge routing is opt-in and off by
//! default — so the frontend treats it as "render nothing", not as an error.

use serde::{Deserialize, Serialize};

use crate::app_state::AppState;
use crate::relay::edge;

/// Quarantine rows returned when the caller does not ask for a specific page.
const DEFAULT_QUARANTINE_LIMIT: usize = 200;

/// Outbox counts behind the delivery labels.
///
/// `delivered_exact` and `delivered_via_digest` stay separate all the way to
/// the UI: an event replayed under its own ID and an event collapsed into an
/// edge-authored digest are both "synced", but they are not the same thing,
/// and the spec forbids merging them.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EdgeDeliverySummary {
    pending: u64,
    claimed: u64,
    delivered_exact: u64,
    delivered_via_digest: u64,
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
    updated_at: i64,
}

/// An identity with queued events and nobody online to push them upstream.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EdgeWaitingAuthor {
    author: String,
    pending: u64,
    oldest_pending_at: i64,
}

/// The whole status payload, parsed once.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EdgeStatusPayload {
    #[serde(default)]
    summary: EdgeDeliverySummary,
    #[serde(default)]
    quarantined: Vec<EdgeQuarantinedEvent>,
    #[serde(default)]
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
) -> Result<Vec<(String, String)>, String> {
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
) -> Result<bool, String> {
    parse_requeue_reply(edge::requeue_edge_event(&state, &event_id).await?)
}

/// Read the sidecar's answer to a manual retry.
///
/// `false` means "no row moved" and is a normal answer — the row may already
/// have been retried elsewhere, or it may belong to another identity. A
/// *missing* field means the sidecar never answered the question, which must
/// not be shown to the operator as a failed retry.
fn parse_requeue_reply(value: serde_json::Value) -> Result<bool, String> {
    value
        .get("requeued")
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(|| "edge requeue reply was unreadable".to_string())
}

#[cfg(test)]
#[path = "edge_status_tests.rs"]
mod tests;
