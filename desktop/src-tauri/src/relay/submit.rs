use super::*;

/// Response from `POST /events`.
#[derive(Debug, Deserialize, serde::Serialize)]
pub struct SubmitEventResponse {
    pub event_id: String,
    pub accepted: bool,
    pub message: String,
}

/// POST an already-signed event to an explicit relay with an explicit owner.
///
/// Deferred/scoped publication uses this form so a workspace or identity
/// switch cannot retarget either the event or its NIP-98 authentication after
/// the operation captured its `(relay, owner)` scope.
pub async fn submit_signed_event_at_with_keys(
    event: &nostr::Event,
    state: &AppState,
    api_base_url: &str,
    keys: &nostr::Keys,
) -> Result<SubmitEventResponse, String> {
    if event.pubkey != keys.public_key() {
        return Err("signed event does not match the publishing identity".to_string());
    }
    crate::relay_admission::wait_for_rate_limit().await;
    let url = format!("{}/events", api_base_url.trim_end_matches('/'));
    let body_bytes = event.as_json().into_bytes();
    crate::egress_guard::assert_no_key_backup_bytes(&body_bytes, "relay event submit")?;
    let auth_header = build_nip98_auth_header_for_keys(keys, &Method::POST, &url, &body_bytes)?;

    let response = state
        .http_client
        .post(&url)
        .header("Authorization", auth_header)
        .header("Content-Type", "application/json")
        .body(body_bytes)
        .send()
        .await
        .map_err(|e| classify_request_error(&e))?;

    if !response.status().is_success() {
        return Err(relay_error_message(response).await);
    }

    let result: SubmitEventResponse = parse_json_response(response).await?;
    if !result.accepted {
        return Err(format!("relay rejected event: {}", result.message));
    }

    Ok(result)
}

/// Sign with an explicit identity and POST the event to an explicit relay.
///
/// The caller owns the signer lifetime. This is important for deferred work:
/// an in-process identity swap cannot retarget the event or its NIP-98 auth
/// after the caller has validated which identity the operation belongs to.
pub async fn submit_event_at_with_keys(
    builder: nostr::EventBuilder,
    state: &AppState,
    api_base_url: &str,
    keys: &nostr::Keys,
) -> Result<SubmitEventResponse, String> {
    let event = builder
        .sign_with_keys(keys)
        .map_err(|e| format!("failed to sign event: {e}"))?;
    submit_signed_event_at_with_keys(&event, state, api_base_url, keys).await
}

/// Build and submit an event to the currently active workspace relay.
pub async fn submit_event(
    builder: nostr::EventBuilder,
    state: &AppState,
) -> Result<SubmitEventResponse, String> {
    let api_base_url = relay_api_base_url_with_override(state);
    let keys = state.signing_keys()?;
    let event = builder
        .sign_with_keys(&keys)
        .map_err(|e| format!("failed to sign event: {e}"))?;
    if let Some(response) = super::edge::try_submit(&event, state, &keys, None).await {
        return Ok(response);
    }
    submit_signed_event_at_with_keys(&event, state, &api_base_url, &keys).await
}

pub(super) async fn submit_signed_event_to_edge(
    event: &nostr::Event,
    state: &AppState,
    keys: &nostr::Keys,
    auth_tag: Option<&str>,
    binding: &EdgeRelayBinding,
) -> Result<SubmitEventResponse, String> {
    if event.pubkey != keys.public_key() {
        return Err("signed event does not match the publishing identity".to_string());
    }
    let url = format!("{}/events", binding.http_url);
    let body = event.as_json().into_bytes();
    crate::egress_guard::assert_no_key_backup_bytes(&body, "edge event submit")?;
    let auth = build_nip98_auth_header_for_keys(keys, &Method::POST, &url, &body)?;
    let mut request = super::edge::with_binding_headers(state.http_client.post(&url), binding)
        .header("Authorization", auth)
        .header("Content-Type", "application/json");
    if let Some(tag) = auth_tag {
        request = request.header("x-auth-tag", tag);
    }
    let response = request
        .body(body)
        .send()
        .await
        .map_err(|error| classify_request_error(&error))?;
    if !response.status().is_success() {
        return Err(relay_error_message(response).await);
    }
    let result: SubmitEventResponse = parse_json_response(response).await?;
    if !result.accepted {
        return Err(format!("relay rejected event: {}", result.message));
    }
    Ok(result)
}

#[cfg(test)]
mod edge_egress_tests {
    use super::*;
    use crate::relay::EdgeRelayBinding;

    /// NIP-49 spec vector, identical to the one in `egress_guard_tests.rs`.
    const NCRYPTSEC: &str = "ncryptsec1qgg9947rlpvqu76pj5ecreduf9jxhselq2nae2kghhvd5g7dgjtcxfqtd67p9m0w57lspw8gsq6yphnm8623nsl8xn9j4jdzz84zm3frztj3z7s35vpzmqf6ksu8r89qk5z2zxfmu5gv8th8wclt0h4p";

    /// Boundary 9 injection test: the loopback edge submit path. The guard must
    /// abort before any network I/O — the discard port has no listener, so a
    /// guard error rather than a connection error proves the abort came first.
    /// Lives here, next to the boundary, because the function is module-private
    /// and this file is already on the NIP-49 handling allowlist.
    #[tokio::test]
    async fn edge_submit_blocks_ncryptsec() {
        let state = crate::app_state::build_app_state();
        let keys = nostr::Keys::generate();
        let event = nostr::EventBuilder::new(nostr::Kind::Custom(9), NCRYPTSEC)
            .sign_with_keys(&keys)
            .unwrap();
        let binding = EdgeRelayBinding {
            relay_url: "ws://127.0.0.1:9".to_string(),
            http_url: "http://127.0.0.1:9".to_string(),
            canonical_origin: "wss://relay.invalid".to_string(),
            community_id: uuid::Uuid::new_v4().to_string(),
        };
        let error = submit_signed_event_to_edge(&event, &state, &keys, None, &binding)
            .await
            .unwrap_err();
        assert!(
            error.contains("key-backup material"),
            "expected the egress-guard error, got: {error}"
        );
        assert!(
            error.contains("edge event submit"),
            "guard must name its boundary, got: {error}"
        );
    }
}
