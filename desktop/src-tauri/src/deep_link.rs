use std::{collections::VecDeque, sync::Mutex};

use serde::Serialize;
use tauri::{Emitter, Manager, State};
use url::Url;

use crate::nostr_bind;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PendingCommunityDeepLink {
    id: String,
    kind: String,
    relay_url: String,
    code: Option<String>,
    policy_receipt: Option<String>,
    name: Option<String>,
}

#[derive(Default)]
pub(crate) struct PendingCommunityDeepLinks(Mutex<VecDeque<PendingCommunityDeepLink>>);

impl PendingCommunityDeepLinks {
    fn enqueue(&self, pending: PendingCommunityDeepLink) {
        let mut queue = self.0.lock().expect("pending deep-link queue poisoned");
        if queue.iter().any(|item| {
            item.kind == pending.kind
                && item.relay_url == pending.relay_url
                && item.code == pending.code
                && item.policy_receipt == pending.policy_receipt
                && item.name == pending.name
        }) {
            return;
        }
        queue.push_back(pending);
    }

    fn first(&self) -> Option<PendingCommunityDeepLink> {
        self.0
            .lock()
            .expect("pending deep-link queue poisoned")
            .front()
            .cloned()
    }

    fn acknowledge(&self, id: &str) -> bool {
        let mut queue = self.0.lock().expect("pending deep-link queue poisoned");
        if queue.front().is_some_and(|item| item.id == id) {
            queue.pop_front();
            true
        } else {
            false
        }
    }
}

#[tauri::command]
pub(crate) fn take_pending_community_deep_link(
    pending: State<'_, PendingCommunityDeepLinks>,
) -> Option<PendingCommunityDeepLink> {
    pending.first()
}

#[tauri::command]
pub(crate) fn acknowledge_pending_community_deep_link(
    id: String,
    pending: State<'_, PendingCommunityDeepLinks>,
) -> bool {
    pending.acknowledge(&id)
}

fn queue_community_deep_link(
    app: &tauri::AppHandle,
    kind: &str,
    relay_url: String,
    code: Option<String>,
    policy_receipt: Option<String>,
    name: Option<String>,
) {
    app.state::<PendingCommunityDeepLinks>()
        .enqueue(PendingCommunityDeepLink {
            id: uuid::Uuid::new_v4().to_string(),
            kind: kind.to_owned(),
            relay_url,
            code,
            policy_receipt,
            name,
        });
}

fn activate_main_window(app: &tauri::AppHandle) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };

    if let Err(error) = window.unminimize() {
        eprintln!("buzz-desktop: failed to unminimize main window for deep link: {error}");
    }
    if let Err(error) = window.show() {
        eprintln!("buzz-desktop: failed to show main window for deep link: {error}");
    }
    if let Err(error) = window.set_focus() {
        eprintln!("buzz-desktop: failed to focus main window for deep link: {error}");
    }
}

/// Parse the query string of a `buzz://message?…` URL into the JSON
/// payload emitted on `deep-link-message`. Returns `None` when a required
/// param (`channel`, `id`) is missing or empty — mirroring the validation
/// policy of the `connect` arm so the frontend never sees a half-formed
/// payload (e.g. `channelId: ""` from `channel=&id=foo`).
///
/// Pulled out of `handle_deep_link_url` so it can be unit-tested without
/// a live `tauri::AppHandle`.
fn parse_message_deep_link(url: &Url) -> Option<serde_json::Value> {
    let mut channel: Option<String> = None;
    let mut message_id: Option<String> = None;
    let mut thread: Option<String> = None;
    for (k, v) in url.query_pairs() {
        let v = v.into_owned();
        if v.is_empty() {
            continue;
        }
        match k.as_ref() {
            "channel" => channel = Some(v),
            "id" => message_id = Some(v),
            "thread" => thread = Some(v),
            _ => {}
        }
    }
    let (channel_id, message_id) = (channel?, message_id?);
    Some(serde_json::json!({
        "channelId": channel_id,
        "messageId": message_id,
        "threadRootId": thread,
    }))
}

/// Parse the query string of a `buzz://join?…` URL into the JSON payload
/// emitted on `deep-link-join`. Requires a ws(s) `relay` URL and a non-empty
/// `code`; returns `None` otherwise so the frontend never sees a half-formed
/// payload.
fn parse_join_deep_link(url: &Url) -> Option<serde_json::Value> {
    let mut code: Option<String> = None;
    let mut policy_receipt: Option<String> = None;
    for (k, v) in url.query_pairs() {
        let v = v.into_owned();
        if v.is_empty() {
            continue;
        }
        match k.as_ref() {
            "code" => code = Some(v),
            "policy_receipt" => policy_receipt = Some(v),
            _ => {}
        }
    }
    let code = code?;
    let relay_url = parse_websocket_relay_param(url)?;
    Some(serde_json::json!({
        "relayUrl": relay_url,
        "code": code,
        "policyReceipt": policy_receipt,
    }))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct AddCommunityDeepLinkPayload {
    relay_url: String,
    name: Option<String>,
}

fn parse_websocket_relay_param(url: &Url) -> Option<String> {
    let relay_url = url
        .query_pairs()
        .find(|(key, _)| key == "relay")
        .map(|(_, value)| value.into_owned())
        .filter(|value| !value.is_empty())?;
    let parsed = Url::parse(&relay_url).ok()?;
    if !matches!(parsed.scheme(), "ws" | "wss") || parsed.host_str().is_none() {
        return None;
    }
    Some(relay_url)
}

fn parse_add_community_deep_link(url: &Url) -> Option<AddCommunityDeepLinkPayload> {
    Some(AddCommunityDeepLinkPayload {
        relay_url: parse_websocket_relay_param(url)?,
        name: optional_non_empty_param(url, "name"),
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct NostrBindDeepLinkPayload {
    challenge_id: String,
    nonce: String,
    verification_code: String,
    audience: String,
    action: String,
    protocol: String,
    version: String,
    origin: String,
    expires_at: String,
    return_mode: String,
    callback_url: Option<String>,
}

fn non_empty_param(url: &Url, name: &str) -> Result<String, String> {
    url.query_pairs()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("missing {name}"))
}

fn optional_non_empty_param(url: &Url, name: &str) -> Option<String> {
    url.query_pairs()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
        .filter(|value| !value.is_empty())
}

fn validate_nostr_bind_callback_url(callback_url: &str, origin: &str) -> Result<(), String> {
    let callback =
        Url::parse(callback_url).map_err(|error| format!("invalid callback_url: {error}"))?;
    let origin = Url::parse(origin).map_err(|error| format!("invalid origin: {error}"))?;
    if callback.scheme() != "https" {
        return Err("callback_url must use https".into());
    }
    if callback.host_str().is_none() {
        return Err("callback_url missing host".into());
    }
    if !callback.username().is_empty() || callback.password().is_some() {
        return Err("callback_url must not include credentials".into());
    }
    if callback.scheme() != origin.scheme()
        || callback.host_str() != origin.host_str()
        || callback.port_or_known_default() != origin.port_or_known_default()
    {
        return Err("callback_url must match origin".into());
    }
    Ok(())
}

fn parse_nostr_bind_deep_link(url: &Url) -> Result<NostrBindDeepLinkPayload, String> {
    let challenge_id = non_empty_param(url, "challenge_id")?;
    let nonce = non_empty_param(url, "nonce")?;
    let verification_code = non_empty_param(url, "verification_code")?;
    let audience = non_empty_param(url, "audience")?;
    let action = non_empty_param(url, "action")?;
    let protocol = non_empty_param(url, "protocol")?;
    let version = non_empty_param(url, "version")?;
    let origin = non_empty_param(url, "origin")?;
    let expires_at = non_empty_param(url, "expires_at")?;
    let return_mode = non_empty_param(url, "return")?;
    let callback_url = optional_non_empty_param(url, "callback_url");

    nostr_bind::validate_challenge_id(&challenge_id)?;
    nostr_bind::validate_nonce(&nonce)?;
    nostr_bind::validate_verification_code(&verification_code)?;
    nostr_bind::validate_protocol_fields(&audience, &action, &protocol, &version)?;
    nostr_bind::validate_origin(&origin)?;
    // Expired links still reach the consent surface so the user gets an explicit
    // failure instead of a silent stderr-only rejection from a launched app.
    nostr_bind::validate_expires_at_format(&expires_at)?;
    match return_mode.as_str() {
        nostr_bind::RETURN_MODE_CLIPBOARD => {}
        nostr_bind::RETURN_MODE_BROWSER_FRAGMENT_V1 if callback_url.is_some() => {}
        nostr_bind::RETURN_MODE_BROWSER_FRAGMENT_V1 => {
            return Err("browser_fragment_v1 requires callback_url".into());
        }
        _ => return Err("unsupported return mode".into()),
    }
    if let Some(callback_url) = callback_url.as_deref() {
        validate_nostr_bind_callback_url(callback_url, &origin)?;
    }

    Ok(NostrBindDeepLinkPayload {
        challenge_id,
        nonce,
        verification_code,
        audience,
        action,
        protocol,
        version,
        origin,
        expires_at,
        return_mode,
        callback_url,
    })
}

/// A validated `buzz://restart-agent?…` request.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RestartAgentRequest {
    /// Lower-cased 64-hex agent pubkey.
    pubkey: String,
    /// Presented control token. Never logged.
    token: String,
    /// Restart only this pair when set; otherwise every live pair.
    relay_url: Option<String>,
}

/// Parse `buzz://restart-agent?pubkey=<64 hex>&token=<token>[&relay=<ws(s)://…>]`.
///
/// Validation is strict and happens before the token is even looked at: the
/// pubkey must be exactly 64 hex characters (the managed-agent key shape, see
/// `ManagedAgentRuntimeKey::new`) so a malformed link can never reach the
/// runtime registry, and an unparseable `relay` is an error rather than a
/// silent fall-through to "restart everything".
///
/// Uppercase pubkeys are normalised rather than rejected: relay tooling and
/// nostr clients disagree on case, and the runtime keys itself lower-case.
///
/// Pure so it can be unit-tested without a live `tauri::AppHandle`.
fn parse_restart_agent_deep_link(url: &Url) -> Result<RestartAgentRequest, String> {
    let pubkey = non_empty_param(url, "pubkey")?.to_ascii_lowercase();
    if pubkey.len() != 64 || !pubkey.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("pubkey must be 64 hexadecimal characters".into());
    }
    let token = non_empty_param(url, "token")?;
    let relay_url = match optional_non_empty_param(url, "relay") {
        Some(_) => Some(
            parse_websocket_relay_param(url)
                .ok_or_else(|| "relay must be a ws:// or wss:// URL".to_string())?,
        ),
        None => None,
    };
    Ok(RestartAgentRequest {
        pubkey,
        token,
        relay_url,
    })
}

/// Carry out an authenticated `restart-agent` request.
///
/// Two distinct shapes, because "restart" means different things depending on
/// whether anything is running:
/// - one or more live pairs — stop+start each of them in place, off the async
///   executor since `restart_managed_agent_runtime` blocks on process teardown;
/// - no live pair at all (the common real-world case: the harness already died,
///   which is *why* something is asking for a restart) — there is nothing to
///   stop, so take the same start path as the UI's start button, preflight and
///   persona re-snapshot included.
async fn run_restart_agent_deep_link(app: tauri::AppHandle, request: RestartAgentRequest) {
    let prefix: String = request.pubkey.chars().take(8).collect();
    // Every later branch logs, yet a real restart produced nothing at all, so
    // the task was stalling before reaching any of them. These two bracket the
    // registry lock — a blocking std mutex taken on an async worker — which is
    // the only thing between entry and the first logged branch.
    tracing::info!(
        event = "restart_agent_task_entered",
        agent = %prefix,
        "restart task started"
    );

    let relay_urls: Vec<String> = match request.relay_url.clone() {
        Some(relay_url) => vec![relay_url],
        None => {
            let state = app.state::<crate::app_state::AppState>();
            let Ok(runtimes) = state.managed_agent_processes.lock() else {
                tracing::error!(
                    event = "restart_agent_registry_unavailable",
                    agent = %prefix,
                    "managed-agent runtime registry unavailable"
                );
                return;
            };
            crate::managed_agents::managed_agent_runtime_keys(&runtimes, &request.pubkey)
                .into_iter()
                .map(|key| key.relay_url)
                .collect()
        }
    };
    tracing::info!(
        event = "restart_agent_resolved_relays",
        agent = %prefix,
        relays = relay_urls.len(),
        "resolved relays for restart"
    );

    if relay_urls.is_empty() {
        let state = app.state::<crate::app_state::AppState>();
        match crate::commands::start_managed_agent(request.pubkey.clone(), app.clone(), state).await
        {
            Ok(_) => tracing::info!(
                event = "restart_agent_started",
                agent = %prefix,
                "agent started from restart deep link (no live pair)"
            ),
            Err(error) => {
                tracing::error!(
                    event = "restart_agent_start_failed",
                    agent = %prefix,
                    error = %error,
                    "agent start failed for restart deep link"
                );
            }
        }
        return;
    }

    for relay_url in relay_urls {
        let restart_app = app.clone();
        let pubkey = request.pubkey.clone();
        let logged_relay = relay_url.clone();
        let outcome = tauri::async_runtime::spawn_blocking(move || {
            crate::managed_agents::restart_managed_agent_runtime(pubkey, relay_url, restart_app)
        })
        .await;
        match outcome {
            Ok(Ok(status)) => tracing::info!(
                event = "restart_agent_restarted",
                agent = %prefix,
                relay = %logged_relay,
                pid = ?status.pid,
                "agent restarted"
            ),
            Ok(Err(error)) => tracing::error!(
                event = "restart_agent_failed",
                agent = %prefix,
                relay = %logged_relay,
                error = %error,
                "agent restart failed"
            ),
            Err(error) => tracing::error!(
                event = "restart_agent_task_failed",
                agent = %prefix,
                relay = %logged_relay,
                error = %error,
                "agent restart task failed"
            ),
        }
    }
}

/// A validated `buzz://delete-agent?…` request.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DeleteAgentRequest {
    /// Lower-cased 64-hex agent pubkey.
    pubkey: String,
    /// Presented control token. Never logged.
    token: String,
}

/// Parse `buzz://delete-agent?pubkey=<64 hex>&token=<token>`.
///
/// Same strict pubkey shape as `parse_restart_agent_deep_link`, and narrower
/// on purpose: deletion is irreversible (the agent's secret is discarded on
/// delete), so there is no bulk form and no optional parameters — one link
/// deletes exactly one pubkey.
///
/// Pure so it can be unit-tested without a live `tauri::AppHandle`.
fn parse_delete_agent_deep_link(url: &Url) -> Result<DeleteAgentRequest, String> {
    let pubkey = non_empty_param(url, "pubkey")?.to_ascii_lowercase();
    if pubkey.len() != 64 || !pubkey.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("pubkey must be 64 hexadecimal characters".into());
    }
    let token = non_empty_param(url, "token")?;
    Ok(DeleteAgentRequest { pubkey, token })
}

/// Carry out an authenticated `delete-agent` request.
///
/// Delegates to the same `delete_managed_agent` command the UI's delete
/// button invokes, so every side effect stays on one path: stop the live
/// process, remove the record, discard the keyring secret, publish the
/// NIP-09 tombstone, and file the NIP-IA archive request.
///
/// `force_remote_delete` is deliberately `None`: a deployed remote agent is
/// refused by the backend guard rather than orphaned. Only the UI's
/// interactive confirm may override that — a fire-and-forget link may not.
async fn run_delete_agent_deep_link(app: tauri::AppHandle, request: DeleteAgentRequest) {
    let prefix: String = request.pubkey.chars().take(8).collect();
    tracing::info!(
        event = "delete_agent_task_entered",
        agent = %prefix,
        "delete task started"
    );
    match crate::commands::delete_managed_agent(request.pubkey.clone(), None, app).await {
        Ok(()) => tracing::info!(
            event = "delete_agent_deleted",
            agent = %prefix,
            "agent deleted from delete deep link"
        ),
        Err(error) => tracing::error!(
            event = "delete_agent_failed",
            agent = %prefix,
            error = %error,
            "agent delete failed"
        ),
    }
}

/// Handle an incoming `buzz://` deep link URL.
///
/// Currently supports:
/// - `buzz://connect?relay=<ws(s)://...>` — emits `deep-link-connect` to the frontend
pub(crate) fn handle_deep_link_url(app: &tauri::AppHandle, url_str: &str) {
    // Proves a deep link actually reached the running instance. Without this,
    // "the restart did nothing" cannot be told apart from "the link never
    // arrived" — which is exactly where the 1,316 silent failures sat.
    // The action only; the query string carries the control token.
    tracing::info!(
        event = "deep_link_received",
        action = Url::parse(url_str)
            .ok()
            .and_then(|u| u.host_str().map(str::to_owned))
            .unwrap_or_else(|| "<unparsable>".to_owned()),
        "deep link received"
    );
    let url = match Url::parse(url_str) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("buzz-desktop: invalid deep link URL {url_str:?}: {e}");
            return;
        }
    };

    if url.scheme() != "buzz" {
        eprintln!("buzz-desktop: ignoring unsupported deep link scheme: {url_str}");
        return;
    }

    match url.host_str() {
        Some("connect") => {
            let Some(relay_url) = parse_websocket_relay_param(&url) else {
                eprintln!("buzz-desktop: connect deep link missing/invalid relay: {url_str}");
                return;
            };
            activate_main_window(app);
            queue_community_deep_link(app, "connect", relay_url.clone(), None, None, None);
            let _ = app.emit("deep-link-connect", relay_url);
        }
        Some("join") => {
            // `buzz://join?relay=<ws(s)://...>&code=<invite code>` — fired by
            // the relay's /invite/<code> landing page. The frontend claims the
            // invite against the relay's HTTP API, then adds the workspace.
            let Some(payload) = parse_join_deep_link(&url) else {
                eprintln!("buzz-desktop: join deep link missing/invalid relay or code: {url_str}");
                return;
            };
            activate_main_window(app);
            let relay_url = payload["relayUrl"].as_str().unwrap_or_default().to_owned();
            let code = payload["code"].as_str().map(str::to_owned);
            let policy_receipt = payload["policyReceipt"].as_str().map(str::to_owned);
            queue_community_deep_link(app, "join", relay_url, code, policy_receipt, None);
            let _ = app.emit("deep-link-join", payload);
        }
        Some("add-community") => {
            let Some(payload) = parse_add_community_deep_link(&url) else {
                eprintln!("buzz-desktop: add-community deep link missing/invalid relay: {url_str}");
                return;
            };
            activate_main_window(app);
            queue_community_deep_link(
                app,
                "add-community",
                payload.relay_url.clone(),
                None,
                None,
                payload.name.clone(),
            );
            let _ = app.emit("deep-link-add-community", payload);
        }
        Some("message") => {
            // `buzz://message?channel=<uuid>&id=<eventId>[&thread=<rootId>]`
            //
            // Validation policy mirrors the `connect` arm: parse what we
            // need, refuse to emit anything if a required param is missing
            // so the frontend never sees a half-formed payload. The
            // frontend listener mirrors `parseMessageLink` in TS — we keep
            // structure on this side (serde JSON) and let the TS code own
            // any further normalisation.
            let Some(payload) = parse_message_deep_link(&url) else {
                eprintln!("buzz-desktop: message deep link missing channel or id: {url_str}");
                return;
            };
            activate_main_window(app);
            let _ = app.emit("deep-link-message", payload);
        }
        Some("nostr-bind") => match parse_nostr_bind_deep_link(&url) {
            Ok(payload) => {
                activate_main_window(app);
                let _ = app.emit("deep-link-nostr-bind", payload);
            }
            Err(error) => {
                eprintln!("buzz-desktop: rejecting nostr-bind deep link: {error}: {url_str}");
            }
        },
        Some("restart-agent") => {
            // `buzz://restart-agent?pubkey=<64 hex>&token=<control token>[&relay=<ws(s)://…>]`
            //
            // A machine-to-machine control link, not a user-facing one: a
            // supervisor script fires it when an agent's harness has gone
            // quiet. So unlike every arm above it neither activates the main
            // window nor emits to the frontend — a background restart must not
            // steal focus from whatever the user is doing.
            //
            // Authentication is the local control-token file, checked before
            // any work is scheduled. A bad token gets a log line and nothing
            // else: no dialog, no window, no distinguishable timing — an
            // unauthenticated caller learns nothing, not even whether the
            // named agent exists. The presented token itself is never logged.
            let request = match parse_restart_agent_deep_link(&url) {
                Ok(request) => request,
                Err(error) => {
                    tracing::warn!(
                        event = "restart_agent_rejected",
                        error = %error,
                        "rejecting malformed restart-agent deep link"
                    );
                    return;
                }
            };
            if !crate::managed_agents::control_token::verify_control_token(app, &request.token) {
                let prefix: String = request.pubkey.chars().take(8).collect();
                tracing::warn!(
                    event = "restart_agent_unauthenticated",
                    agent = %prefix,
                    "rejecting restart-agent deep link with invalid control token"
                );
                return;
            }
            // The restart itself is slow (process teardown, relay preflight)
            // and this runs on the main thread, so hand it off and return.
            let restart_app = app.clone();
            tauri::async_runtime::spawn(run_restart_agent_deep_link(restart_app, request));
        }
        Some("delete-agent") => {
            // `buzz://delete-agent?pubkey=<64 hex>&token=<control token>`
            //
            // Machine-to-machine like `restart-agent` above, with the same
            // no-window, no-focus contract and the same authentication: the
            // local control-token file, checked before any work is scheduled.
            // A bad token gets a log line and nothing else, and the presented
            // token is never logged.
            //
            // Unlike a restart this is irreversible — the agent's secret is
            // discarded and the pubkey can never sign again — so the arm stays
            // deliberately narrow: exactly one pubkey per link, no relay
            // scoping, no force flags.
            let request = match parse_delete_agent_deep_link(&url) {
                Ok(request) => request,
                Err(error) => {
                    tracing::warn!(
                        event = "delete_agent_rejected",
                        error = %error,
                        "rejecting malformed delete-agent deep link"
                    );
                    return;
                }
            };
            if !crate::managed_agents::control_token::verify_control_token(app, &request.token) {
                let prefix: String = request.pubkey.chars().take(8).collect();
                tracing::warn!(
                    event = "delete_agent_unauthenticated",
                    agent = %prefix,
                    "rejecting delete-agent deep link with invalid control token"
                );
                return;
            }
            // Deletion blocks on process teardown, disk writes, and the
            // keyring; hand it off the main thread and return.
            let delete_app = app.clone();
            tauri::async_runtime::spawn(run_delete_agent_deep_link(delete_app, request));
        }
        Some(action) => {
            eprintln!("buzz-desktop: unknown deep link action: {action}");
        }
        None => {
            eprintln!("buzz-desktop: deep link missing action: {url_str}");
        }
    }
}

#[cfg(test)]
mod tests {
    use url::Url;

    use super::{
        parse_add_community_deep_link, parse_delete_agent_deep_link, parse_join_deep_link,
        parse_message_deep_link, parse_nostr_bind_deep_link, parse_restart_agent_deep_link,
        PendingCommunityDeepLink, PendingCommunityDeepLinks,
    };

    const AGENT_PUBKEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn pending(id: &str, relay_url: &str, code: Option<&str>) -> PendingCommunityDeepLink {
        PendingCommunityDeepLink {
            id: id.to_owned(),
            kind: if code.is_some() { "join" } else { "connect" }.to_owned(),
            relay_url: relay_url.to_owned(),
            code: code.map(str::to_owned),
            policy_receipt: None,
            name: None,
        }
    }

    #[test]
    fn pending_join_serializes_policy_receipt_for_cold_launch_recovery() {
        let mut link = pending("join", "wss://relay.example", Some("invite"));
        link.policy_receipt = Some("relay-signed-receipt".to_owned());

        let payload = serde_json::to_value(link).unwrap();
        assert_eq!(payload["policyReceipt"], "relay-signed-receipt");
    }

    #[test]
    fn pending_community_links_are_fifo_and_acknowledged_in_order() {
        let queue = PendingCommunityDeepLinks::default();
        queue.enqueue(pending("first", "wss://one.example", Some("one")));
        queue.enqueue(pending("second", "wss://two.example", Some("two")));
        assert_eq!(queue.first().unwrap().id, "first");
        assert!(!queue.acknowledge("second"));
        assert!(queue.acknowledge("first"));
        assert_eq!(queue.first().unwrap().id, "second");
    }

    #[test]
    fn pending_community_links_dedupe_exact_intents() {
        let queue = PendingCommunityDeepLinks::default();
        queue.enqueue(pending("first", "wss://one.example", Some("one")));
        queue.enqueue(pending("duplicate", "wss://one.example", Some("one")));
        assert!(queue.acknowledge("first"));
        assert!(queue.first().is_none());
    }

    fn valid_nostr_bind_url() -> Url {
        Url::parse(
            "buzz://nostr-bind?challenge_id=550e8400-e29b-41d4-a716-446655440000&nonce=ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghi01234567&verification_code=123456&audience=buzz%3Anostr-identity&action=bind_nostr_identity&protocol=buzz-nostr-identity&version=1&origin=https%3A%2F%2Fexample.com&expires_at=2999-01-01T00%3A00%3A00Z&return=clipboard",
        )
        .unwrap()
    }

    #[test]
    fn parse_add_community_deep_link_extracts_relay_and_name() {
        let url = Url::parse(
            "buzz://add-community?relay=wss%3A%2F%2Facme.communities.buzz.xyz&name=Acme%20Team&ignored=value",
        )
        .unwrap();
        let payload = parse_add_community_deep_link(&url).unwrap();
        assert_eq!(payload.relay_url, "wss://acme.communities.buzz.xyz");
        assert_eq!(payload.name.as_deref(), Some("Acme Team"));
    }

    #[test]
    fn parse_add_community_deep_link_accepts_an_omitted_or_empty_name() {
        for raw in [
            "buzz://add-community?relay=wss%3A%2F%2Facme.example",
            "buzz://add-community?relay=wss%3A%2F%2Facme.example&name=",
        ] {
            assert!(parse_add_community_deep_link(&Url::parse(raw).unwrap())
                .unwrap()
                .name
                .is_none());
        }
    }

    #[test]
    fn parse_add_community_deep_link_rejects_invalid_relays() {
        for raw in [
            "buzz://add-community",
            "buzz://add-community?relay=",
            "buzz://add-community?relay=not-a-url",
            "buzz://add-community?relay=https%3A%2F%2Facme.example",
            "buzz://add-community?relay=wss%3A%2F%2F",
        ] {
            assert!(parse_add_community_deep_link(&Url::parse(raw).unwrap()).is_none());
        }
    }

    #[test]
    fn parse_message_deep_link_extracts_required_params() {
        let url = Url::parse("buzz://message?channel=abc&id=xyz").unwrap();
        let payload = parse_message_deep_link(&url).expect("required params present");
        assert_eq!(payload["channelId"], "abc");
        assert_eq!(payload["messageId"], "xyz");
        assert!(payload["threadRootId"].is_null());
    }

    #[test]
    fn parse_message_deep_link_accepts_buzz_scheme() {
        let url = Url::parse("buzz://message?channel=abc&id=xyz").unwrap();
        let payload = parse_message_deep_link(&url).expect("required params present");
        assert_eq!(payload["channelId"], "abc");
        assert_eq!(payload["messageId"], "xyz");
    }

    #[test]
    fn parse_message_deep_link_includes_thread_root() {
        let url = Url::parse("buzz://message?channel=abc&id=xyz&thread=root1").unwrap();
        let payload = parse_message_deep_link(&url).expect("required params present");
        assert_eq!(payload["threadRootId"], "root1");
    }

    #[test]
    fn parse_message_deep_link_rejects_missing_id() {
        let url = Url::parse("buzz://message?channel=abc").unwrap();
        assert!(parse_message_deep_link(&url).is_none());
    }

    #[test]
    fn parse_message_deep_link_rejects_empty_channel() {
        // Regression: `channel=&id=foo` previously produced channelId: "".
        let url = Url::parse("buzz://message?channel=&id=foo").unwrap();
        assert!(parse_message_deep_link(&url).is_none());
    }

    #[test]
    fn parse_message_deep_link_rejects_empty_id() {
        let url = Url::parse("buzz://message?channel=abc&id=").unwrap();
        assert!(parse_message_deep_link(&url).is_none());
    }

    #[test]
    fn parse_message_deep_link_treats_empty_thread_as_absent() {
        let url = Url::parse("buzz://message?channel=abc&id=xyz&thread=").unwrap();
        let payload = parse_message_deep_link(&url).expect("required params present");
        assert!(payload["threadRootId"].is_null());
    }

    #[test]
    fn parse_join_deep_link_extracts_relay_and_code() {
        let url = Url::parse("buzz://join?relay=wss%3A%2F%2Frelay.example&code=abc.def").unwrap();
        let payload = parse_join_deep_link(&url).expect("required params present");
        assert_eq!(payload["relayUrl"], "wss://relay.example");
        assert_eq!(payload["code"], "abc.def");
        assert!(payload["policyReceipt"].is_null());
    }

    #[test]
    fn parse_join_deep_link_extracts_policy_receipt() {
        let url = Url::parse(
            "buzz://join?relay=wss%3A%2F%2Frelay.example&code=abc.def&policy_receipt=receipt.value",
        )
        .unwrap();
        let payload = parse_join_deep_link(&url).expect("required params present");
        assert_eq!(payload["policyReceipt"], "receipt.value");
    }

    #[test]
    fn parse_join_deep_link_rejects_missing_code() {
        let url = Url::parse("buzz://join?relay=wss%3A%2F%2Frelay.example").unwrap();
        assert!(parse_join_deep_link(&url).is_none());
    }

    #[test]
    fn parse_join_deep_link_rejects_empty_code() {
        let url = Url::parse("buzz://join?relay=wss%3A%2F%2Frelay.example&code=").unwrap();
        assert!(parse_join_deep_link(&url).is_none());
    }

    #[test]
    fn parse_join_deep_link_rejects_missing_relay() {
        let url = Url::parse("buzz://join?code=abc.def").unwrap();
        assert!(parse_join_deep_link(&url).is_none());
    }

    #[test]
    fn parse_join_deep_link_rejects_non_websocket_relay() {
        let url = Url::parse("buzz://join?relay=https%3A%2F%2Frelay.example&code=abc.def").unwrap();
        assert!(parse_join_deep_link(&url).is_none());
    }

    #[test]
    fn parse_nostr_bind_deep_link_accepts_valid_url() {
        let payload = parse_nostr_bind_deep_link(&valid_nostr_bind_url()).unwrap();
        assert_eq!(payload.challenge_id, "550e8400-e29b-41d4-a716-446655440000");
        assert_eq!(payload.nonce, "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghi01234567");
        assert_eq!(payload.verification_code, "123456");
        assert_eq!(payload.audience, "buzz:nostr-identity");
        assert_eq!(payload.action, "bind_nostr_identity");
        assert_eq!(payload.protocol, "buzz-nostr-identity");
        assert_eq!(payload.version, "1");
        assert_eq!(payload.origin, "https://example.com");
        assert_eq!(payload.expires_at, "2999-01-01T00:00:00Z");
        assert_eq!(payload.return_mode, "clipboard");
        assert_eq!(payload.callback_url, None);
    }

    #[test]
    fn parse_nostr_bind_deep_link_accepts_same_origin_callback_url() {
        let url = Url::parse("buzz://nostr-bind?challenge_id=550e8400-e29b-41d4-a716-446655440000&nonce=ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghi01234567&verification_code=123456&audience=buzz%3Anostr-identity&action=bind_nostr_identity&protocol=buzz-nostr-identity&version=1&origin=https%3A%2F%2Fexample.com&expires_at=2999-01-01T00%3A00%3A00Z&return=clipboard&callback_url=https%3A%2F%2Fexample.com%2Fbuzz%3FmockSession%3D1").unwrap();
        let payload = parse_nostr_bind_deep_link(&url).unwrap();
        assert_eq!(
            payload.callback_url.as_deref(),
            Some("https://example.com/buzz?mockSession=1")
        );
    }

    #[test]
    fn parse_nostr_bind_deep_link_accepts_browser_fragment_return() {
        let url = Url::parse("buzz://nostr-bind?challenge_id=550e8400-e29b-41d4-a716-446655440000&nonce=ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghi01234567&verification_code=123456&audience=buzz%3Anostr-identity&action=bind_nostr_identity&protocol=buzz-nostr-identity&version=1&origin=https%3A%2F%2Fexample.com&expires_at=2999-01-01T00%3A00%3A00Z&return=browser_fragment_v1&callback_url=https%3A%2F%2Fexample.com%2Fbuzz").unwrap();
        let payload = parse_nostr_bind_deep_link(&url).unwrap();

        assert_eq!(payload.return_mode, "browser_fragment_v1");
        assert_eq!(
            payload.callback_url.as_deref(),
            Some("https://example.com/buzz")
        );
    }

    #[test]
    fn parse_nostr_bind_deep_link_requires_callback_for_browser_fragment_return() {
        let url = Url::parse("buzz://nostr-bind?challenge_id=550e8400-e29b-41d4-a716-446655440000&nonce=ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghi01234567&verification_code=123456&audience=buzz%3Anostr-identity&action=bind_nostr_identity&protocol=buzz-nostr-identity&version=1&origin=https%3A%2F%2Fexample.com&expires_at=2999-01-01T00%3A00%3A00Z&return=browser_fragment_v1").unwrap();

        assert_eq!(
            parse_nostr_bind_deep_link(&url).unwrap_err(),
            "browser_fragment_v1 requires callback_url"
        );
    }

    #[test]
    fn parse_nostr_bind_deep_link_rejects_cross_origin_callback_url() {
        let url = Url::parse("buzz://nostr-bind?challenge_id=550e8400-e29b-41d4-a716-446655440000&nonce=ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghi01234567&verification_code=123456&audience=buzz%3Anostr-identity&action=bind_nostr_identity&protocol=buzz-nostr-identity&version=1&origin=https%3A%2F%2Fexample.com&expires_at=2999-01-01T00%3A00%3A00Z&return=clipboard&callback_url=https%3A%2F%2Fevil.example%2Fbuzz").unwrap();
        assert!(parse_nostr_bind_deep_link(&url).is_err());
    }

    #[test]
    fn parse_nostr_bind_deep_link_rejects_http_callback_url() {
        let url = Url::parse("buzz://nostr-bind?challenge_id=550e8400-e29b-41d4-a716-446655440000&nonce=ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghi01234567&verification_code=123456&audience=buzz%3Anostr-identity&action=bind_nostr_identity&protocol=buzz-nostr-identity&version=1&origin=https%3A%2F%2Fexample.com&expires_at=2999-01-01T00%3A00%3A00Z&return=clipboard&callback_url=http%3A%2F%2Fexample.com%2Fbuzz").unwrap();
        assert!(parse_nostr_bind_deep_link(&url).is_err());
    }

    #[test]
    fn parse_nostr_bind_deep_link_rejects_missing_challenge_id() {
        let url = Url::parse("buzz://nostr-bind?nonce=ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghi01234567&verification_code=123456&audience=buzz%3Anostr-identity&action=bind_nostr_identity&protocol=buzz-nostr-identity&version=1&origin=https%3A%2F%2Fexample.com&expires_at=2999-01-01T00%3A00%3A00Z&return=clipboard").unwrap();
        assert!(parse_nostr_bind_deep_link(&url).is_err());
    }

    #[test]
    fn parse_nostr_bind_deep_link_rejects_empty_nonce() {
        let url = Url::parse("buzz://nostr-bind?challenge_id=550e8400-e29b-41d4-a716-446655440000&nonce=&verification_code=123456&audience=buzz%3Anostr-identity&action=bind_nostr_identity&protocol=buzz-nostr-identity&version=1&origin=https%3A%2F%2Fexample.com&expires_at=2999-01-01T00%3A00%3A00Z&return=clipboard").unwrap();
        assert!(parse_nostr_bind_deep_link(&url).is_err());
    }

    #[test]
    fn parse_nostr_bind_deep_link_rejects_missing_verification_code() {
        let url = Url::parse("buzz://nostr-bind?challenge_id=550e8400-e29b-41d4-a716-446655440000&nonce=ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghi01234567&audience=buzz%3Anostr-identity&action=bind_nostr_identity&protocol=buzz-nostr-identity&version=1&origin=https%3A%2F%2Fexample.com&expires_at=2999-01-01T00%3A00%3A00Z&return=clipboard").unwrap();
        assert!(parse_nostr_bind_deep_link(&url).is_err());
    }

    #[test]
    fn parse_nostr_bind_deep_link_rejects_short_verification_code() {
        let url = Url::parse("buzz://nostr-bind?challenge_id=550e8400-e29b-41d4-a716-446655440000&nonce=ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghi01234567&verification_code=12345&audience=buzz%3Anostr-identity&action=bind_nostr_identity&protocol=buzz-nostr-identity&version=1&origin=https%3A%2F%2Fexample.com&expires_at=2999-01-01T00%3A00%3A00Z&return=clipboard").unwrap();
        assert!(parse_nostr_bind_deep_link(&url).is_err());
    }

    #[test]
    fn parse_nostr_bind_deep_link_rejects_long_verification_code() {
        let url = Url::parse("buzz://nostr-bind?challenge_id=550e8400-e29b-41d4-a716-446655440000&nonce=ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghi01234567&verification_code=1234567&audience=buzz%3Anostr-identity&action=bind_nostr_identity&protocol=buzz-nostr-identity&version=1&origin=https%3A%2F%2Fexample.com&expires_at=2999-01-01T00%3A00%3A00Z&return=clipboard").unwrap();
        assert!(parse_nostr_bind_deep_link(&url).is_err());
    }

    #[test]
    fn parse_nostr_bind_deep_link_rejects_non_digit_verification_code() {
        let url = Url::parse("buzz://nostr-bind?challenge_id=550e8400-e29b-41d4-a716-446655440000&nonce=ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghi01234567&verification_code=12345a&audience=buzz%3Anostr-identity&action=bind_nostr_identity&protocol=buzz-nostr-identity&version=1&origin=https%3A%2F%2Fexample.com&expires_at=2999-01-01T00%3A00%3A00Z&return=clipboard").unwrap();
        assert!(parse_nostr_bind_deep_link(&url).is_err());
    }

    #[test]
    fn parse_nostr_bind_deep_link_rejects_wrong_action() {
        let url = Url::parse("buzz://nostr-bind?challenge_id=550e8400-e29b-41d4-a716-446655440000&nonce=ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghi01234567&verification_code=123456&audience=buzz%3Anostr-identity&action=wrong&protocol=buzz-nostr-identity&version=1&origin=https%3A%2F%2Fexample.com&expires_at=2999-01-01T00%3A00%3A00Z&return=clipboard").unwrap();
        assert!(parse_nostr_bind_deep_link(&url).is_err());
    }

    #[test]
    fn parse_nostr_bind_deep_link_rejects_wrong_audience() {
        let url = Url::parse("buzz://nostr-bind?challenge_id=550e8400-e29b-41d4-a716-446655440000&nonce=ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghi01234567&verification_code=123456&audience=other&action=bind_nostr_identity&protocol=buzz-nostr-identity&version=1&origin=https%3A%2F%2Fexample.com&expires_at=2999-01-01T00%3A00%3A00Z&return=clipboard").unwrap();
        assert!(parse_nostr_bind_deep_link(&url).is_err());
    }

    #[test]
    fn parse_nostr_bind_deep_link_rejects_non_https_origin() {
        let url = Url::parse("buzz://nostr-bind?challenge_id=550e8400-e29b-41d4-a716-446655440000&nonce=ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghi01234567&verification_code=123456&audience=buzz%3Anostr-identity&action=bind_nostr_identity&protocol=buzz-nostr-identity&version=1&origin=http%3A%2F%2Fexample.com&expires_at=2999-01-01T00%3A00%3A00Z&return=clipboard").unwrap();
        assert!(parse_nostr_bind_deep_link(&url).is_err());
    }

    #[test]
    fn parse_nostr_bind_deep_link_rejects_origin_with_path() {
        let url = Url::parse("buzz://nostr-bind?challenge_id=550e8400-e29b-41d4-a716-446655440000&nonce=ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghi01234567&verification_code=123456&audience=buzz%3Anostr-identity&action=bind_nostr_identity&protocol=buzz-nostr-identity&version=1&origin=https%3A%2F%2Fexample.com%2Fbind&expires_at=2999-01-01T00%3A00%3A00Z&return=clipboard").unwrap();
        assert!(parse_nostr_bind_deep_link(&url).is_err());
    }

    #[test]
    fn parse_nostr_bind_deep_link_rejects_origin_with_credentials() {
        let url = Url::parse("buzz://nostr-bind?challenge_id=550e8400-e29b-41d4-a716-446655440000&nonce=ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghi01234567&verification_code=123456&audience=buzz%3Anostr-identity&action=bind_nostr_identity&protocol=buzz-nostr-identity&version=1&origin=https%3A%2F%2Fuser%40example.com&expires_at=2999-01-01T00%3A00%3A00Z&return=clipboard").unwrap();
        assert!(parse_nostr_bind_deep_link(&url).is_err());
    }

    #[test]
    fn parse_nostr_bind_deep_link_rejects_unsupported_return_mode() {
        let url = Url::parse("buzz://nostr-bind?challenge_id=550e8400-e29b-41d4-a716-446655440000&nonce=ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghi01234567&verification_code=123456&audience=buzz%3Anostr-identity&action=bind_nostr_identity&protocol=buzz-nostr-identity&version=1&origin=https%3A%2F%2Fexample.com&expires_at=2999-01-01T00%3A00%3A00Z&return=callback").unwrap();
        assert!(parse_nostr_bind_deep_link(&url).is_err());
    }

    #[test]
    fn parse_restart_agent_deep_link_extracts_pubkey_and_token() {
        let url = Url::parse(&format!(
            "buzz://restart-agent?pubkey={AGENT_PUBKEY}&token=s3cret"
        ))
        .unwrap();
        let request = parse_restart_agent_deep_link(&url).unwrap();
        assert_eq!(request.pubkey, AGENT_PUBKEY);
        assert_eq!(request.token, "s3cret");
        assert_eq!(request.relay_url, None);
    }

    #[test]
    fn parse_restart_agent_deep_link_extracts_optional_relay() {
        let url = Url::parse(&format!(
            "buzz://restart-agent?pubkey={AGENT_PUBKEY}&token=s3cret&relay=wss%3A%2F%2Frelay.example"
        ))
        .unwrap();
        let request = parse_restart_agent_deep_link(&url).unwrap();
        assert_eq!(request.relay_url.as_deref(), Some("wss://relay.example"));
    }

    #[test]
    fn parse_restart_agent_deep_link_normalizes_an_uppercase_pubkey() {
        let url = Url::parse(&format!(
            "buzz://restart-agent?pubkey={}&token=s3cret",
            AGENT_PUBKEY.to_ascii_uppercase()
        ))
        .unwrap();
        let request = parse_restart_agent_deep_link(&url).unwrap();
        assert_eq!(request.pubkey, AGENT_PUBKEY);
    }

    #[test]
    fn parse_restart_agent_deep_link_rejects_a_missing_or_empty_pubkey() {
        for raw in [
            "buzz://restart-agent?token=s3cret".to_owned(),
            "buzz://restart-agent?pubkey=&token=s3cret".to_owned(),
        ] {
            assert_eq!(
                parse_restart_agent_deep_link(&Url::parse(&raw).unwrap()).unwrap_err(),
                "missing pubkey"
            );
        }
    }

    #[test]
    fn parse_restart_agent_deep_link_rejects_a_malformed_pubkey() {
        for pubkey in [
            "notahexstring",
            &AGENT_PUBKEY[..63],
            &format!("{AGENT_PUBKEY}0"),
            &format!("{}zz", &AGENT_PUBKEY[..62]),
        ] {
            let url = Url::parse(&format!(
                "buzz://restart-agent?pubkey={pubkey}&token=s3cret"
            ))
            .unwrap();
            assert_eq!(
                parse_restart_agent_deep_link(&url).unwrap_err(),
                "pubkey must be 64 hexadecimal characters",
                "{pubkey}"
            );
        }
    }

    #[test]
    fn parse_restart_agent_deep_link_rejects_a_missing_or_empty_token() {
        for raw in [
            format!("buzz://restart-agent?pubkey={AGENT_PUBKEY}"),
            format!("buzz://restart-agent?pubkey={AGENT_PUBKEY}&token="),
        ] {
            assert_eq!(
                parse_restart_agent_deep_link(&Url::parse(&raw).unwrap()).unwrap_err(),
                "missing token"
            );
        }
    }

    #[test]
    fn parse_restart_agent_deep_link_rejects_a_non_websocket_relay() {
        // A present-but-invalid relay must fail, not silently degrade into the
        // "restart every live pair" branch.
        for relay in ["not-a-url", "https%3A%2F%2Frelay.example", "wss%3A%2F%2F"] {
            let url = Url::parse(&format!(
                "buzz://restart-agent?pubkey={AGENT_PUBKEY}&token=s3cret&relay={relay}"
            ))
            .unwrap();
            assert_eq!(
                parse_restart_agent_deep_link(&url).unwrap_err(),
                "relay must be a ws:// or wss:// URL",
                "{relay}"
            );
        }
    }

    #[test]
    fn parse_restart_agent_deep_link_treats_an_empty_relay_as_absent() {
        let url = Url::parse(&format!(
            "buzz://restart-agent?pubkey={AGENT_PUBKEY}&token=s3cret&relay="
        ))
        .unwrap();
        assert_eq!(parse_restart_agent_deep_link(&url).unwrap().relay_url, None);
    }

    #[test]
    fn parse_delete_agent_deep_link_extracts_pubkey_and_token() {
        let url = Url::parse(&format!(
            "buzz://delete-agent?pubkey={AGENT_PUBKEY}&token=s3cret"
        ))
        .unwrap();
        let request = parse_delete_agent_deep_link(&url).unwrap();
        assert_eq!(request.pubkey, AGENT_PUBKEY);
        assert_eq!(request.token, "s3cret");
    }

    #[test]
    fn parse_delete_agent_deep_link_normalizes_an_uppercase_pubkey() {
        let url = Url::parse(&format!(
            "buzz://delete-agent?pubkey={}&token=s3cret",
            AGENT_PUBKEY.to_ascii_uppercase()
        ))
        .unwrap();
        let request = parse_delete_agent_deep_link(&url).unwrap();
        assert_eq!(request.pubkey, AGENT_PUBKEY);
    }

    #[test]
    fn parse_delete_agent_deep_link_rejects_a_missing_or_empty_pubkey() {
        for raw in [
            "buzz://delete-agent?token=s3cret".to_owned(),
            "buzz://delete-agent?pubkey=&token=s3cret".to_owned(),
        ] {
            assert_eq!(
                parse_delete_agent_deep_link(&Url::parse(&raw).unwrap()).unwrap_err(),
                "missing pubkey"
            );
        }
    }

    #[test]
    fn parse_delete_agent_deep_link_rejects_a_malformed_pubkey() {
        for pubkey in [
            "notahexstring",
            &AGENT_PUBKEY[..63],
            &format!("{AGENT_PUBKEY}0"),
            &format!("{}zz", &AGENT_PUBKEY[..62]),
        ] {
            let url =
                Url::parse(&format!("buzz://delete-agent?pubkey={pubkey}&token=s3cret")).unwrap();
            assert_eq!(
                parse_delete_agent_deep_link(&url).unwrap_err(),
                "pubkey must be 64 hexadecimal characters",
                "{pubkey}"
            );
        }
    }

    #[test]
    fn parse_delete_agent_deep_link_rejects_a_missing_or_empty_token() {
        for raw in [
            format!("buzz://delete-agent?pubkey={AGENT_PUBKEY}"),
            format!("buzz://delete-agent?pubkey={AGENT_PUBKEY}&token="),
        ] {
            assert_eq!(
                parse_delete_agent_deep_link(&Url::parse(&raw).unwrap()).unwrap_err(),
                "missing token"
            );
        }
    }

    #[test]
    fn parse_nostr_bind_deep_link_accepts_expired_link_for_user_facing_error() {
        let url = Url::parse("buzz://nostr-bind?challenge_id=550e8400-e29b-41d4-a716-446655440000&nonce=ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghi01234567&verification_code=123456&audience=buzz%3Anostr-identity&action=bind_nostr_identity&protocol=buzz-nostr-identity&version=1&origin=https%3A%2F%2Fexample.com&expires_at=2000-01-01T00%3A00%3A00Z&return=clipboard").unwrap();
        let payload = parse_nostr_bind_deep_link(&url).unwrap();
        assert_eq!(payload.expires_at, "2000-01-01T00:00:00Z");
    }
}
