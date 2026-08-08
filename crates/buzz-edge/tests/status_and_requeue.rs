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
//!
//! The fixture seeds **real** channel selection, edge eligibility, an
//! authorization lease, and a signed roster. Without those, every read here is
//! refused (or, before the channel gate existed, every read returned
//! everything) and the assertions below would be about an unauthorized store
//! rather than about the surface.

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use nostr::{Event, EventBuilder, JsonUtil, Keys, Kind, PublicKey, Tag, Timestamp};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};
use uuid::Uuid;

use buzz_edge::storage::{
    AuthorizationPolicy, CommunityBinding, DrainOutcome, EdgeStore, EventDeliveryState,
    VerifiedChannelAuthorization,
};
use buzz_edge::{run_server, EdgeConfig, EdgeError, EdgeRelay};

type ClientSocket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// Bob's message body. Bob shares a channel with alice, so his *row* is
/// deliberately visible to her — that is the operator behaviour the surface
/// exists for — but his message content must never be.
const PRIVATE_BODY: &str = "PRIVATE-BODY-9c31-do-not-leak";

/// Carol's message body. Carol is in a channel alice is not a member of, so
/// nothing about her row may reach alice: not the body, not the event ID, not
/// the channel ID, not her pubkey, not the failure text.
const OTHER_CHANNEL_BODY: &str = "OTHER-CHANNEL-BODY-4d70-do-not-leak";

/// Why upstream refused bob's event. A requeue clears `last_error`, so this
/// surviving is evidence the row was left completely alone.
const QUARANTINE_REASON: &str = "upstream refused: membership revoked";

/// Why upstream refused carol's event. Failure text is operational detail about
/// a channel alice cannot read, so it is part of what must not leak.
const OTHER_CHANNEL_REASON: &str = "upstream refused: OTHER-CHANNEL-REASON-8b12";

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

/// Select each channel, mark it edge-eligible, and record its signed roster.
///
/// This is what a real startup leaves behind: `selected_channels.active = 1`,
/// `edge_eligible = 1`, a live `authorization_snapshot`, and `channel_members`
/// rows. Every status read is gated on exactly that.
fn authorize(
    store: &EdgeStore,
    edge: &Keys,
    verified_at: i64,
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
        let source = EventBuilder::new(Kind::Custom(39_002), "")
            .tags(tags)
            .custom_created_at(Timestamp::from(verified_at as u64))
            .sign_with_keys(&relay)
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
        .expect("snapshot");
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

/// Ask for status over an authenticated session and return the reply body.
async fn status_body(socket: &mut ClientSocket, req_id: &str) -> Value {
    send(
        socket,
        json!(["BUZZ-EDGE", "STATUS", {"req_id": req_id, "limit": 50}]),
    )
    .await;
    let frame = next_frame(socket).await;
    let parsed: Value = serde_json::from_str(&frame).expect("json");
    assert_eq!(parsed[1], "STATUS-REPLY", "unexpected frame: {frame}");
    assert_eq!(parsed[2]["req_id"], req_id);
    parsed[2].clone()
}

struct Fixture {
    store: Arc<EdgeStore>,
    binding: CommunityBinding,
    url: String,
    /// Owns two rows in the shared channel: one delivered, one still waiting.
    alice: Keys,
    /// Owns the quarantined row in the shared channel, which alice may see.
    bob: Keys,
    /// Owns a quarantined row in a channel alice is not a member of.
    carol: Keys,
    /// A freshly generated identity in no channel at all.
    dave: Keys,
    /// The channel alice and bob share.
    shared_channel: Uuid,
    /// The channel only carol belongs to.
    other_channel: Uuid,
    /// Bob's permanently refused event, carrying [`PRIVATE_BODY`].
    quarantined: Event,
    /// Carol's permanently refused event, carrying [`OTHER_CHANNEL_BODY`].
    other_quarantined: Event,
    /// Alice's row that no author is currently draining.
    pending: Event,
    server: tokio::task::JoinHandle<Result<(), EdgeError>>,
}

/// Two channels with real membership: a shared one holding pending, delivered,
/// and quarantined rows, and one alice has no membership in at all.
async fn fixture() -> Fixture {
    let binding =
        CommunityBinding::new("wss://relay.example.com", Uuid::new_v4()).expect("binding");
    let policy = AuthorizationPolicy::new(Duration::from_secs(72 * 60 * 60)).expect("policy");
    let store = Arc::new(EdgeStore::open_in_memory(binding.clone(), policy).expect("store"));
    let edge_keys = Keys::generate();
    let alice = Keys::generate();
    let bob = Keys::generate();
    let carol = Keys::generate();
    let dave = Keys::generate();
    let shared_channel = Uuid::new_v4();
    let other_channel = Uuid::new_v4();

    let now = unix_now();
    authorize(
        &store,
        &edge_keys,
        now,
        &[
            (
                shared_channel,
                vec![alice.public_key(), bob.public_key(), edge_keys.public_key()],
            ),
            (other_channel, vec![carol.public_key()]),
        ],
    );

    let delivered = message(&alice, shared_channel, "alice delivered");
    let pending = message(&alice, shared_channel, "alice pending");
    let quarantined = message(&bob, shared_channel, PRIVATE_BODY);
    for event in [&delivered, &pending, &quarantined] {
        store
            .insert_local_event(
                event,
                event.as_json().as_bytes(),
                shared_channel,
                &edge_keys,
            )
            .expect("insert");
    }
    let other_quarantined = message(&carol, other_channel, OTHER_CHANNEL_BODY);
    store
        .insert_local_event(
            &other_quarantined,
            other_quarantined.as_json().as_bytes(),
            other_channel,
            &edge_keys,
        )
        .expect("insert");

    // Move the rows through the real claim/acknowledge path rather than
    // writing SQL, so the fixture cannot invent a state the sidecar could
    // never produce.
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
    let claimed = store
        .claim_outbox_batch(&carol.public_key(), "carol-setup", 100, now, 60)
        .expect("claim carol");
    assert_eq!(claimed.len(), 1, "carol owns exactly one row");
    assert!(store
        .acknowledge_outbox_row(
            "carol-setup",
            &other_quarantined.id,
            DrainOutcome::Rejected(OTHER_CHANNEL_REASON.to_string()),
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
        carol,
        dave,
        shared_channel,
        other_channel,
        quarantined,
        other_quarantined,
        pending,
        server,
    }
}

#[tokio::test]
async fn status_and_requeue_are_refused_while_local_routing_is_unauthorized() {
    // `local_routing_ready` is false when startup could not restore a valid
    // offline authorization lease. `REQ` and `COUNT` fail closed on it, and the
    // status reads have to as well: otherwise a sidecar that never obtained a
    // lease still answers questions about the outbox.
    let binding =
        CommunityBinding::new("wss://relay.example.com", Uuid::new_v4()).expect("binding");
    let policy = AuthorizationPolicy::new(Duration::from_secs(72 * 60 * 60)).expect("policy");
    let store = Arc::new(EdgeStore::open_in_memory(binding.clone(), policy).expect("store"));
    let edge_keys = Keys::generate();
    let alice = Keys::generate();
    let channel = Uuid::new_v4();
    let now = unix_now();
    authorize(
        &store,
        &edge_keys,
        now,
        &[(channel, vec![alice.public_key()])],
    );
    let queued = message(&alice, channel, "queued");
    store
        .insert_local_event(&queued, queued.as_json().as_bytes(), channel, &edge_keys)
        .expect("insert");

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let url = format!("ws://{address}");
    let relay = EdgeRelay::new(
        EdgeConfig::new(&url, binding.clone()).expect("config"),
        Arc::clone(&store),
        edge_keys,
        false,
    )
    .expect("relay");
    let server = tokio::spawn(run_server(listener, relay));

    let mut client = authenticated_client(&url, &binding, &alice).await;
    send(
        &mut client,
        json!(["BUZZ-EDGE", "STATUS", {"req_id":"blocked-1","limit":50}]),
    )
    .await;
    let frame = next_json(&mut client).await;
    assert_eq!(frame[0], "NOTICE", "expected a refusal, got {frame}");
    assert!(
        frame[1]
            .as_str()
            .expect("notice text")
            .starts_with("restricted:"),
        "{frame}"
    );
    assert!(
        !frame.to_string().contains(&queued.id.to_hex()),
        "a refused status must not carry rows: {frame}"
    );

    send(
        &mut client,
        json!([
            "BUZZ-EDGE", "REQUEUE",
            {"req_id":"blocked-2","event_id":queued.id.to_hex()}
        ]),
    )
    .await;
    let frame = next_json(&mut client).await;
    assert_eq!(frame[0], "NOTICE", "expected a refusal, got {frame}");

    server.abort();
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

    // The privacy invariant, end to end: the quarantined row is bob's, alice
    // shares the channel with him so she is meant to see that it is stuck, and
    // bob's message body is sitting in the same database this reply was built
    // from.
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
    // Carol's row is in a channel alice cannot read, so it is in none of these.
    let summary = &parsed[2]["summary"];
    assert_eq!(summary["pending"], 1);
    assert_eq!(summary["pendingViaDigest"], 0);
    assert_eq!(summary["claimed"], 0);
    assert_eq!(summary["syncedExact"], 1);
    assert_eq!(summary["syncedViaDigest"], 0);
    assert_eq!(
        summary["quarantined"], 1,
        "carol's refused row is counted in her channel, not in alice's view"
    );

    let quarantined = parsed[2]["quarantined"]
        .as_array()
        .expect("quarantine list");
    assert_eq!(quarantined.len(), 1);
    assert_eq!(quarantined[0]["eventId"], fixture.quarantined.id.to_hex());
    assert_eq!(quarantined[0]["author"], fixture.bob.public_key().to_hex());
    assert_eq!(quarantined[0]["reason"], QUARANTINE_REASON);
    assert_eq!(quarantined[0]["carriedByDigest"], false);
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
    assert_eq!(waiting[0]["ancestorBlocked"], 0);
    assert_eq!(waiting[0]["pendingViaDigest"], 0);

    fixture.server.abort();
}

#[tokio::test]
async fn status_shows_nothing_at_all_from_a_channel_the_caller_is_not_in() {
    // Reading status is deliberately not filtered to the *calling identity* —
    // that is what lets the operator see an agent's stuck events. It is still
    // filtered by channel. Without that, an agent scoped to one channel learns
    // a private channel's UUID, who posts there, when, and why a post failed,
    // and `waitingAuthors` becomes a presence oracle over every local identity.
    let fixture = fixture().await;
    let mut alice = authenticated_client(&fixture.url, &fixture.binding, &fixture.alice).await;
    let body = status_body(&mut alice, "scope-1").await;
    let rendered = body.to_string();

    for (label, needle) in [
        ("carol's event ID", fixture.other_quarantined.id.to_hex()),
        (
            "the other channel's UUID",
            fixture.other_channel.to_string(),
        ),
        ("carol's pubkey", fixture.carol.public_key().to_hex()),
        ("carol's failure reason", OTHER_CHANNEL_REASON.to_string()),
        ("carol's message body", OTHER_CHANNEL_BODY.to_string()),
    ] {
        assert!(
            !rendered.contains(&needle),
            "{label} leaked into a status reply for a non-member: {rendered}"
        );
    }

    // Carol's row still exists and is still quarantined, so the assertions
    // above are about the channel gate and not about an empty database.
    let carol_key = fixture.carol.public_key();
    let now = unix_now();
    let carol_rows = fixture
        .store
        .quarantined_rows(&carol_key, now, 500)
        .expect("carol's rows");
    assert_eq!(carol_rows.len(), 1);
    assert_eq!(carol_rows[0].event_id, fixture.other_quarantined.id);
    assert_eq!(carol_rows[0].channel_id, fixture.other_channel);

    // Per-event lookups are the same gate with a smaller payload: asking for an
    // event by ID must not confirm it exists either.
    assert!(
        fixture
            .store
            .event_delivery_states(
                &fixture.alice.public_key(),
                now,
                &[fixture.other_quarantined.id]
            )
            .expect("states")
            .is_empty(),
        "a direct delivery-state lookup must not confirm a foreign channel's event"
    );

    // And an identity in no channel at all sees an empty surface rather than
    // the whole community: authentication is not authorization, and a freshly
    // generated keypair authenticates.
    let mut dave = authenticated_client(&fixture.url, &fixture.binding, &fixture.dave).await;
    let dave_body = status_body(&mut dave, "scope-2").await;
    assert!(dave_body["quarantined"]
        .as_array()
        .expect("quarantine list")
        .is_empty());
    assert!(dave_body["waitingAuthors"]
        .as_array()
        .expect("waiting list")
        .is_empty());
    assert_eq!(dave_body["summary"]["pending"], 0);
    assert_eq!(dave_body["summary"]["quarantined"], 0);
    assert_eq!(dave_body["summary"]["syncedExact"], 0);

    fixture.server.abort();
}

#[tokio::test]
async fn a_requeue_from_another_identity_is_refused_and_leaves_the_row_quarantined() {
    // The authorization boundary of this surface: reading status is scoped by
    // channel, writing is scoped to the row's own author. Alice can see that
    // bob's row is stuck; she must not be able to act on it.
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
    assert_eq!(reply[2]["outcome"], "notFound");

    // The reply is not the assertion — the row is. A requeue moves the row to
    // `pending` and clears `last_error`, so a hole in the ownership filter
    // would show up here even if the reply still said false.
    let now = unix_now();
    let rows = fixture
        .store
        .quarantined_rows(&fixture.bob.public_key(), now, 500)
        .expect("quarantine list");
    let row = rows
        .iter()
        .find(|row| row.event_id == fixture.quarantined.id)
        .expect("bob's row must still be quarantined");
    assert_eq!(row.author, fixture.bob.public_key());
    assert_eq!(row.channel_id, fixture.shared_channel);
    assert_eq!(row.reason, QUARANTINE_REASON);
    assert_eq!(
        row.attempts, 1,
        "a refused requeue must not spend an attempt"
    );
    let states = fixture
        .store
        .event_delivery_states(&fixture.bob.public_key(), now, &[fixture.quarantined.id])
        .expect("delivery states");
    assert_eq!(states.len(), 1);
    assert_eq!(states[0].event_id, fixture.quarantined.id);
    assert_eq!(states[0].state, EventDeliveryState::Quarantined);

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
    assert_eq!(reply[2]["outcome"], "requeued");

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

#[tokio::test]
async fn retry_on_a_demoted_row_is_refused_with_a_reason_instead_of_hiding_it() {
    // Once the demotion pass has moved a quarantined row to the digest path the
    // edge identity carries it upstream. Setting it back to `pending` would
    // drop it out of the quarantine list, out of the exact-path waiting count,
    // and out of the drain queue at once — the button would only delete the
    // operator's view of the problem.
    let fixture = fixture().await;
    assert_eq!(
        fixture
            .store
            .demote_unreplayable_threads(900, unix_now())
            .expect("demote"),
        2,
        "the pass demotes every quarantined exact row: bob's and carol's"
    );

    let mut bob = authenticated_client(&fixture.url, &fixture.binding, &fixture.bob).await;
    send(
        &mut bob,
        json!([
            "BUZZ-EDGE", "REQUEUE",
            {"req_id":"bob-2","event_id":fixture.quarantined.id.to_hex()}
        ]),
    )
    .await;
    let reply = next_json(&mut bob).await;
    assert_eq!(reply[1], "REQUEUE-REPLY");
    assert_eq!(reply[2]["requeued"], false);
    assert_eq!(
        reply[2]["outcome"], "carriedByDigest",
        "the caller has to be able to explain the refusal: {reply}"
    );

    // The row stays visible and stays marked, rather than disappearing.
    let body = status_body(&mut bob, "bob-3").await;
    let quarantined = body["quarantined"].as_array().expect("quarantine list");
    assert_eq!(quarantined.len(), 1);
    assert_eq!(quarantined[0]["eventId"], fixture.quarantined.id.to_hex());
    assert_eq!(quarantined[0]["carriedByDigest"], true);
    assert_eq!(
        quarantined[0]["demotionReason"], "permanently rejected upstream",
        "the operator is told why the row left the exact path"
    );

    // And it is still not claimable, which is what makes the refusal correct.
    send(
        &mut bob,
        json!(["BUZZ-EDGE", "DRAIN", {"claim_token":"bob-token","limit":10}]),
    )
    .await;
    let batch = next_json(&mut bob).await;
    assert!(batch[2]["events"].as_array().expect("events").is_empty());

    fixture.server.abort();
}
