//! End-to-end tests for the operator status and manual requeue surface (§11).
//!
//! Each test opens an in-memory store, drives outbox rows into known states
//! through the real drain state machine, serves the sidecar on a random
//! loopback port (`:0`), and talks to it over an authenticated WebSocket
//! session — the same path Desktop uses.
//!
//! Rows are set up through `EdgeStore` rather than over the wire so every test
//! starts from an exact, known outbox state; all assertions are then made
//! against what the sidecar itself sends back, and against the rows afterwards.

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use nostr::{Event, EventBuilder, JsonUtil, Keys, Kind, Tag};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};
use uuid::Uuid;

use buzz_edge::storage::{
    AuthorizationPolicy, CommunityBinding, DrainOutcome, EdgeStore, EventDeliveryState,
};
use buzz_edge::{run_server, EdgeConfig, EdgeError, EdgeRelay};

type ClientSocket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// Bob's message body. The operator status surface is community-wide, so this
/// string must never reach a session that is not bob's.
const PRIVATE_BODY: &str = "PRIVATE-BODY-9c31-do-not-leak";

/// Why upstream refused bob's event. A requeue clears `last_error`, so this
/// surviving is evidence the row was left completely alone.
const QUARANTINE_REASON: &str = "upstream refused: membership revoked";

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_secs() as i64
}

fn message(keys: &Keys, channel: Uuid, content: &str) -> Event {
    EventBuilder::new(Kind::Custom(9), content)
        .tags([Tag::parse(["h", channel.to_string().as_str()]).expect("h tag")])
        .sign_with_keys(keys)
        .expect("sign")
}

async fn next_frame(socket: &mut ClientSocket) -> String {
    let message = socket
        .next()
        .await
        .expect("server frame")
        .expect("valid frame");
    let Message::Text(message) = message else {
        panic!("expected text frame");
    };
    message.to_string()
}

async fn next_json(socket: &mut ClientSocket) -> Value {
    serde_json::from_str(&next_frame(socket).await).expect("valid JSON")
}

async fn send(socket: &mut ClientSocket, frame: Value) {
    socket
        .send(Message::Text(frame.to_string().into()))
        .await
        .expect("send");
}

async fn authenticated_client(url: &str, binding: &CommunityBinding, keys: &Keys) -> ClientSocket {
    let (mut socket, _) = connect_async(url).await.expect("connect");
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
    assert_eq!(next_json(&mut socket).await[2], true);
    let auth = buzz_ws_client::build_auth_event(&challenge, url, keys, None).expect("auth event");
    send(&mut socket, json!(["AUTH", auth])).await;
    assert_eq!(next_json(&mut socket).await[2], true);
    socket
}

struct Fixture {
    store: Arc<EdgeStore>,
    binding: CommunityBinding,
    url: String,
    /// Owns two rows: one acknowledged delivered, one still waiting to drain.
    alice: Keys,
    /// Owns the single quarantined row.
    bob: Keys,
    /// Bob's permanently refused event, carrying [`PRIVATE_BODY`].
    quarantined: Event,
    /// Alice's row that no author is currently draining.
    pending: Event,
    server: tokio::task::JoinHandle<Result<(), EdgeError>>,
}

/// Three rows in three different states, written the way the sidecar writes
/// them: pending (alice), delivered (alice), quarantined (bob).
async fn fixture() -> Fixture {
    let binding =
        CommunityBinding::new("wss://relay.example.com", Uuid::new_v4()).expect("binding");
    let policy = AuthorizationPolicy::new(Duration::from_secs(72 * 60 * 60)).expect("policy");
    let store = Arc::new(EdgeStore::open_in_memory(binding.clone(), policy).expect("store"));
    let edge_keys = Keys::generate();
    let alice = Keys::generate();
    let bob = Keys::generate();
    let channel = Uuid::new_v4();

    let delivered = message(&alice, channel, "alice delivered");
    let pending = message(&alice, channel, "alice pending");
    let quarantined = message(&bob, channel, PRIVATE_BODY);
    for event in [&delivered, &pending, &quarantined] {
        store
            .insert_local_event(event, event.as_json().as_bytes(), channel, &edge_keys)
            .expect("insert");
    }

    // Move the rows through the real claim/acknowledge path rather than
    // writing SQL, so the fixture cannot invent a state the sidecar could
    // never produce.
    let now = unix_now();
    let claimed = store
        .claim_outbox_batch(&alice.public_key(), "alice-setup", 100, now, 60)
        .expect("claim alice");
    assert_eq!(claimed.len(), 2, "both of alice's rows should be claimable");
    assert!(store
        .acknowledge_outbox_row("alice-setup", &delivered.id, DrainOutcome::Delivered)
        .expect("ack delivered"));
    let claimed = store
        .claim_outbox_batch(&bob.public_key(), "bob-setup", 100, now, 60)
        .expect("claim bob");
    assert_eq!(claimed.len(), 1, "bob owns exactly one row");
    assert!(store
        .acknowledge_outbox_row(
            "bob-setup",
            &quarantined.id,
            DrainOutcome::Rejected(QUARANTINE_REASON.to_string()),
        )
        .expect("ack rejected"));
    // Release the row alice claimed but never acknowledged.
    store.expire_outbox_leases(now + 61).expect("expire leases");

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let url = format!("ws://{address}");
    let relay = EdgeRelay::new(
        EdgeConfig::new(&url, binding.clone()).expect("config"),
        Arc::clone(&store),
        edge_keys,
        true,
    )
    .expect("relay");
    let server = tokio::spawn(run_server(listener, relay));

    Fixture {
        store,
        binding,
        url,
        alice,
        bob,
        quarantined,
        pending,
        server,
    }
}

#[tokio::test]
async fn status_reports_the_rows_actually_in_the_store() {
    let fixture = fixture().await;
    let mut alice = authenticated_client(&fixture.url, &fixture.binding, &fixture.alice).await;
    send(
        &mut alice,
        json!(["BUZZ-EDGE", "STATUS", {"req_id":"status-1","limit":50}]),
    )
    .await;
    let frame = next_frame(&mut alice).await;

    // The privacy invariant, end to end: the quarantined row is bob's, the
    // session is alice's, and bob's message body is sitting in the same
    // database this reply was built from.
    assert!(
        frame.contains(&fixture.quarantined.id.to_hex()),
        "the quarantined row must be in the reply, or its absence of content \
         proves nothing: {frame}"
    );
    assert!(
        !frame.contains(PRIVATE_BODY),
        "another identity's message content leaked through the status surface: {frame}"
    );

    let parsed: Value = serde_json::from_str(&frame).expect("json");
    assert_eq!(parsed[0], "BUZZ-EDGE");
    assert_eq!(parsed[1], "STATUS-REPLY");
    assert_eq!(parsed[2]["req_id"], "status-1");

    // One row was acknowledged delivered, one is waiting for alice, one was
    // refused upstream. Nothing is claimed: the setup lease was released.
    let summary = &parsed[2]["summary"];
    assert_eq!(summary["pending"], 1);
    assert_eq!(summary["claimed"], 0);
    assert_eq!(summary["deliveredExact"], 1);
    assert_eq!(summary["deliveredViaDigest"], 0);
    assert_eq!(summary["quarantined"], 1);

    let quarantined = parsed[2]["quarantined"]
        .as_array()
        .expect("quarantine list");
    assert_eq!(quarantined.len(), 1);
    assert_eq!(quarantined[0]["eventId"], fixture.quarantined.id.to_hex());
    assert_eq!(quarantined[0]["author"], fixture.bob.public_key().to_hex());
    assert_eq!(quarantined[0]["reason"], QUARANTINE_REASON);
    assert_eq!(
        quarantined[0]["attempts"], 1,
        "the row cost one drain attempt before it was refused"
    );

    // Bob's refused row is not "waiting for an author": nobody can drain it
    // until it is requeued. Alice's untouched row is.
    let waiting = parsed[2]["waitingAuthors"]
        .as_array()
        .expect("waiting list");
    assert_eq!(waiting.len(), 1, "only alice has a row nobody is draining");
    assert_eq!(waiting[0]["author"], fixture.alice.public_key().to_hex());
    assert_eq!(waiting[0]["pending"], 1);

    fixture.server.abort();
}

#[tokio::test]
async fn a_requeue_from_another_identity_is_refused_and_leaves_the_row_quarantined() {
    // The authorization boundary of this surface: reading status is
    // community-wide, writing is not. Alice can see that bob's row is stuck;
    // she must not be able to act on it.
    let fixture = fixture().await;
    let mut alice = authenticated_client(&fixture.url, &fixture.binding, &fixture.alice).await;
    send(
        &mut alice,
        json!([
            "BUZZ-EDGE", "REQUEUE",
            {"req_id":"alice-1","event_id":fixture.quarantined.id.to_hex()}
        ]),
    )
    .await;
    let reply = next_json(&mut alice).await;
    assert_eq!(reply[0], "BUZZ-EDGE");
    assert_eq!(reply[1], "REQUEUE-REPLY");
    assert_eq!(reply[2]["req_id"], "alice-1");
    assert_eq!(
        reply[2]["requeued"], false,
        "alice must not be able to retry bob's row: {reply}"
    );

    // The reply is not the assertion — the row is. A requeue moves the row to
    // `pending` and clears `last_error`, so a hole in the ownership filter
    // would show up here even if the reply still said false.
    let rows = fixture
        .store
        .quarantined_rows(500)
        .expect("quarantine list");
    let row = rows
        .iter()
        .find(|row| row.event_id == fixture.quarantined.id)
        .expect("bob's row must still be quarantined");
    assert_eq!(row.author, fixture.bob.public_key());
    assert_eq!(row.reason, QUARANTINE_REASON);
    assert_eq!(
        row.attempts, 1,
        "a refused requeue must not spend an attempt"
    );
    assert_eq!(
        fixture
            .store
            .event_delivery_states(&[fixture.quarantined.id])
            .expect("delivery states"),
        vec![(fixture.quarantined.id, EventDeliveryState::Quarantined)]
    );

    // And behaviourally: the row is still out of the drain queue, including
    // for the identity that owns it.
    let mut bob = authenticated_client(&fixture.url, &fixture.binding, &fixture.bob).await;
    send(
        &mut bob,
        json!(["BUZZ-EDGE", "DRAIN", {"claim_token":"bob-token","limit":10}]),
    )
    .await;
    let batch = next_json(&mut bob).await;
    assert_eq!(batch[1], "DRAIN-BATCH");
    assert!(
        batch[2]["events"].as_array().expect("events").is_empty(),
        "a quarantined row must stay out of the drain queue: {batch}"
    );

    fixture.server.abort();
}

#[tokio::test]
async fn the_owning_identity_requeues_its_row_back_into_the_drain_queue() {
    let fixture = fixture().await;
    let mut bob = authenticated_client(&fixture.url, &fixture.binding, &fixture.bob).await;
    send(
        &mut bob,
        json!([
            "BUZZ-EDGE", "REQUEUE",
            {"req_id":"bob-1","event_id":fixture.quarantined.id.to_hex()}
        ]),
    )
    .await;
    let reply = next_json(&mut bob).await;
    assert_eq!(reply[1], "REQUEUE-REPLY");
    assert_eq!(reply[2]["req_id"], "bob-1");
    assert_eq!(reply[2]["requeued"], true);

    // A requeue that does not make the row drainable again has not done the
    // thing the operator pressed the button for.
    send(
        &mut bob,
        json!(["BUZZ-EDGE", "DRAIN", {"claim_token":"bob-token","limit":10}]),
    )
    .await;
    let batch = next_json(&mut bob).await;
    assert_eq!(batch[1], "DRAIN-BATCH");
    let events = batch[2]["events"].as_array().expect("events");
    assert_eq!(
        events.len(),
        1,
        "the requeued row must be claimable: {batch}"
    );
    let returned: Event = serde_json::from_value(events[0].clone()).expect("event");
    assert_eq!(returned.id, fixture.quarantined.id);
    assert!(
        returned.verify().is_ok(),
        "the requeued row must still be the signed original, byte for byte"
    );

    // Requeueing bob's row must not have made it reachable from another
    // session: a drain only ever returns the session identity's own rows.
    let mut alice = authenticated_client(&fixture.url, &fixture.binding, &fixture.alice).await;
    send(
        &mut alice,
        json!(["BUZZ-EDGE", "DRAIN", {"claim_token":"alice-token","limit":10}]),
    )
    .await;
    let batch = next_json(&mut alice).await;
    let events = batch[2]["events"].as_array().expect("events");
    assert_eq!(events.len(), 1);
    let returned: Event = serde_json::from_value(events[0].clone()).expect("event");
    assert_eq!(returned.id, fixture.pending.id);

    fixture.server.abort();
}
