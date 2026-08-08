//! Release gate 2 — slow-upstream transport SLO (spec "Test list", gate 2).
//!
//! The operator's actual failure mode is not a severed relay; it is a relay
//! that answers eventually. This gate stands up a **reachable** canonical relay
//! that completes its TCP connection and its WebSocket upgrade and then takes
//! [`UPSTREAM_DELAY`] to answer anything at all, runs the real upstream mirror
//! against it, and measures local delivery while the mirror sits blocked.
//!
//! Measurement boundary, exactly as the spec fixes it: **from the sidecar
//! returning `OK` for the submitted event, to each subscribed peer receiving
//! the event frame.** One test-harness process hosts all three client
//! connections and every timestamp comes from [`Instant`] — one host, one clock
//! source, no cross-machine skew.
//!
//! The `OK` frame and the peers' `EVENT` frames travel on different sockets,
//! and the sidecar fans out *before* it enqueues the `OK`, so a peer can
//! legitimately be served before the submitter is acknowledged. Latencies are
//! therefore kept **signed** rather than clamped at zero: a negative sample is
//! real information about the pipeline, and hiding it behind a `saturating_sub`
//! would quietly discard the evidence that fan-out precedes acknowledgment.
//!
//! What makes this gate more than a stopwatch is what it asserts about the
//! upstream *while* it measures. If the mirror had never connected, or if the
//! slow relay had answered, or if the events had drained upstream, the numbers
//! below would be about a healthy system and would prove nothing. So the gate
//! also requires: the mirror really connected (upstream reachable, not cut),
//! upstream sent zero responses across the whole window, upstream received zero
//! events, and every accepted event is still sitting pending in the outbox.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
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
use nostr::{Event, Keys};
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

/// Artificial upstream delay applied to every canonical-relay response.
///
/// The spec asks for at least ten seconds. Sixty is used so the delay is still
/// outstanding, unfulfilled, when the gate finishes asserting — the whole
/// measurement window then sits strictly inside one un-answered upstream
/// operation, which is a stronger statement than "ten seconds of slowness
/// happened somewhere during the run". The margin also keeps the gate honest
/// on a slow machine: a run that took four times longer than expected still
/// reports on the SLO rather than on an upstream that finally answered.
const UPSTREAM_DELAY: Duration = Duration::from_secs(60);

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
    /// Responses upstream managed to complete, over WebSocket or HTTP.
    responses_sent: Arc<AtomicU64>,
    /// HTTP bridge backfill requests.
    query_requests: Arc<AtomicU64>,
    /// NIP-11 relay-identity document requests.
    identity_requests: Arc<AtomicU64>,
}

impl SlowUpstream {
    fn new(identity: &nostr::PublicKey) -> Self {
        Self {
            identity: identity.to_hex(),
            connections: Arc::default(),
            frames_received: Arc::default(),
            events_received: Arc::default(),
            responses_sent: Arc::default(),
            query_requests: Arc::default(),
            identity_requests: Arc::default(),
        }
    }
}

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
            state.responses_sent.fetch_add(1, Ordering::SeqCst);
            axum::response::IntoResponse::into_response(Json(
                json!({"self": state.identity.clone()}),
            ))
        }
    }
}

/// Reachable, authenticated-eventually, and answering nothing for a long time.
///
/// The upgrade completes immediately — this is a slow relay, not a cut one —
/// and every response, starting with the NIP-42 challenge, waits
/// [`UPSTREAM_DELAY`] first.
async fn slow_session(state: SlowUpstream, mut socket: WebSocket) {
    state.connections.fetch_add(1, Ordering::SeqCst);
    tokio::time::sleep(UPSTREAM_DELAY).await;
    if socket
        .send(UpstreamFrame::Text(
            json!(["AUTH", "slow-upstream-challenge"])
                .to_string()
                .into(),
        ))
        .await
        .is_err()
    {
        return;
    }
    state.responses_sent.fetch_add(1, Ordering::SeqCst);

    while let Some(Ok(frame)) = socket.next().await {
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
        if verb == "EVENT" {
            state.events_received.fetch_add(1, Ordering::SeqCst);
        }
        tokio::time::sleep(UPSTREAM_DELAY).await;
        let reply = match verb.as_str() {
            "AUTH" | "EVENT" => {
                json!(["OK", parsed[1]["id"].as_str().unwrap_or_default(), true, ""])
            }
            "REQ" => json!(["EOSE", parsed[1].as_str().unwrap_or_default()]),
            _ => continue,
        };
        if socket
            .send(UpstreamFrame::Text(reply.to_string().into()))
            .await
            .is_err()
        {
            return;
        }
        state.responses_sent.fetch_add(1, Ordering::SeqCst);
    }
}

async fn slow_query(
    State(state): State<SlowUpstream>,
    _body: axum::body::Bytes,
) -> Json<Vec<Event>> {
    state.query_requests.fetch_add(1, Ordering::SeqCst);
    tokio::time::sleep(UPSTREAM_DELAY).await;
    state.responses_sent.fetch_add(1, Ordering::SeqCst);
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

// ── The gate ────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gate2_slow_upstream_transport_slo() {
    // The canonical relay has to exist before the community binding does: the
    // binding pins the origin the mirror will dial, and the port is only known
    // once the listener is bound.
    let relay_identity = Keys::generate();
    let upstream = SlowUpstream::new(&relay_identity.public_key());
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
    let channel = Uuid::new_v4();
    let mut members: Vec<_> = identities.iter().map(Keys::public_key).collect();
    members.push(edge_keys.public_key());
    let now = harness::unix_now();
    harness::authorize_as(
        &store,
        &edge_keys,
        &relay_identity,
        now,
        &[(channel, members)],
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

    // Precondition, asserted rather than assumed: the mirror actually reached
    // upstream. A gate that measured a sidecar with no upstream at all would be
    // measuring the wrong system and would still be green.
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

    let anomalies = Arc::new(Mutex::new(Vec::new()));
    let (delivery_tx, mut deliveries) = mpsc::unbounded_channel::<Delivery>();
    let mut participants = Vec::new();
    let mut readers = Vec::new();
    for (index, keys) in identities.iter().enumerate() {
        let mut socket = harness::authenticated_client(&url, &binding, keys).await;
        let history = harness::subscribe(&mut socket, &format!("gate2-{index}"), channel).await;
        assert!(
            history.is_empty(),
            "the store is fresh; a non-empty backfill means upstream history leaked in"
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
    let mut ack_samples: Vec<i128> = Vec::new();
    let mut measured_messages = 0usize;
    let measurement_start = Instant::now();
    while measured_messages < MIN_MEASURED_MESSAGES
        || measurement_start.elapsed() < MIN_MEASUREMENT_WINDOW
    {
        let mut collected = Vec::new();
        let ack_latency = exchange(
            &mut participants,
            &mut deliveries,
            channel,
            &mut sequence,
            Some(&mut collected),
        )
        .await;
        assert_eq!(
            collected.len(),
            peers,
            "expected one delivery per peer for each message"
        );
        samples.extend(collected);
        ack_samples.push(ack_latency);
        measured_messages += 1;
        tokio::time::sleep(SEND_INTERVAL).await;
    }
    let window = measurement_start.elapsed();

    // Snapshot upstream the instant the window closes. Reporting and asserting
    // on counters read later would describe the state at assertion time, not
    // the state the measurements were taken under.
    let upstream_connections = upstream.connections.load(Ordering::SeqCst);
    let upstream_frames = upstream.frames_received.load(Ordering::SeqCst);
    let upstream_events = upstream.events_received.load(Ordering::SeqCst);
    let upstream_responses = upstream.responses_sent.load(Ordering::SeqCst);
    let upstream_queries = upstream.query_requests.load(Ordering::SeqCst);
    let upstream_identity = upstream.identity_requests.load(Ordering::SeqCst);
    let pending_rows = store.pending_count().expect("pending count");
    let stored_events = store.event_count().expect("event count");

    // ── The report (counts and measurements, not a verdict) ────────────────
    let mut sorted = samples.clone();
    sorted.sort_unstable();
    let p50 = percentile(&sorted, 50);
    let p95 = percentile(&sorted, 95);
    let max = *sorted.last().expect("at least one sample");
    let min = sorted[0];
    let mut sorted_acks = ack_samples.clone();
    sorted_acks.sort_unstable();
    let ack_max = *sorted_acks.last().expect("at least one acknowledgment");
    let ack_p95 = percentile(&sorted_acks, 95);

    println!("[gate2] injected upstream delay: {UPSTREAM_DELAY:?} per canonical-relay response");
    println!("[gate2] upstream sessions established: {upstream_connections}");
    println!("[gate2] upstream frames received from the sidecar: {upstream_frames}");
    println!("[gate2] upstream EVENT frames received: {upstream_events}");
    println!("[gate2] upstream HTTP bridge requests: {upstream_queries}");
    println!("[gate2] upstream identity-document requests: {upstream_identity}");
    println!("[gate2] upstream responses completed: {upstream_responses}");
    println!("[gate2] warm-up messages (discarded): {WARMUP_MESSAGES}");
    println!("[gate2] measured messages: {measured_messages}");
    println!("[gate2] measured window: {:.3} s", window.as_secs_f64());
    println!("[gate2] client connections: {}", participants.len());
    println!(
        "[gate2] ingress-to-peer-delivery samples: {}",
        samples.len()
    );
    println!(
        "[gate2] ingress-to-peer-delivery min: {:.3} ms",
        millis(min)
    );
    println!(
        "[gate2] ingress-to-peer-delivery p50: {:.3} ms",
        millis(p50)
    );
    println!(
        "[gate2] ingress-to-peer-delivery p95: {:.3} ms",
        millis(p95)
    );
    println!(
        "[gate2] ingress-to-peer-delivery max: {:.3} ms",
        millis(max)
    );
    println!("[gate2] submit-to-OK p95: {:.3} ms", millis(ack_p95));
    println!("[gate2] submit-to-OK max: {:.3} ms", millis(ack_max));
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

    // The upstream state the SLO numbers were collected under. Without these
    // four, the numbers above describe an unknown system.
    assert!(
        upstream_connections >= 1,
        "upstream must be reachable, not cut"
    );
    assert_eq!(
        upstream_responses, 0,
        "the canonical relay completed a response inside the measurement window, so the run \
         did not measure a fully stalled upstream"
    );
    assert_eq!(
        upstream_events, 0,
        "the sidecar submitted events upstream during the window, so 'still pending' below \
         would not mean what it says"
    );
    let total_messages = (WARMUP_MESSAGES + measured_messages) as u64;
    assert_eq!(
        stored_events, total_messages,
        "every submitted message must be persisted exactly once"
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

    // The spec's SLO.
    assert!(
        p95 <= P95_BUDGET.as_nanos() as i128,
        "sidecar-ingress-to-peer-delivery p95 was {:.3} ms, over the {:.0} ms budget \
         (p50 {:.3} ms, max {:.3} ms, n={})",
        millis(p95),
        P95_BUDGET.as_millis(),
        millis(p50),
        millis(max),
        samples.len()
    );
    assert!(
        max <= MAX_BUDGET.as_nanos() as i128,
        "sidecar-ingress-to-peer-delivery max was {:.3} ms, over the {:.0} ms budget \
         (p50 {:.3} ms, p95 {:.3} ms, n={})",
        millis(max),
        MAX_BUDGET.as_millis(),
        millis(p50),
        millis(p95),
        samples.len()
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
/// peer's signed ingress-to-delivery latency is appended to it; the submitter's
/// own echo is drained but never measured, because the spec's boundary is
/// delivery to a *peer*.
async fn exchange(
    participants: &mut [Participant],
    deliveries: &mut mpsc::UnboundedReceiver<Delivery>,
    channel: Uuid,
    sequence: &mut usize,
    mut collected: Option<&mut Vec<i128>>,
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
    let ingress_at = ack.at;

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
            collected.push(signed_nanos(ingress_at, delivery.at));
        }
    }

    signed_nanos(submitted_at, ingress_at)
}

/// `to - from` in nanoseconds, negative when `to` precedes `from`.
fn signed_nanos(from: Instant, to: Instant) -> i128 {
    if to >= from {
        (to - from).as_nanos() as i128
    } else {
        -((from - to).as_nanos() as i128)
    }
}
