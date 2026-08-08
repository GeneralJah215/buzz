//! Release gate 2 — slow-upstream transport SLO (spec "Test list", gate 2).
//!
//! The operator's actual failure mode is not a severed relay; it is a relay
//! that answers eventually. This gate stands up a **reachable** canonical relay
//! that completes its TCP connection, its WebSocket upgrade, and its NIP-42
//! handshake, and then takes [`UPSTREAM_DELAY`] to answer any `EVENT` or `REQ`,
//! runs the real upstream mirror against it, and measures local delivery while
//! the mirror sits in steady state with an unanswerable subscription.
//!
//! # Measurement boundary, and why it is not the spec's literal wording
//!
//! The spec fixes the boundary as "from the sidecar returning `OK` for the
//! submitted event to each subscribed peer client receiving the event frame".
//! That wording assumes the sidecar acknowledges first and fans out second.
//! **The implementation is the reverse.** `EdgeRelay::accept_event`
//! (`src/lib.rs`) awaits `fan_out(...)` to completion and only then returns the
//! tuple that becomes the `OK` frame. Signature verification, the store insert,
//! the per-subscriber access check, and the enqueue with its 250 ms timeout all
//! happen *before* the `OK` exists.
//!
//! Measured literally, then, `OK → peer delivery` is
//! "peer's socket read of a frame already written, minus the submitter's socket
//! read of a frame written after it": harness scheduling jitter, negative by
//! construction, and vacuous as an SLO. It is green on the exact regression the
//! gate exists to catch — inserting a 400 ms sleep at the top of `fan_out`
//! shifts the peers' `EVENT` frames and the submitter's `OK` 400 ms later
//! *together*, leaving p95 and max near zero.
//!
//! So the gate asserts the spec's **budgets** (p95 ≤ 250 ms, max ≤ 1 s) against
//! the interval the spec was reaching for: **submit → peer delivery**, from the
//! submitter writing its `EVENT` frame to each subscribed peer reading the
//! delivered frame. That interval strictly contains everything the literal
//! boundary meant to cover, and it is the number a human waiting for a message
//! to appear on the other screen actually experiences.
//!
//! `OK → peer delivery` is still computed and printed, as a **diagnostic**: it
//! is the direct evidence of the acknowledge-after-fan-out ordering, and its
//! samples are kept **signed** rather than clamped, because a negative sample is
//! real information and a `saturating_sub` would hide it. It is not asserted on.
//! Do not "fix" the boundary back to the spec's literal wording without first
//! changing `accept_event` to acknowledge before it fans out.
//!
//! # What makes this more than a stopwatch
//!
//! The upstream state the numbers were collected under is asserted, not
//! assumed. A gate whose mirror never connected — or one whose mirror parked
//! inside its handshake, which is where an AUTH-delaying stub leaves it — would
//! measure a sidecar with no live upstream at all and would still be green. So
//! the gate requires all of:
//!
//! - the mirror established an upstream session (reachable, not cut);
//! - the mirror got **past** the handshake: it sent frames upstream and
//!   registered a real subscription;
//! - the mirror reached its **live read loop** and stayed there — upstream
//!   pushes mirrored kind-9 events on a second eligible channel throughout the
//!   run and the gate requires them to land in the local store. Every one of
//!   those takes the same global `sequence` mutex and the same
//!   `authorization_epoch` read lock that every local send takes, so the
//!   measurement happens under real contention with the upstream path. A
//!   connect-and-sleep stub cannot satisfy this;
//! - upstream completed **zero** `EVENT` acknowledgments and **zero** `EOSE`
//!   inside the window, and received zero `EVENT` frames, so nothing drained;
//! - every accepted local event is still sitting pending in the outbox.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{Message as UpstreamFrame, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use nostr::{Event, EventBuilder, Keys, Kind, Tag};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as ClientFrame;
use uuid::Uuid;

use buzz_edge::storage::{CommunityBinding, EdgeStore};
use buzz_edge::upstream::run_upstream_mirror;

mod gate_harness;

use gate_harness as harness;
use harness::ClientSocket;

/// Artificial upstream delay applied to every canonical-relay `EVENT` and `REQ`
/// response.
///
/// The spec asks for at least ten seconds. Five minutes is used so the delay is
/// still outstanding, unfulfilled, when the gate finishes asserting — the whole
/// measurement window then sits strictly inside one un-answered upstream
/// operation, which is a stronger statement than "ten seconds of slowness
/// happened somewhere during the run".
///
/// The margin is this large on purpose. A real fan-out regression stretches the
/// run: a 400 ms delay per delivery turns an 11-second window into an
/// 80-second one. With a 60-second delay the stalled `EOSE` lands *during* that
/// stretched window and the gate fails on "upstream answered" instead of on the
/// SLO it exists to protect — a red gate, but pointing at the wrong thing.
/// Nothing waits on this constant, so buying the margin costs nothing.
///
/// The NIP-42 handshake is deliberately **not** delayed. A relay that never
/// answers its handshake parks the mirror inside `connect_authenticated`, where
/// none of its post-connect code — and therefore none of the lock contention
/// this gate cares about — ever runs.
const UPSTREAM_DELAY: Duration = Duration::from_secs(300);

/// Messages exchanged and discarded before measurement starts.
const WARMUP_MESSAGES: usize = 30;

/// Spec minimum sample size: at least 100 warmed kind-9 messages.
const MIN_MEASURED_MESSAGES: usize = 100;

/// Minimum measured wall-clock window, comfortably over the spec's ten seconds.
const MIN_MEASUREMENT_WINDOW: Duration = Duration::from_secs(11);

/// Spacing between submissions — a conversation between three participants,
/// not a flood. Throughput is not what this gate is about.
const SEND_INTERVAL: Duration = Duration::from_millis(20);

/// Spec SLO: p95 of sidecar-ingress-to-peer-delivery latency.
const P95_BUDGET: Duration = Duration::from_millis(250);

/// Spec SLO: maximum sidecar-ingress-to-peer-delivery latency.
const MAX_BUDGET: Duration = Duration::from_secs(1);

/// How long any single frame may be waited for before the gate fails loudly.
const FRAME_TIMEOUT: Duration = Duration::from_secs(5);

/// Spacing of the mirrored kind-9 events upstream pushes down the mirror's live
/// subscription. A steady trickle, not a flood: the point is to prove the
/// mirror is in its read loop and contending for the shared locks, not to
/// benchmark upstream ingest.
const MIRROR_FEED_INTERVAL: Duration = Duration::from_millis(50);

/// How many mirrored events must have been ingested by the end of the run.
///
/// Deliberately far below what [`MIRROR_FEED_INTERVAL`] yields over the
/// measurement window, so a slow machine cannot fail the gate for the wrong
/// reason, and far above zero, so a mirror that never reached its read loop
/// cannot pass it.
const MIN_MIRRORED_EVENTS: u64 = 25;

// ── The slow canonical relay ────────────────────────────────────────────────

/// Counters describing exactly what the canonical relay did during the run.
#[derive(Clone)]
struct SlowUpstream {
    /// The signing identity the relay advertises in its NIP-11 document.
    identity: String,
    /// WebSocket sessions the sidecar successfully established.
    connections: Arc<AtomicU64>,
    /// Frames the sidecar sent upstream.
    frames_received: Arc<AtomicU64>,
    /// `EVENT` frames the sidecar sent upstream.
    events_received: Arc<AtomicU64>,
    /// Subscription ids the sidecar registered with a `REQ`.
    subscriptions: Arc<Mutex<Vec<String>>>,
    /// Handshake responses: the `AUTH` challenge and its `OK`. Answered
    /// immediately — a slow relay answers the handshake and is slow later.
    handshake_responses: Arc<AtomicU64>,
    /// HTTP bridge backfill responses. Answered immediately and empty: this
    /// relay has no canonical history, it is slow on the event path.
    backfill_responses: Arc<AtomicU64>,
    /// `EVENT` acknowledgments and `EOSE` frames upstream managed to complete
    /// after waiting [`UPSTREAM_DELAY`]. Must be zero for the whole run.
    stalled_responses: Arc<AtomicU64>,
    /// HTTP bridge backfill requests.
    query_requests: Arc<AtomicU64>,
    /// NIP-11 relay-identity document requests.
    identity_requests: Arc<AtomicU64>,
    /// Identity the mirrored kind-9 feed is authored under.
    feed_keys: Keys,
    /// Channel the mirrored feed targets — edge-eligible, and deliberately not
    /// one any client in this gate subscribes to, so mirroring contends for the
    /// sidecar's locks without perturbing the measured deliveries.
    feed_channel: Uuid,
    /// Cleared when the measurement window closes.
    feed_running: Arc<AtomicBool>,
    /// Mirrored events pushed down the live subscription.
    feed_sent: Arc<AtomicU64>,
}

impl SlowUpstream {
    fn new(identity: &nostr::PublicKey, feed_channel: Uuid) -> Self {
        Self {
            identity: identity.to_hex(),
            connections: Arc::default(),
            frames_received: Arc::default(),
            events_received: Arc::default(),
            subscriptions: Arc::new(Mutex::new(Vec::new())),
            handshake_responses: Arc::default(),
            backfill_responses: Arc::default(),
            stalled_responses: Arc::default(),
            query_requests: Arc::default(),
            identity_requests: Arc::default(),
            feed_keys: Keys::generate(),
            feed_channel,
            feed_running: Arc::new(AtomicBool::new(true)),
            feed_sent: Arc::default(),
        }
    }

    fn registered_subscriptions(&self) -> Vec<String> {
        self.subscriptions.lock().expect("subscription log").clone()
    }
}

/// The shared write half of one upstream session.
type UpstreamSink = Arc<tokio::sync::Mutex<SplitSink<WebSocket, UpstreamFrame>>>;

/// The canonical origin serves two things on `/`: the WebSocket relay, and —
/// for a plain `GET` — the NIP-11 identity document the eligibility check reads
/// before it will trust a roster.
///
/// Serving the document matters even though this gate is about latency. Without
/// it the check fails *definitively* rather than transiently, the sidecar
/// revokes its authorization snapshot, and local routing shuts down — which
/// would make this gate's numbers depend on how long it happened to run before
/// the mirror gave up. A slow relay is not a hostile one.
async fn slow_root(
    State(state): State<SlowUpstream>,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    match upgrade {
        Ok(upgrade) => upgrade.on_upgrade(move |socket| slow_session(state, socket)),
        Err(_) => {
            state.identity_requests.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(UPSTREAM_DELAY).await;
            state.stalled_responses.fetch_add(1, Ordering::SeqCst);
            axum::response::IntoResponse::into_response(Json(
                json!({"self": state.identity.clone()}),
            ))
        }
    }
}

/// Reachable, authenticated **immediately**, and answering no `EVENT` or `REQ`
/// for a long time.
///
/// The handshake completes at once and the mirror proceeds into its live read
/// loop, which is the only state in which the upstream path competes for the
/// sidecar's `sequence` mutex and `authorization_epoch` lock. Everything after
/// the handshake — the `EOSE` closing the mirror's subscription, the `OK` for
/// any drained event — waits [`UPSTREAM_DELAY`] and therefore never arrives
/// inside this gate's window.
async fn slow_session(state: SlowUpstream, socket: WebSocket) {
    state.connections.fetch_add(1, Ordering::SeqCst);
    let (sink, mut stream) = socket.split();
    let sink: UpstreamSink = Arc::new(tokio::sync::Mutex::new(sink));

    if send_upstream(&sink, json!(["AUTH", "slow-upstream-challenge"]))
        .await
        .is_err()
    {
        return;
    }
    state.handshake_responses.fetch_add(1, Ordering::SeqCst);

    while let Some(Ok(frame)) = stream.next().await {
        let UpstreamFrame::Text(raw) = frame else {
            continue;
        };
        state.frames_received.fetch_add(1, Ordering::SeqCst);
        let Ok(parsed) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        let verb = parsed
            .get(0)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        match verb.as_str() {
            "AUTH" => {
                let event_id = parsed[1]["id"].as_str().unwrap_or_default().to_string();
                if send_upstream(&sink, json!(["OK", event_id, true, ""]))
                    .await
                    .is_err()
                {
                    return;
                }
                state.handshake_responses.fetch_add(1, Ordering::SeqCst);
            }
            "REQ" => {
                let subscription_id = parsed[1].as_str().unwrap_or_default().to_string();
                state
                    .subscriptions
                    .lock()
                    .expect("subscription log")
                    .push(subscription_id.clone());
                spawn_mirror_feed(state.clone(), Arc::clone(&sink), subscription_id.clone());
                spawn_stalled_response(
                    state.clone(),
                    Arc::clone(&sink),
                    json!(["EOSE", subscription_id]),
                );
            }
            "EVENT" => {
                state.events_received.fetch_add(1, Ordering::SeqCst);
                let event_id = parsed[1]["id"].as_str().unwrap_or_default().to_string();
                spawn_stalled_response(
                    state.clone(),
                    Arc::clone(&sink),
                    json!(["OK", event_id, true, ""]),
                );
            }
            _ => {}
        }
    }
}

async fn send_upstream(sink: &UpstreamSink, frame: Value) -> Result<(), ()> {
    sink.lock()
        .await
        .send(UpstreamFrame::Text(frame.to_string().into()))
        .await
        .map_err(|_| ())
}

/// Queue one response that only completes after [`UPSTREAM_DELAY`].
///
/// Spawned rather than awaited inline so the relay keeps *reading* while it
/// stalls: a frame the sidecar sends is counted the moment it arrives, not when
/// the relay gets around to answering it. Counting on the read side is what
/// makes "upstream received zero `EVENT` frames" a claim about the sidecar
/// rather than about this stub's scheduling.
fn spawn_stalled_response(state: SlowUpstream, sink: UpstreamSink, frame: Value) {
    tokio::spawn(async move {
        tokio::time::sleep(UPSTREAM_DELAY).await;
        if send_upstream(&sink, frame).await.is_ok() {
            state.stalled_responses.fetch_add(1, Ordering::SeqCst);
        }
    });
}

/// Push mirrored kind-9 events down a registered subscription until the
/// measurement window closes.
fn spawn_mirror_feed(state: SlowUpstream, sink: UpstreamSink, subscription_id: String) {
    tokio::spawn(async move {
        let mut index = 0usize;
        while state.feed_running.load(Ordering::SeqCst) {
            let event = EventBuilder::new(Kind::Custom(9), format!("upstream mirror feed {index}"))
                .tags([Tag::parse(["h", state.feed_channel.to_string().as_str()]).expect("h tag")])
                .sign_with_keys(&state.feed_keys)
                .expect("sign a mirrored upstream event");
            index += 1;
            if send_upstream(&sink, json!(["EVENT", subscription_id, event]))
                .await
                .is_err()
            {
                return;
            }
            state.feed_sent.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(MIRROR_FEED_INTERVAL).await;
        }
    });
}

/// The HTTP bridge answers immediately, with no history.
///
/// A stalled bridge would leave the mirror parked inside `backfill_channels`
/// for the whole run — post-connect, but never in the read loop, and never
/// touching a shared lock. Answering it is what lets the mirror reach steady
/// state; the injected slowness that matters is on the `EVENT`/`REQ` path,
/// because that is the path a local send could conceivably wait on.
async fn slow_query(
    State(state): State<SlowUpstream>,
    _body: axum::body::Bytes,
) -> Json<Vec<Event>> {
    state.query_requests.fetch_add(1, Ordering::SeqCst);
    state.backfill_responses.fetch_add(1, Ordering::SeqCst);
    Json(Vec::new())
}

// ── Client plumbing ─────────────────────────────────────────────────────────

/// One peer receiving one event frame.
struct Delivery {
    client: usize,
    event_id: String,
    at: Instant,
}

/// One `OK` acknowledgment.
struct Ack {
    event_id: String,
    accepted: bool,
    message: String,
    at: Instant,
}

/// A connected, authenticated, subscribed participant.
struct Participant {
    keys: Keys,
    sink: SplitSink<ClientSocket, ClientFrame>,
    acks: mpsc::UnboundedReceiver<Ack>,
}

/// Per-message latency samples.
#[derive(Default)]
struct Collected {
    /// The asserted quantity: submitter writes `EVENT` → peer reads it.
    submit_to_delivery: Vec<i128>,
    /// Diagnostic only: submitter reads `OK` → peer reads the event.
    ok_to_delivery: Vec<i128>,
}

/// Timestamp every inbound frame the moment it is read off the socket.
///
/// Anything that is neither an `OK` nor an `EVENT` — a `CLOSED` subscription,
/// a `NOTICE` — is recorded as an anomaly and asserted on at the end, so a run
/// whose subscriptions were quietly torn down cannot look like a fast run.
fn spawn_reader(
    client: usize,
    mut stream: SplitStream<ClientSocket>,
    acks: mpsc::UnboundedSender<Ack>,
    deliveries: mpsc::UnboundedSender<Delivery>,
    anomalies: Arc<Mutex<Vec<String>>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(Ok(frame)) = stream.next().await {
            let at = Instant::now();
            let ClientFrame::Text(raw) = frame else {
                continue;
            };
            let Ok(parsed) = serde_json::from_str::<Value>(&raw) else {
                anomalies
                    .lock()
                    .expect("anomaly log")
                    .push(format!("client {client} received non-JSON frame: {raw}"));
                continue;
            };
            match parsed.get(0).and_then(Value::as_str) {
                Some("OK") => {
                    let _ = acks.send(Ack {
                        event_id: parsed[1].as_str().unwrap_or_default().to_string(),
                        accepted: parsed[2].as_bool().unwrap_or(false),
                        message: parsed[3].as_str().unwrap_or_default().to_string(),
                        at,
                    });
                }
                Some("EVENT") => {
                    let _ = deliveries.send(Delivery {
                        client,
                        event_id: parsed[2]["id"].as_str().unwrap_or_default().to_string(),
                        at,
                    });
                }
                _ => anomalies
                    .lock()
                    .expect("anomaly log")
                    .push(format!("client {client} received {raw}")),
            }
        }
    })
}

/// Nearest-rank percentile over an ascending sample.
fn percentile(sorted: &[i128], percentile: usize) -> i128 {
    assert!(!sorted.is_empty(), "percentile of an empty sample");
    let rank = (percentile * sorted.len()).div_ceil(100);
    sorted[rank.clamp(1, sorted.len()) - 1]
}

fn millis(nanos: i128) -> f64 {
    nanos as f64 / 1_000_000.0
}

/// `min`, `p50`, `p95`, `max` of an unsorted sample.
fn summary(samples: &[i128]) -> (i128, i128, i128, i128) {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    (
        sorted[0],
        percentile(&sorted, 50),
        percentile(&sorted, 95),
        *sorted.last().expect("at least one sample"),
    )
}

// ── The gate ────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gate2_slow_upstream_transport_slo() {
    // The canonical relay has to exist before the community binding does: the
    // binding pins the origin the mirror will dial, and the port is only known
    // once the listener is bound.
    let relay_identity = Keys::generate();
    let channel = Uuid::new_v4();
    let mirror_channel = Uuid::new_v4();
    let upstream = SlowUpstream::new(&relay_identity.public_key(), mirror_channel);
    let upstream_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the slow canonical relay");
    let upstream_address = upstream_listener
        .local_addr()
        .expect("slow canonical relay address");
    let upstream_app = Router::new()
        .route("/", get(slow_root))
        .route("/query", post(slow_query))
        .with_state(upstream.clone());
    let upstream_server = tokio::spawn(async move {
        let _ = axum::serve(upstream_listener, upstream_app).await;
    });

    let binding = CommunityBinding::new(&format!("ws://{upstream_address}"), Uuid::new_v4())
        .expect("community binding");
    let store = Arc::new(
        EdgeStore::open_in_memory(binding.clone(), harness::policy()).expect("edge store"),
    );
    let edge_keys = Keys::generate();

    // Desktop plus two agent clients, as the spec's scenario describes.
    let identities = [Keys::generate(), Keys::generate(), Keys::generate()];
    let mut members: Vec<_> = identities.iter().map(Keys::public_key).collect();
    members.push(edge_keys.public_key());
    let now = harness::unix_now();
    harness::authorize_as(
        &store,
        &edge_keys,
        &relay_identity,
        now,
        &[
            (channel, members),
            // The mirrored feed's channel is eligible and has no local clients.
            (
                mirror_channel,
                vec![edge_keys.public_key(), upstream.feed_keys.public_key()],
            ),
        ],
    );
    assert_eq!(
        store
            .relay_identity(&edge_keys.public_key())
            .expect("pinned relay identity"),
        relay_identity.public_key(),
        "the mirror pins the roster signer; without it the mirror never dials"
    );

    let (url, relay, server) = harness::serve(Arc::clone(&store), edge_keys.clone(), true).await;

    // The real mirror, against the real slow relay. Nothing here is stubbed on
    // the sidecar side.
    let mirror = tokio::spawn(run_upstream_mirror(relay.clone(), edge_keys.clone(), true));

    // Preconditions, asserted rather than assumed. A gate that measured a
    // sidecar with no live upstream would be measuring the wrong system and
    // would still be green — and "no live upstream" includes a mirror parked
    // inside its handshake, which is what a stub that delays the AUTH challenge
    // produces.
    let connect_deadline = Instant::now() + Duration::from_secs(15);
    while upstream.connections.load(Ordering::SeqCst) == 0 {
        assert!(
            Instant::now() < connect_deadline,
            "the upstream mirror never opened a session against the slow canonical relay \
             at ws://{upstream_address}; this gate requires a reachable-but-slow upstream, \
             not an absent one"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let steady_state_deadline = Instant::now() + Duration::from_secs(30);
    while store.event_count().expect("event count") == 0 {
        assert!(
            Instant::now() < steady_state_deadline,
            "the upstream mirror never ingested a mirrored event, so it never reached its \
             live read loop: it authenticated ({} handshake responses), sent {} frames, \
             registered {:?} subscriptions, and upstream pushed {} mirrored events at it. \
             A mirror that has not reached its read loop never touches the sequence mutex \
             or the authorization epoch lock, so 'no send was blocked' below would be a \
             statement about a sidecar with no live upstream contention at all",
            upstream.handshake_responses.load(Ordering::SeqCst),
            upstream.frames_received.load(Ordering::SeqCst),
            upstream.registered_subscriptions(),
            upstream.feed_sent.load(Ordering::SeqCst),
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let anomalies = Arc::new(Mutex::new(Vec::new()));
    let (delivery_tx, mut deliveries) = mpsc::unbounded_channel::<Delivery>();
    let mut participants = Vec::new();
    let mut readers = Vec::new();
    for (index, keys) in identities.iter().enumerate() {
        let mut socket = harness::authenticated_client(&url, &binding, keys).await;
        let history = harness::subscribe(&mut socket, &format!("gate2-{index}"), channel).await;
        assert!(
            history.is_empty(),
            "the measured channel is fresh; a non-empty backfill means upstream history \
             leaked into the channel this gate measures"
        );
        let (sink, stream) = socket.split();
        let (ack_tx, ack_rx) = mpsc::unbounded_channel::<Ack>();
        readers.push(spawn_reader(
            index,
            stream,
            ack_tx,
            delivery_tx.clone(),
            Arc::clone(&anomalies),
        ));
        participants.push(Participant {
            keys: keys.clone(),
            sink,
            acks: ack_rx,
        });
    }
    drop(delivery_tx);

    let peers = participants.len() - 1;
    let mut sequence = 0usize;

    // ── Warm-up ────────────────────────────────────────────────────────────
    for _ in 0..WARMUP_MESSAGES {
        exchange(
            &mut participants,
            &mut deliveries,
            channel,
            &mut sequence,
            None,
        )
        .await;
    }

    // ── Measurement ────────────────────────────────────────────────────────
    let mut samples: Vec<i128> = Vec::new();
    let mut ok_samples: Vec<i128> = Vec::new();
    let mut ack_samples: Vec<i128> = Vec::new();
    let mut measured_messages = 0usize;
    let measurement_start = Instant::now();
    while measured_messages < MIN_MEASURED_MESSAGES
        || measurement_start.elapsed() < MIN_MEASUREMENT_WINDOW
    {
        let mut collected = Collected::default();
        let ack_latency = exchange(
            &mut participants,
            &mut deliveries,
            channel,
            &mut sequence,
            Some(&mut collected),
        )
        .await;
        assert_eq!(
            collected.submit_to_delivery.len(),
            peers,
            "expected one delivery per peer for each message"
        );
        samples.extend(collected.submit_to_delivery);
        ok_samples.extend(collected.ok_to_delivery);
        ack_samples.push(ack_latency);
        measured_messages += 1;
        tokio::time::sleep(SEND_INTERVAL).await;
    }
    let window = measurement_start.elapsed();

    // Snapshot upstream the instant the window closes. Reporting and asserting
    // on counters read later would describe the state at assertion time, not
    // the state the measurements were taken under. The mirrored feed is stopped
    // first and given a moment to quiesce so the stored-event arithmetic below
    // is over a settled store rather than a moving one.
    upstream.feed_running.store(false, Ordering::SeqCst);
    tokio::time::sleep(MIRROR_FEED_INTERVAL * 8).await;
    let upstream_connections = upstream.connections.load(Ordering::SeqCst);
    let upstream_frames = upstream.frames_received.load(Ordering::SeqCst);
    let upstream_events = upstream.events_received.load(Ordering::SeqCst);
    let upstream_handshake = upstream.handshake_responses.load(Ordering::SeqCst);
    let upstream_backfill = upstream.backfill_responses.load(Ordering::SeqCst);
    let upstream_stalled = upstream.stalled_responses.load(Ordering::SeqCst);
    let upstream_queries = upstream.query_requests.load(Ordering::SeqCst);
    let upstream_identity = upstream.identity_requests.load(Ordering::SeqCst);
    let upstream_subscriptions = upstream.registered_subscriptions();
    let mirrored_sent = upstream.feed_sent.load(Ordering::SeqCst);
    let pending_rows = store.pending_count().expect("pending count");
    let stored_events = store.event_count().expect("event count");
    let total_messages = (WARMUP_MESSAGES + measured_messages) as u64;
    let mirrored_stored = stored_events
        .checked_sub(total_messages)
        .unwrap_or_else(|| {
            panic!(
                "the store holds {stored_events} events but {total_messages} local messages were \
             submitted; a local message was lost"
            )
        });

    // ── The report (counts and measurements, not a verdict) ────────────────
    let (min, p50, p95, max) = summary(&samples);
    let (ok_min, ok_p50, ok_p95, ok_max) = summary(&ok_samples);
    let (_, _, ack_p95, ack_max) = summary(&ack_samples);

    println!("[gate2] injected upstream delay: {UPSTREAM_DELAY:?} per EVENT/REQ response");
    println!("[gate2] upstream sessions established: {upstream_connections}");
    println!("[gate2] upstream handshake responses completed: {upstream_handshake}");
    println!("[gate2] upstream frames received from the sidecar: {upstream_frames}");
    println!(
        "[gate2] upstream subscriptions registered by the mirror: {}",
        upstream_subscriptions.len()
    );
    println!("[gate2] upstream EVENT frames received: {upstream_events}");
    println!("[gate2] upstream HTTP bridge requests: {upstream_queries}");
    println!("[gate2] upstream HTTP bridge responses completed: {upstream_backfill}");
    println!("[gate2] upstream identity-document requests: {upstream_identity}");
    println!("[gate2] upstream stalled responses completed (EVENT OK / EOSE): {upstream_stalled}");
    println!("[gate2] mirrored events pushed by upstream: {mirrored_sent}");
    println!("[gate2] mirrored events ingested by the sidecar: {mirrored_stored}");
    println!("[gate2] warm-up messages (discarded): {WARMUP_MESSAGES}");
    println!("[gate2] measured messages: {measured_messages}");
    println!("[gate2] measured window: {:.3} s", window.as_secs_f64());
    println!("[gate2] client connections: {}", participants.len());
    println!("[gate2] submit-to-peer-delivery samples: {}", samples.len());
    println!("[gate2] submit-to-peer-delivery min: {:.3} ms", millis(min));
    println!("[gate2] submit-to-peer-delivery p50: {:.3} ms", millis(p50));
    println!("[gate2] submit-to-peer-delivery p95: {:.3} ms", millis(p95));
    println!("[gate2] submit-to-peer-delivery max: {:.3} ms", millis(max));
    println!("[gate2] submit-to-OK p95: {:.3} ms", millis(ack_p95));
    println!("[gate2] submit-to-OK max: {:.3} ms", millis(ack_max));
    println!(
        "[gate2] diagnostic, not asserted — OK-to-peer-delivery min/p50/p95/max: \
         {:.3} / {:.3} / {:.3} / {:.3} ms (negative because accept_event fans out before it \
         acknowledges; see this file's header)",
        millis(ok_min),
        millis(ok_p50),
        millis(ok_p95),
        millis(ok_max)
    );
    println!("[gate2] outbox rows still pending: {pending_rows}");
    println!("[gate2] events stored: {stored_events}");

    // ── Assertions ─────────────────────────────────────────────────────────
    let recorded_anomalies = anomalies.lock().expect("anomaly log").clone();
    assert!(
        recorded_anomalies.is_empty(),
        "the sidecar sent frames that are neither OK nor EVENT during the run; a torn-down \
         subscription would otherwise look like a fast one: {recorded_anomalies:?}"
    );

    assert!(
        measured_messages >= MIN_MEASURED_MESSAGES,
        "the spec requires at least {MIN_MEASURED_MESSAGES} measured messages; got {measured_messages}"
    );
    assert!(
        window >= MIN_MEASUREMENT_WINDOW,
        "the measurement window was {window:?}, shorter than the required {MIN_MEASUREMENT_WINDOW:?}"
    );
    assert_eq!(
        samples.len(),
        measured_messages * peers,
        "every measured message must produce one sample per subscribed peer"
    );

    // The spec's SLO comes first, deliberately. Every counter is printed above,
    // so nothing is hidden by the ordering — but a regression in the quantity
    // this gate exists to protect must be the failure a reader sees, not a
    // knock-on precondition that the regression itself pushed out of range.
    // The preconditions below still run on a green SLO, so a fast-but-
    // meaningless run cannot pass.
    //
    // Measured over submit → peer delivery. See this file's header for why that
    // is the interval and not the spec's literal OK → peer delivery wording.
    assert!(
        p95 <= P95_BUDGET.as_nanos() as i128,
        "sidecar-submit-to-peer-delivery p95 was {:.3} ms, over the {:.0} ms budget \
         (p50 {:.3} ms, max {:.3} ms, n={})",
        millis(p95),
        P95_BUDGET.as_millis(),
        millis(p50),
        millis(max),
        samples.len()
    );
    assert!(
        max <= MAX_BUDGET.as_nanos() as i128,
        "sidecar-submit-to-peer-delivery max was {:.3} ms, over the {:.0} ms budget \
         (p50 {:.3} ms, p95 {:.3} ms, n={})",
        millis(max),
        MAX_BUDGET.as_millis(),
        millis(p50),
        millis(p95),
        samples.len()
    );

    // The upstream state the SLO numbers were collected under. Without these,
    // the numbers above describe an unknown system.
    assert!(
        upstream_connections >= 1,
        "upstream must be reachable, not cut"
    );
    assert!(
        upstream_frames >= 1,
        "the mirror sent nothing upstream, so it never got past its handshake; a stub that \
         opens a socket and sleeps would produce exactly this"
    );
    assert!(
        !upstream_subscriptions.is_empty(),
        "the mirror registered no subscription upstream, so it never got past its handshake \
         into the code this gate claims to measure alongside"
    );
    assert!(
        mirrored_stored >= MIN_MIRRORED_EVENTS,
        "only {mirrored_stored} mirrored events were ingested (upstream pushed {mirrored_sent}); \
         the mirror was not live in its read loop for the measured window, so the local sends \
         below never contended with the upstream path for the sequence mutex"
    );
    assert!(
        mirrored_stored <= mirrored_sent,
        "the store holds {mirrored_stored} mirrored events but upstream only pushed \
         {mirrored_sent}"
    );
    assert_eq!(
        upstream_stalled, 0,
        "the canonical relay completed a stalled EVENT/EOSE response inside the measurement \
         window, so the run did not measure a stalled upstream"
    );
    assert_eq!(
        upstream_events, 0,
        "the sidecar submitted events upstream during the window, so 'still pending' below \
         would not mean what it says"
    );
    assert_eq!(
        pending_rows, total_messages,
        "every message must still be awaiting upstream: nothing was acknowledged, so no send \
         could have been waiting on an acknowledgment"
    );
    // The submit-to-OK leg is bounded by the spec's own one-second maximum
    // rather than by a number invented here. It is not a second SLO: it is the
    // shortest bound that a send genuinely waiting on this run's upstream
    // ({UPSTREAM_DELAY:?}) could not possibly satisfy. The per-frame timeout
    // above is the primary catch; this is the backstop that names the reason.
    assert!(
        ack_max < MAX_BUDGET.as_nanos() as i128,
        "a send took {:.3} ms to be acknowledged locally, against an upstream that answers \
         nothing for {UPSTREAM_DELAY:?} — sends are waiting on upstream (p95 {:.3} ms, n={})",
        millis(ack_max),
        millis(ack_p95),
        ack_samples.len()
    );

    mirror.abort();
    server.abort();
    upstream_server.abort();
    for reader in readers {
        reader.abort();
    }
}

/// Send one message from the next participant in rotation and wait until every
/// participant has received it.
///
/// Returns the submit-to-`OK` latency. When `collected` is supplied, each
/// peer's submit-to-delivery latency — the quantity the SLO is asserted on —
/// and the signed OK-to-delivery diagnostic are appended to it; the submitter's
/// own echo is drained but never measured, because the spec's boundary ends at
/// delivery to a *peer*.
async fn exchange(
    participants: &mut [Participant],
    deliveries: &mut mpsc::UnboundedReceiver<Delivery>,
    channel: Uuid,
    sequence: &mut usize,
    mut collected: Option<&mut Collected>,
) -> i128 {
    let index = *sequence % participants.len();
    let body = format!("gate2 message {sequence}");
    *sequence += 1;

    let event = harness::message(&participants[index].keys, channel, &body);
    let submitted_at = Instant::now();
    participants[index]
        .sink
        .send(ClientFrame::Text(
            json!(["EVENT", event]).to_string().into(),
        ))
        .await
        .expect("submit the event to the sidecar");

    let ack = tokio::time::timeout(FRAME_TIMEOUT, participants[index].acks.recv())
        .await
        .unwrap_or_else(|_| {
            panic!("no OK for {body} within {FRAME_TIMEOUT:?}: the send is blocked")
        })
        .expect("the sidecar closed the submitter's session");
    assert_eq!(ack.event_id, event.id.to_hex(), "OK for another event");
    assert!(ack.accepted, "the sidecar refused {body}: {}", ack.message);
    let acknowledged_at = ack.at;

    let mut seen: HashSet<usize> = HashSet::new();
    while seen.len() < participants.len() {
        let delivery = tokio::time::timeout(FRAME_TIMEOUT, deliveries.recv())
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "only {} of {} participants received {body} within {FRAME_TIMEOUT:?}",
                    seen.len(),
                    participants.len()
                )
            })
            .expect("every client reader stopped");
        assert_eq!(
            delivery.event_id,
            event.id.to_hex(),
            "a client received an event other than the one in flight, which means a duplicate \
             or reordered delivery"
        );
        assert!(
            seen.insert(delivery.client),
            "client {} received {body} twice",
            delivery.client
        );
        if delivery.client == index {
            continue;
        }
        if let Some(collected) = collected.as_deref_mut() {
            collected
                .submit_to_delivery
                .push(signed_nanos(submitted_at, delivery.at));
            collected
                .ok_to_delivery
                .push(signed_nanos(acknowledged_at, delivery.at));
        }
    }

    signed_nanos(submitted_at, acknowledged_at)
}

/// `to - from` in nanoseconds, negative when `to` precedes `from`.
fn signed_nanos(from: Instant, to: Instant) -> i128 {
    if to >= from {
        (to - from).as_nanos() as i128
    } else {
        -((from - to).as_nanos() as i128)
    }
}
