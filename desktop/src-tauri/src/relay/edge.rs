//! Optional loopback edge routing for kind-9 channel messages.
//!
//! Phase-1 `buzz-edge` (SPEC-2026-08-05) puts a local sidecar in front of the
//! canonical relay for message traffic only. Everything here is inert unless
//! `BUZZ_EDGE_RELAY_URL` is set: an unset variable is exactly today's
//! canonical-only behavior, which is also the spec's first-line rollback.
//!
//! The active community lives in a module static rather than `AppState` for
//! the same reason `relay_admission` keeps its gate here: both are process-wide
//! workspace scope written by `apply_workspace`, and keeping edge state in the
//! edge module means the feature can be reverted without touching shared state.

use std::sync::Mutex;

use reqwest::Method;
use serde::Serialize;

use super::{
    build_nip98_auth_header, classify_request_error, parse_json_response, relay_error_message,
    relay_ws_url_with_override,
};
use crate::app_state::AppState;

/// Loopback requests are local; a slow sidecar must never outlast the
/// canonical path it is supposed to beat, so every edge call fails over fast.
const EDGE_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Active workspace community, set atomically with the relay override by
/// `apply_workspace`. `None` means no community is bound and edge routing is
/// off — the same fail-closed state as an unset `BUZZ_EDGE_RELAY_URL`.
static ACTIVE_COMMUNITY_ID: Mutex<Option<uuid::Uuid>> = Mutex::new(None);

/// Bind edge routing to a community. Called on every workspace apply, so a
/// community switch re-points the binding instead of leaving the old one live.
pub fn set_active_community(community_id: uuid::Uuid) {
    if let Ok(mut guard) = ACTIVE_COMMUNITY_ID.lock() {
        *guard = Some(community_id);
    }
}

fn active_community() -> Option<uuid::Uuid> {
    ACTIVE_COMMUNITY_ID.lock().ok()?.as_ref().copied()
}

/// The resolved `(edge endpoint, canonical origin, community)` triple a client
/// declares in the §14 community-binding handshake.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EdgeRelayBinding {
    pub relay_url: String,
    pub http_url: String,
    pub canonical_origin: String,
    pub community_id: String,
}

/// Resolve the optional Desktop edge route against the active workspace.
/// Invalid, incomplete, or non-loopback configuration is canonical-only.
pub fn edge_relay_binding(state: &AppState) -> Option<EdgeRelayBinding> {
    let edge_url = super::configured_env_var("BUZZ_EDGE_RELAY_URL")?;
    let community_id = active_community()?;

    let mut edge = url::Url::parse(&edge_url).ok()?;
    let loopback = match edge.host() {
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        Some(url::Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        None => false,
    };
    if !matches!(edge.scheme(), "ws" | "http")
        || !loopback
        || !edge.username().is_empty()
        || edge.password().is_some()
        || (edge.path() != "" && edge.path() != "/")
        || edge.query().is_some()
        || edge.fragment().is_some()
    {
        return None;
    }
    edge.set_path("");
    edge.set_scheme("ws").ok()?;
    let relay_url = edge.as_str().trim_end_matches('/').to_string();
    edge.set_scheme("http").ok()?;
    let http_url = edge.as_str().trim_end_matches('/').to_string();

    let mut canonical = url::Url::parse(&relay_ws_url_with_override(state)).ok()?;
    if !matches!(canonical.scheme(), "ws" | "wss")
        || canonical.host_str().is_none()
        || !canonical.username().is_empty()
        || canonical.password().is_some()
        || (canonical.path() != "" && canonical.path() != "/")
        || canonical.query().is_some()
        || canonical.fragment().is_some()
    {
        return None;
    }
    canonical.set_path("");

    Some(EdgeRelayBinding {
        relay_url,
        http_url,
        canonical_origin: canonical.as_str().trim_end_matches('/').to_string(),
        community_id: community_id.to_string(),
    })
}

/// True only for filters the sidecar is allowed to answer: exactly kind 9,
/// scoped to explicit channel UUIDs. Anything broader stays canonical, so a
/// wildcard or mixed-kind query can never be served from the local cache.
pub(super) fn filters_are_edge_message_only(filters: &[serde_json::Value]) -> bool {
    !filters.is_empty()
        && filters.iter().all(|filter| {
            let kinds = filter.get("kinds").and_then(serde_json::Value::as_array);
            let message_only = kinds.is_some_and(|kinds| {
                kinds.len() == 1
                    && kinds[0].as_u64() == Some(buzz_core_pkg::kind::KIND_STREAM_MESSAGE as u64)
            });
            let channels = filter.get("#h").and_then(serde_json::Value::as_array);
            let channel_scoped = channels.is_some_and(|channels| {
                !channels.is_empty()
                    && channels.iter().all(|channel| {
                        channel
                            .as_str()
                            .is_some_and(|value| uuid::Uuid::parse_str(value).is_ok())
                    })
            });
            message_only && channel_scoped
        })
}

/// Declare the bound pair on every edge request. The sidecar rejects a
/// mismatch, which is what makes a community switch fail closed rather than
/// reading another community's cache.
pub(super) fn with_binding_headers(
    request: reqwest::RequestBuilder,
    binding: &EdgeRelayBinding,
) -> reqwest::RequestBuilder {
    request
        .timeout(EDGE_REQUEST_TIMEOUT)
        .header("x-buzz-canonical-origin", &binding.canonical_origin)
        .header("x-buzz-community-id", &binding.community_id)
}

/// Single decision point for reads: answer from the sidecar only for
/// edge-eligible filters while a binding holds. `None` means "not routed here"
/// — including every edge failure, so the caller falls through to canonical
/// rather than surfacing a local-only error.
pub(super) async fn try_query(
    state: &AppState,
    filters: &[serde_json::Value],
) -> Option<Vec<nostr::Event>> {
    if !filters_are_edge_message_only(filters) {
        return None;
    }
    let binding = edge_relay_binding(state)?;
    query_edge_relay(state, &binding, filters).await.ok()
}

/// Single decision point for writes: kind-9 only, binding required, and any
/// edge failure falls through to canonical submission.
pub(super) async fn try_submit(
    event: &nostr::Event,
    state: &AppState,
    keys: &nostr::Keys,
    auth_tag: Option<&str>,
) -> Option<super::SubmitEventResponse> {
    if event.kind.as_u16() != buzz_core_pkg::kind::KIND_STREAM_MESSAGE as u16 {
        return None;
    }
    let binding = edge_relay_binding(state)?;
    super::submit::submit_signed_event_to_edge(event, state, keys, auth_tag, &binding)
        .await
        .ok()
}

async fn query_edge_relay(
    state: &AppState,
    binding: &EdgeRelayBinding,
    filters: &[serde_json::Value],
) -> Result<Vec<nostr::Event>, String> {
    let url = format!("{}/query", binding.http_url);
    let body = serde_json::to_vec(filters)
        .map_err(|error| format!("filter serialization failed: {error}"))?;
    let auth = build_nip98_auth_header(&Method::POST, &url, &body, state)?;
    let request = with_binding_headers(state.http_client.post(&url), binding)
        .header("Authorization", auth)
        .header("Content-Type", "application/json");
    let response = request
        .body(body)
        .send()
        .await
        .map_err(|error| classify_request_error(&error))?;
    if !response.status().is_success() {
        return Err(relay_error_message(response).await);
    }
    parse_json_response(response).await
}

/// Hand the edge route down to a managed agent, but only when the agent is
/// pointed at the same canonical relay this binding was derived from. An agent
/// on another relay must never inherit this community's sidecar.
pub fn apply_agent_env(
    command: &mut std::process::Command,
    state: &AppState,
    effective_relay_url: &str,
) {
    match edge_relay_binding(state)
        .filter(|binding| binding.canonical_origin == effective_relay_url.trim_end_matches('/'))
    {
        Some(binding) => {
            command.env("BUZZ_EDGE_RELAY_URL", binding.relay_url);
            command.env("BUZZ_COMMUNITY_ID", binding.community_id);
        }
        None => {
            command.env_remove("BUZZ_EDGE_RELAY_URL");
            command.env_remove("BUZZ_COMMUNITY_ID");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::filters_are_edge_message_only;

    #[test]
    fn edge_http_filter_requires_exact_kind_nine_and_uuid_channel() {
        let channel = uuid::Uuid::new_v4().to_string();
        assert!(filters_are_edge_message_only(&[serde_json::json!({
            "kinds": [9],
            "#h": [channel],
            "limit": 50
        })]));
        assert!(!filters_are_edge_message_only(&[serde_json::json!({
            "kinds": [9, 7],
            "#h": [channel]
        })]));
        assert!(!filters_are_edge_message_only(&[serde_json::json!({
            "kinds": [9],
            "#h": ["not-a-uuid"]
        })]));
    }

    #[test]
    fn empty_and_unscoped_filters_stay_canonical() {
        assert!(!filters_are_edge_message_only(&[]));
        assert!(!filters_are_edge_message_only(&[
            serde_json::json!({"kinds": [9]})
        ]));
        assert!(!filters_are_edge_message_only(&[
            serde_json::json!({"#h": [uuid::Uuid::new_v4().to_string()]})
        ]));
        assert!(!filters_are_edge_message_only(&[
            serde_json::json!({"kinds": [9], "#h": []})
        ]));
    }

    #[test]
    fn one_canonical_filter_disqualifies_the_whole_request() {
        let channel = uuid::Uuid::new_v4().to_string();
        assert!(!filters_are_edge_message_only(&[
            serde_json::json!({"kinds": [9], "#h": [channel]}),
            serde_json::json!({"kinds": [40099], "#h": [uuid::Uuid::new_v4().to_string()]}),
        ]));
    }
}
