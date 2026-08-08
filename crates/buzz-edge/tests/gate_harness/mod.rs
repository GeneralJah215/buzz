//! Shared fixture for the M5 release gates.
//!
//! This is the same standing-up procedure `tests/status_and_requeue.rs`
//! performs — a real [`EdgeStore`], a real signed kind-39002 roster, a real
//! authorization lease, and a loopback sidecar reached over an authenticated
//! WebSocket session — lifted into one module so the gates cannot drift into a
//! second, subtly different notion of "an authorized sidecar".
//!
//! Nothing here is a mock of the sidecar. The only thing the gates fake is the
//! *canonical relay*, and only gate 2 does that (deliberately, and slowly).
//!
//! `dead_code` is allowed because each gate is its own test binary and links
//! this module separately: a helper only gate 3 needs is genuinely unused in
//! gate 2's binary.

#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use nostr::{Event, EventBuilder, JsonUtil, Keys, Kind, PublicKey, Tag, Timestamp};
use serde_json::{json, Value};
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};
use uuid::Uuid;

use buzz_edge::storage::{
    AuthorizationPolicy, CommunityBinding, EdgeStore, VerifiedChannelAuthorization,
};

/// A client's end of a loopback sidecar session.
pub type ClientSocket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// Every frame a gate sent to a sidecar through this module, in order.
///
/// Recorded unconditionally rather than opt-in: gate 4 asserts that no private
/// key material ever crosses the client/sidecar boundary, and an opt-in log is
/// an invitation to add one more `send` call that the assertion silently misses.
static SENT_FRAMES: std::sync::OnceLock<std::sync::Mutex<Vec<String>>> = std::sync::OnceLock::new();

/// Every frame a sidecar sent back through this module, in order.
static RECEIVED_FRAMES: std::sync::OnceLock<std::sync::Mutex<Vec<String>>> =
    std::sync::OnceLock::new();

fn sent_log() -> &'static std::sync::Mutex<Vec<String>> {
    SENT_FRAMES.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

fn received_log() -> &'static std::sync::Mutex<Vec<String>> {
    RECEIVED_FRAMES.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

/// Snapshot of every frame sent to a sidecar so far.
pub fn sent_frames() -> Vec<String> {
    sent_log().lock().expect("sent frame log").clone()
}

/// Snapshot of every frame received from a sidecar so far.
pub fn received_frames() -> Vec<String> {
    received_log().lock().expect("received frame log").clone()
}

/// The production offline-lease window (§7).
pub const LEASE_WINDOW: Duration = Duration::from_secs(72 * 60 * 60);

/// Seconds since the Unix epoch.
pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock is after the Unix epoch")
        .as_secs() as i64
}

/// The production authorization policy.
pub fn policy() -> AuthorizationPolicy {
    AuthorizationPolicy::new(LEASE_WINDOW).expect("authorization policy")
}

/// Sign one kind-9 channel message.
pub fn message(keys: &Keys, channel: Uuid, content: &str) -> Event {
    EventBuilder::new(Kind::Custom(9), content)
        .tags([Tag::parse(["h", channel.to_string().as_str()]).expect("h tag")])
        .sign_with_keys(keys)
        .expect("sign kind-9 event")
}

/// Select each channel, mark it edge-eligible, and record its signed roster.
///
/// This is what a real startup leaves behind: `selected_channels.active = 1`,
/// `edge_eligible = 1`, a live `authorization_snapshot`, and `channel_members`
/// rows. Every read and every ingress is gated on exactly that, so a gate that
/// skipped this would be measuring an unauthorized store.
///
/// Returns the relay identity that signed the rosters — the upstream mirror
/// pins it, so gate 2 needs it to stand up a canonical relay the sidecar will
/// actually talk to — together with the edge-signed snapshot event the store
/// persisted, which gate 4 inspects.
pub fn authorize(
    store: &EdgeStore,
    edge: &Keys,
    verified_at: i64,
    channels: &[(Uuid, Vec<PublicKey>)],
) -> (Keys, Event) {
    let relay = Keys::generate();
    let snapshot = authorize_as(store, edge, &relay, verified_at, channels);
    (relay, snapshot)
}

/// [`authorize`] with a caller-chosen relay signing identity.
pub fn authorize_as(
    store: &EdgeStore,
    edge: &Keys,
    relay: &Keys,
    verified_at: i64,
    channels: &[(Uuid, Vec<PublicKey>)],
) -> Event {
    let mut authorizations = Vec::new();
    for (channel, members) in channels {
        store
            .set_channel_selected(*channel, true)
            .expect("channel selection");
        let mut tags = vec![Tag::parse(["d", channel.to_string().as_str()]).expect("d tag")];
        for member in members {
            tags.push(Tag::parse(["p", member.to_hex().as_str()]).expect("p tag"));
        }
        let source = EventBuilder::new(Kind::Custom(39_002), "")
            .tags(tags)
            .custom_created_at(Timestamp::from(verified_at as u64))
            .sign_with_keys(relay)
            .expect("membership event");
        authorizations.push(VerifiedChannelAuthorization {
            channel_id: *channel,
            membership_event_id: source.id,
            membership_event_created_at: verified_at,
            membership_event_bytes: source.as_json().into_bytes(),
            membership_fetch_cursor: None,
            signal_cursor: None,
            edge_notification_cursor: None,
            active_authors: members.clone(),
            removed_authors: Vec::new(),
        });
    }
    store
        .persist_verified_authorization_snapshot(&authorizations, verified_at, edge)
        .expect("authorization snapshot")
}

/// Read the next text frame, failing loudly on anything else.
pub async fn next_frame(socket: &mut ClientSocket) -> String {
    let message = socket
        .next()
        .await
        .expect("the sidecar closed the session instead of sending a frame")
        .expect("valid WebSocket frame");
    let Message::Text(message) = message else {
        panic!("expected a text frame from the sidecar");
    };
    let message = message.to_string();
    received_log()
        .lock()
        .expect("received frame log")
        .push(message.clone());
    message
}

/// Read the next text frame as JSON.
pub async fn next_json(socket: &mut ClientSocket) -> Value {
    let frame = next_frame(socket).await;
    serde_json::from_str(&frame)
        .unwrap_or_else(|error| panic!("invalid JSON frame {frame}: {error}"))
}

/// Send one JSON frame, recording it in [`sent_frames`].
pub async fn send(socket: &mut ClientSocket, frame: Value) {
    let raw = frame.to_string();
    sent_log().lock().expect("sent frame log").push(raw.clone());
    socket
        .send(Message::Text(raw.into()))
        .await
        .expect("send frame to the sidecar");
}

/// Complete the community-binding handshake and NIP-42 authentication.
pub async fn authenticated_client(
    url: &str,
    binding: &CommunityBinding,
    keys: &Keys,
) -> ClientSocket {
    let (mut socket, _) = connect_async(url)
        .await
        .unwrap_or_else(|error| panic!("connect to the sidecar at {url}: {error}"));
    let challenge = next_json(&mut socket).await;
    let challenge = challenge
        .get(1)
        .and_then(Value::as_str)
        .expect("AUTH challenge")
        .to_string();
    send(
        &mut socket,
        json!([
            "BUZZ-EDGE",
            "BIND",
            {
                "canonical_origin": binding.canonical_origin(),
                "community_id": binding.community_id(),
            }
        ]),
    )
    .await;
    let bound = next_json(&mut socket).await;
    assert_eq!(
        bound[2], true,
        "community binding handshake refused: {bound}"
    );
    let auth = buzz_ws_client::build_auth_event(&challenge, url, keys, None).expect("auth event");
    send(&mut socket, json!(["AUTH", auth])).await;
    let authenticated = next_json(&mut socket).await;
    assert_eq!(
        authenticated[2], true,
        "NIP-42 authentication refused: {authenticated}"
    );
    socket
}

/// Open a live kind-9 subscription and return the stored history it replayed.
///
/// Fails loudly on `CLOSED`: a gate that treated a refused subscription as
/// "no history" would turn an authorization failure into a green run.
pub async fn subscribe(socket: &mut ClientSocket, sub_id: &str, channel: Uuid) -> Vec<Event> {
    send(
        socket,
        json!(["REQ", sub_id, {"kinds":[9], "#h":[channel.to_string()]}]),
    )
    .await;
    let mut history = Vec::new();
    loop {
        let frame = next_json(socket).await;
        match frame[0].as_str() {
            Some("EVENT") => {
                assert_eq!(
                    frame[1], sub_id,
                    "history for another subscription: {frame}"
                );
                history.push(
                    serde_json::from_value::<Event>(frame[2].clone())
                        .unwrap_or_else(|error| panic!("invalid history event {frame}: {error}")),
                );
            }
            Some("EOSE") => {
                assert_eq!(frame[1], sub_id, "EOSE for another subscription: {frame}");
                return history;
            }
            _ => panic!("subscription {sub_id} was refused or misordered: {frame}"),
        }
    }
}

/// Submit one signed event and return the sidecar's `OK` frame.
pub async fn submit(socket: &mut ClientSocket, event: &Event) -> Value {
    send(socket, json!(["EVENT", event])).await;
    loop {
        let frame = next_json(socket).await;
        if frame[0].as_str() == Some("EVENT") {
            // The submitter's own subscription echoes the event back; the OK is
            // what this call is waiting for.
            continue;
        }
        assert_eq!(frame[0], "OK", "expected an OK for the submission: {frame}");
        assert_eq!(frame[1], event.id.to_hex(), "OK for another event: {frame}");
        return frame;
    }
}

/// Stand a sidecar up on a fresh loopback port around an already-opened store.
pub async fn serve(
    store: Arc<EdgeStore>,
    edge_keys: Keys,
    local_routing_ready: bool,
) -> (String, buzz_edge::EdgeRelay, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback listener");
    let address = listener.local_addr().expect("listener address");
    let url = format!("ws://{address}");
    let binding = store.binding().clone();
    let relay = buzz_edge::EdgeRelay::new(
        buzz_edge::EdgeConfig::new(&url, binding).expect("edge config"),
        store,
        edge_keys,
        local_routing_ready,
    )
    .expect("edge relay");
    let served = relay.clone();
    let server = tokio::spawn(async move {
        buzz_edge::run_server(listener, served)
            .await
            .expect("sidecar server");
    });
    (url, relay, server)
}
