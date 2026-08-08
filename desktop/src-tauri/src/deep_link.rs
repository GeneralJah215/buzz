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
        Some(action) => {
            eprintln!("buzz-desktop: unknown deep link action: {action}");
        }
        None => {
            eprintln!("buzz-desktop: deep link missing action: {url_str}");
        }
    }
}

#[cfg(test)]
#[path = "deep_link_tests.rs"]
mod tests;
