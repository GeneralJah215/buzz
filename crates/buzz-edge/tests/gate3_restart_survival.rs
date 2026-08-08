//! Release gate 3 — restart survival (spec "Test list", gate 3).
//!
//! Restart the sidecar, then reconnect the Desktop client. No outbox loss, and
//! no duplicate canonical events — with event-ID dedup demonstrated rather than
//! inferred from a row count.
//!
//! The sidecar is restarted the only way an in-process harness honestly can:
//! the client sessions are closed, the server task is aborted, the harness drops
//! its relay and store handles, the old listener is proven to refuse new
//! connections, and then a **new** `EdgeStore` connection, a **new**
//! `EdgeRelay`, and a **new** loopback port are built from the file on disk.
//! Every post-restart assertion is made through the new store. Nothing
//! in-memory crosses the boundary, which is the property a real process restart
//! provides and the only one this gate claims.
//!
//! Startup after the restart goes through the real
//! [`buzz_edge::eligibility::apply_startup_policy`] with an unavailable
//! upstream, so the sidecar comes back the way it comes back on the operator's
//! machine at 6am: offline, on its signed lease.
//!
//! "No duplicate canonical events" is demonstrated four ways, because a count
//! alone cannot tell the difference between correct dedup and a write that
//! silently failed:
//!   1. replaying byte-identical signed bytes of an already-*delivered* event
//!      is answered `duplicate`, stores nothing, and fans out nothing;
//!   2. replaying a still-*pending* event does the same and does not create a
//!      second outbox row;
//!   3. the same event arriving back from upstream — which is exactly what the
//!      mirror sees after a successful drain — is `Duplicate` and leaves the
//!      local receipt intact;
//!   4. a **different** event ID carrying **identical content** is accepted and
//!      delivered, which is what proves the dedup key is the event ID and not
//!      the message body.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use nostr::{Event, EventBuilder, JsonUtil, Keys, Kind, Tag, Timestamp};
use serde_json::{json, Value};
use tokio_tungstenite::connect_async;
use uuid::Uuid;

use buzz_edge::eligibility::{apply_startup_policy, AuthorizationStartup, VerificationResult};
use buzz_edge::storage::{CommunityBinding, EdgeStore, EventDeliveryState, InsertOutcome};

mod gate_harness;

use gate_harness as harness;
use harness::ClientSocket;

/// The sidecar's hard-coded drain lease, in seconds.
const DRAIN_LEASE_SECONDS: i64 = 60;

/// How long the gate waits to prove a frame did *not* arrive.
const SILENCE_PROBE: Duration = Duration::from_millis(750);

/// Read the next frame that is not a live `EVENT` delivery.
///
/// Both clients hold live subscriptions, so their own and each other's messages
/// interleave with control replies. Skipping them here keeps the control
/// assertions honest; it never swallows a `CLOSED` or a `NOTICE`, which is what
/// a refusal would arrive as.
async fn next_control_frame(socket: &mut ClientSocket) -> Value {
    loop {
        let frame = harness::next_json(socket).await;
        if frame[0].as_str() == Some("EVENT") {
            continue;
        }
        return frame;
    }
}

/// Claim every drainable row for this session identity over the wire.
async fn drain(socket: &mut ClientSocket, claim_token: &str) -> Vec<Event> {
    harness::send(
        socket,
        json!(["BUZZ-EDGE", "DRAIN", {"claim_token": claim_token, "limit": 100}]),
    )
    .await;
    let frame = next_control_frame(socket).await;
    assert_eq!(frame[1], "DRAIN-BATCH", "expected a drain batch: {frame}");
    assert_eq!(frame[2]["claim_token"], claim_token);
    frame[2]["events"]
        .as_array()
        .expect("drain batch events")
        .iter()
        .map(|value| {
            serde_json::from_value::<Event>(value.clone()).expect("drained event is a valid event")
        })
        .collect()
}

async fn acknowledge(socket: &mut ClientSocket, claim_token: &str, event: &Event, outcome: &str) {
    harness::send(
        socket,
        json!([
            "BUZZ-EDGE", "DRAIN-ACK",
            {
                "claim_token": claim_token,
                "event_id": event.id.to_hex(),
                "outcome": outcome,
            }
        ]),
    )
    .await;
}

fn ids(events: &[Event]) -> BTreeSet<String> {
    events.iter().map(|event| event.id.to_hex()).collect()
}

fn id_set<'a>(events: impl IntoIterator<Item = &'a Event>) -> BTreeSet<String> {
    events.into_iter().map(|event| event.id.to_hex()).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gate3_restart_survival() {
    let directory = tempfile::tempdir().expect("temporary edge data directory");
    let database = directory.path().join("buzz-edge-gate3.sqlite3");
    let binding = CommunityBinding::new("wss://relay.example.com", Uuid::new_v4())
        .expect("community binding");
    let edge_keys = Keys::generate();
    let alice = Keys::generate();
    let bob = Keys::generate();
    let channel = Uuid::new_v4();
    let started_at = harness::unix_now();

    // ── Before the restart ─────────────────────────────────────────────────
    let store = Arc::new(
        EdgeStore::open(&database, binding.clone(), harness::policy()).expect("open edge store"),
    );
    harness::authorize(
        &store,
        &edge_keys,
        started_at,
        &[(
            channel,
            vec![alice.public_key(), bob.public_key(), edge_keys.public_key()],
        )],
    );
    let (first_url, first_relay, first_server) =
        harness::serve(Arc::clone(&store), edge_keys.clone(), true).await;

    let mut alice_socket = harness::authenticated_client(&first_url, &binding, &alice).await;
    let mut bob_socket = harness::authenticated_client(&first_url, &binding, &bob).await;
    assert!(
        harness::subscribe(&mut alice_socket, "alice-before", channel)
            .await
            .is_empty(),
        "a fresh database must replay no history"
    );
    assert!(
        harness::subscribe(&mut bob_socket, "bob-before", channel)
            .await
            .is_empty(),
        "a fresh database must replay no history"
    );

    let mut alice_events = Vec::new();
    for index in 0..3 {
        let event = harness::message(&alice, channel, &format!("alice pre-restart {index}"));
        let ok = harness::submit(&mut alice_socket, &event).await;
        assert_eq!(
            ok[2], true,
            "the sidecar refused an authorized message: {ok}"
        );
        alice_events.push(event);
    }
    let mut bob_events = Vec::new();
    for index in 0..2 {
        let event = harness::message(&bob, channel, &format!("bob pre-restart {index}"));
        let ok = harness::submit(&mut bob_socket, &event).await;
        assert_eq!(
            ok[2], true,
            "the sidecar refused an authorized message: {ok}"
        );
        bob_events.push(event);
    }

    // Partial progress: alice claims all three of her rows and finishes exactly
    // one before the process goes away. Two of hers and both of bob's are left
    // mid-flight, which is the state a restart has to survive.
    let claimed = drain(&mut alice_socket, "alice-before-restart").await;
    assert_eq!(
        ids(&claimed),
        id_set(&alice_events),
        "alice's drain must offer exactly her own rows"
    );
    acknowledge(
        &mut alice_socket,
        "alice-before-restart",
        &alice_events[0],
        "delivered",
    )
    .await;
    let bob_claimed = drain(&mut bob_socket, "bob-before-restart").await;
    assert_eq!(
        ids(&bob_claimed),
        id_set(&bob_events),
        "bob's drain must offer exactly his own rows"
    );

    // The acknowledgment is fire-and-forget on the wire, so wait for it to land
    // rather than racing the restart. Reading the row back is also the proof
    // that the pre-restart state this gate compares against is real.
    let delivered = &alice_events[0];
    let mut settled = false;
    for _ in 0..100 {
        let states = store
            .event_delivery_states(&alice.public_key(), started_at, &[delivered.id])
            .expect("delivery state lookup");
        if states
            .first()
            .is_some_and(|row| row.state == EventDeliveryState::SyncedExact)
        {
            settled = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        settled,
        "the pre-restart acknowledgment never landed, so there is no known state to survive"
    );
    assert_eq!(
        store
            .outbox_summary(&[channel])
            .expect("outbox summary")
            .claimed,
        4,
        "four rows must be mid-flight when the sidecar goes away"
    );
    assert_eq!(
        store.event_count().expect("event count"),
        5,
        "five messages were submitted before the restart"
    );

    let survivors: Vec<Event> = alice_events
        .iter()
        .chain(bob_events.iter())
        .cloned()
        .collect();
    let unfinished: BTreeSet<String> = alice_events[1..]
        .iter()
        .chain(bob_events.iter())
        .map(|event| event.id.to_hex())
        .collect();

    // ── The restart ────────────────────────────────────────────────────────
    drop(alice_socket);
    drop(bob_socket);
    first_server.abort();
    let _ = first_server.await;
    drop(first_relay);
    drop(store);

    let mut refused = false;
    for _ in 0..30 {
        if connect_async(first_url.as_str()).await.is_err() {
            refused = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        refused,
        "the pre-restart sidecar at {first_url} still accepts connections, so nothing was \
         actually restarted"
    );

    // ── After the restart ──────────────────────────────────────────────────
    let store = Arc::new(
        EdgeStore::open(&database, binding.clone(), harness::policy())
            .expect("reopen the edge store after the restart"),
    );
    let restarted_at = harness::unix_now();
    let startup = apply_startup_policy(
        &store,
        &edge_keys,
        restarted_at,
        VerificationResult::Unavailable("gate 3 restarts with upstream unavailable".to_string()),
    )
    .expect("startup policy");
    let AuthorizationStartup::OfflineLease {
        eligible_channels,
        expires_at,
    } = startup
    else {
        panic!(
            "the restarted sidecar did not come back on its signed lease: {startup:?}; \
             every assertion below would then be about a fail-closed sidecar"
        );
    };
    assert_eq!(
        eligible_channels,
        vec![channel],
        "the restored lease must cover the channel the messages are in"
    );
    assert!(
        expires_at > restarted_at,
        "the restored lease is already expired"
    );

    let (url, relay, server) = harness::serve(Arc::clone(&store), edge_keys.clone(), true).await;

    // The author process vanished with the sidecar, so its drain leases are
    // stranded. Sixty-one seconds of clock is applied directly, because the
    // lease length is a sidecar constant and the wire drain reads the real
    // clock. How many rows come back is itself the outbox-loss measurement.
    let reclaimed = store
        .expire_outbox_leases(restarted_at + DRAIN_LEASE_SECONDS + 1)
        .expect("expire stranded leases");
    assert_eq!(
        reclaimed, 4,
        "four rows were mid-flight when the sidecar went away and all four must return \
         to the queue"
    );

    // ── No outbox loss ─────────────────────────────────────────────────────
    let mut alice_socket = harness::authenticated_client(&url, &binding, &alice).await;
    let mut bob_socket = harness::authenticated_client(&url, &binding, &bob).await;
    let history = harness::subscribe(&mut alice_socket, "alice-after", channel).await;
    assert_eq!(
        ids(&history),
        id_set(&survivors),
        "the reconnecting Desktop must be replayed every stored message"
    );
    assert_eq!(
        history.len(),
        survivors.len(),
        "the replay contained a repeated event: {} frames for {} events",
        history.len(),
        survivors.len()
    );
    for event in &history {
        assert!(
            event.verify().is_ok(),
            "a restored event no longer verifies: {}",
            event.id
        );
        let original = survivors
            .iter()
            .find(|candidate| candidate.id == event.id)
            .expect("restored event was one of the submitted events");
        assert_eq!(
            event.as_json(),
            original.as_json(),
            "a restored event is not byte-identical to what was submitted"
        );
    }
    let bob_history = harness::subscribe(&mut bob_socket, "bob-after", channel).await;
    assert_eq!(
        ids(&bob_history),
        id_set(&survivors),
        "the second reconnecting client must be replayed the same stored history"
    );

    let alice_pending = drain(&mut alice_socket, "alice-after-restart").await;
    let bob_pending = drain(&mut bob_socket, "bob-after-restart").await;
    let mut recovered = ids(&alice_pending);
    recovered.extend(ids(&bob_pending));
    assert_eq!(
        recovered, unfinished,
        "the outbox after the restart does not hold exactly the rows that were unfinished \
         before it"
    );
    assert!(
        !recovered.contains(&delivered.id.to_hex()),
        "the row acknowledged delivered before the restart was offered for submission again, \
         which is how a duplicate canonical event gets created"
    );
    assert_eq!(
        alice_pending.len(),
        2,
        "a repeated row in one drain batch would be a duplicate submission"
    );
    assert_eq!(bob_pending.len(), 2);

    // A second claim, with a fresh token, must find nothing: the rows are
    // leased, not re-offered.
    assert!(
        drain(&mut alice_socket, "alice-second-claim")
            .await
            .is_empty(),
        "an already-claimed row was offered to a second drain"
    );

    // ── Event-ID dedup, demonstrated ───────────────────────────────────────
    // 1. The delivered event, replayed byte-for-byte by a Desktop that
    //    reconnected without knowing what it had already sent.
    let replay = harness::submit(&mut alice_socket, delivered).await;
    assert_eq!(
        replay[2], true,
        "a replay must be accepted, not refused: refusing it makes clients retry forever"
    );
    assert_eq!(
        replay[3], "duplicate: already delivered locally",
        "the sidecar must name the replay a duplicate: {replay}"
    );

    // 2. A still-pending event, replayed the same way.
    let replay = harness::submit(&mut alice_socket, &alice_events[1]).await;
    assert_eq!(replay[2], true);
    assert_eq!(replay[3], "duplicate: already delivered locally");

    assert_eq!(
        store.event_count().expect("event count"),
        5,
        "a replayed event ID created a second stored copy"
    );

    // No fan-out for either replay. The probe is only meaningful because the
    // positive control below proves it can see a delivery.
    let silence = tokio::time::timeout(SILENCE_PROBE, bob_socket.next()).await;
    assert!(
        silence.is_err(),
        "a replayed event was fanned out to a peer a second time: {:?}",
        silence.map(|frame| format!("{frame:?}"))
    );

    // 3. The same event coming back from canonical history, which is exactly
    //    what the mirror sees once a drained event lands upstream.
    assert_eq!(
        store
            .insert_upstream_event(delivered, delivered.as_json().as_bytes(), channel)
            .expect("upstream echo"),
        InsertOutcome::Duplicate,
        "the sidecar's own event, echoed back by canonical history, must dedup on its ID"
    );
    assert_eq!(
        store.event_count().expect("event count"),
        5,
        "the upstream echo created a second stored copy"
    );
    assert!(
        store
            .receipt(&delivered.id)
            .expect("receipt lookup")
            .is_some(),
        "the upstream echo replaced the locally authored row and lost its delivery receipt"
    );

    // 4. The dedup key is the event ID, not the message body — and the probe
    //    above can in fact see a delivery.
    let twin = EventBuilder::new(Kind::Custom(9), delivered.content.clone())
        .tags([Tag::parse(["h", channel.to_string().as_str()]).expect("h tag")])
        .custom_created_at(Timestamp::from(delivered.created_at.as_secs() + 1))
        .sign_with_keys(&alice)
        .expect("sign the identical-content twin");
    assert_ne!(
        twin.id, delivered.id,
        "the twin must be a different event ID for this to test anything"
    );
    assert_eq!(twin.content, delivered.content);
    let accepted = harness::submit(&mut alice_socket, &twin).await;
    assert_eq!(accepted[2], true);
    assert_eq!(
        accepted[3], "delivered locally",
        "identical content under a new event ID must be accepted as new: {accepted}"
    );
    let delivered_frame = tokio::time::timeout(SILENCE_PROBE, harness::next_json(&mut bob_socket))
        .await
        .expect("the peer must receive the new event, or the silence probe above proves nothing");
    assert_eq!(delivered_frame[0], "EVENT");
    assert_eq!(delivered_frame[2]["id"], twin.id.to_hex());
    assert_eq!(
        store.event_count().expect("event count"),
        6,
        "a distinct event ID must be stored as a distinct event"
    );

    println!("[gate3] messages submitted before the restart: 5");
    println!("[gate3] rows acknowledged delivered before the restart: 1");
    println!("[gate3] rows mid-flight at the restart: 4");
    println!("[gate3] rows reclaimed by lease expiry after the restart: {reclaimed}");
    println!(
        "[gate3] events replayed to the reconnecting Desktop: {}",
        history.len()
    );
    println!(
        "[gate3] rows re-offered for submission after the restart: {}",
        recovered.len()
    );
    println!("[gate3] byte-identical replays answered duplicate: 2");
    println!("[gate3] upstream echoes answered duplicate: 1");
    println!(
        "[gate3] events stored at the end: {}",
        store.event_count().expect("event count")
    );

    server.abort();
    drop(relay);
}
