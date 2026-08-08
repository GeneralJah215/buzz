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
    let (relay_url, http_url) = resolve_edge_endpoint(&edge_url)?;
    let canonical_origin = resolve_canonical_origin(&relay_ws_url_with_override(state))?;

    Some(EdgeRelayBinding {
        relay_url,
        http_url,
        canonical_origin,
        community_id: community_id.to_string(),
    })
}

/// Reduce a configured edge URL to its `(ws, http)` origin pair, or `None` if
/// it is anything but a bare loopback origin.
///
/// This is deliberately strict. The sidecar's whole safety story is that it is
/// local-only, so a typo that points at a remote host, smuggles credentials,
/// or hides a path must resolve to canonical-only rather than quietly opening
/// a remote hop for message traffic.
fn resolve_edge_endpoint(edge_url: &str) -> Option<(String, String)> {
    let mut edge = url::Url::parse(edge_url).ok()?;
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
    Some((relay_url, edge.as_str().trim_end_matches('/').to_string()))
}

/// The canonical origin the client declares in the §14 handshake. A canonical
/// URL that is not a bare `ws`/`wss` origin yields `None`, which disables edge
/// routing — the binding must name exactly one unambiguous origin.
fn resolve_canonical_origin(canonical_url: &str) -> Option<String> {
    let mut canonical = url::Url::parse(canonical_url).ok()?;
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
    Some(canonical.as_str().trim_end_matches('/').to_string())
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

/// The exact error the status calls return when no sidecar is reachable.
///
/// The frontend keys off this string to stay silent instead of showing an
/// error banner. Edge routing is optional and off by default, so "not running"
/// is the normal state for most installs, not a fault worth reporting.
pub const EDGE_UNAVAILABLE: &str = "edge sidecar not running";

/// Read the operator-facing sync status from the sidecar.
///
/// Returns metadata only — counts, identifiers, failure reasons. The sidecar
/// deliberately does not include message content in this payload, and neither
/// should anything built on it.
pub async fn fetch_edge_status(
    state: &AppState,
    limit: usize,
) -> Result<serde_json::Value, String> {
    edge_post(state, "/status", serde_json::json!({ "limit": limit })).await
}

/// Ask the sidecar to move one quarantined event back to `pending`.
///
/// Which rows this can touch is decided by the NIP-98 identity on the request,
/// not by the body, so this can only ever retry the operator's own event.
pub async fn requeue_edge_event(
    state: &AppState,
    event_id: &str,
) -> Result<serde_json::Value, String> {
    edge_post(state, "/requeue", serde_json::json!({ "event_id": event_id })).await
}

/// Look up the delivery state of specific events for the message list.
pub async fn fetch_edge_delivery_states(
    state: &AppState,
    event_ids: &[String],
) -> Result<serde_json::Value, String> {
    edge_post(
        state,
        "/delivery-states",
        serde_json::json!({ "event_ids": event_ids }),
    )
    .await
}

/// Shared POST path for the status routes.
///
/// Unlike `try_query`/`try_submit`, a failure here is surfaced rather than
/// swallowed: there is no canonical fallback for sidecar status, and silently
/// returning empty state would tell the operator "nothing is stuck" when the
/// truth is "nobody asked".
async fn edge_post(
    state: &AppState,
    path: &str,
    body: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let binding = edge_relay_binding(state).ok_or_else(|| EDGE_UNAVAILABLE.to_string())?;
    let url = format!("{}{path}", binding.http_url);
    let body = serde_json::to_vec(&body)
        .map_err(|error| format!("edge request serialization failed: {error}"))?;
    let auth = build_nip98_auth_header(&Method::POST, &url, &body, state)?;
    let response = with_binding_headers(state.http_client.post(&url), &binding)
        .header("Authorization", auth)
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|_| EDGE_UNAVAILABLE.to_string())?;
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
    use super::{filters_are_edge_message_only, resolve_canonical_origin, resolve_edge_endpoint};

    #[test]
    fn loopback_edge_urls_resolve_to_a_ws_and_http_origin_pair() {
        for accepted in [
            "ws://127.0.0.1:7777",
            "ws://127.0.0.1:7777/",
            "http://127.0.0.1:7777",
            "http://localhost:7777",
            "http://LOCALHOST:7777",
            "ws://[::1]:7777",
        ] {
            let (relay, http) =
                resolve_edge_endpoint(accepted).unwrap_or_else(|| panic!("{accepted}"));
            assert!(relay.starts_with("ws://"), "{accepted} -> {relay}");
            assert!(http.starts_with("http://"), "{accepted} -> {http}");
            assert!(!relay.ends_with('/'), "{accepted} -> {relay}");
        }
    }

    /// Everything that is not a bare loopback origin must disable edge routing.
    /// A mistake here would send message traffic somewhere it was never meant
    /// to go, so each rejection is asserted individually.
    #[test]
    fn non_loopback_or_decorated_edge_urls_are_refused() {
        for rejected in [
            "ws://example.com:7777",         // remote host
            "wss://127.0.0.1:7777",          // tls scheme is not an edge scheme
            "https://127.0.0.1:7777",        // same
            "ws://8.8.8.8:7777",             // public address
            "ws://user@127.0.0.1:7777",      // credentials
            "ws://user:pass@127.0.0.1:7777", // credentials
            "ws://127.0.0.1:7777/path",      // path
            "ws://127.0.0.1:7777/?a=b",      // query
            "ws://127.0.0.1:7777/#frag",     // fragment
            "file:///tmp/socket",            // wrong scheme entirely
            "not a url",
            "",
        ] {
            assert!(
                resolve_edge_endpoint(rejected).is_none(),
                "{rejected} must not resolve to an edge endpoint"
            );
        }
    }

    #[test]
    fn canonical_origin_keeps_only_a_bare_ws_origin() {
        assert_eq!(
            resolve_canonical_origin("wss://relay.example.com/").as_deref(),
            Some("wss://relay.example.com")
        );
        assert_eq!(
            resolve_canonical_origin("ws://relay.example.com:8080").as_deref(),
            Some("ws://relay.example.com:8080")
        );
        for rejected in [
            "https://relay.example.com",
            "wss://relay.example.com/path",
            "wss://relay.example.com/?a=b",
            "wss://user@relay.example.com",
            "not a url",
        ] {
            assert!(
                resolve_canonical_origin(rejected).is_none(),
                "{rejected} must not become a canonical origin"
            );
        }
    }

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
