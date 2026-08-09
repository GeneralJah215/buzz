//! In-process observer bus for ACP session activity.
//!
//! This is intentionally process-local infrastructure: it lets the harness
//! collect raw ACP JSON-RPC activity and publish owner-scoped encrypted relay
//! frames without exposing a local HTTP port.

use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};

use serde::Serialize;
use tokio::sync::broadcast;

const OBSERVER_BUFFER_CAP: usize = 1_000;

/// Kind carried by discontinuity frames minted by a lagging bus consumer.
///
/// A consumer that detects a `seq` gap it cannot fully refill from the replay
/// ring publishes one of these *in place of* the frames it lost, so downstream
/// never mistakes a hole for a quiet period. Deliberately a first-class kind
/// and not a log line: every consumer of this bus is in another process.
pub const OBSERVER_GAP_KIND: &str = "observer_gap";

/// Frame kinds whose loss corrupts downstream state rather than merely
/// thinning out telemetry.
///
/// These carry *state* — an agent's lifecycle, an RPC's completion, a session's
/// captured config. Losing one leaves a consumer holding a value that is wrong
/// rather than stale, and no later frame corrects it. `observer_gap` is on the
/// list because a gap announcement that is itself lost converts a known hole
/// into an unknown one.
pub const OBSERVER_CONTROL_PLANE_KINDS: [&str; 4] = [
    "managed_agent_runtime_lifecycle",
    "control_result",
    "session_config_captured",
    OBSERVER_GAP_KIND,
];

/// True when losing a frame of this kind corrupts downstream state.
pub fn is_control_plane_kind(kind: &str) -> bool {
    OBSERVER_CONTROL_PLANE_KINDS.contains(&kind)
}

/// Replay capacity reserved exclusively for control-plane frames.
///
/// This is a *separate* ring, not a bigger one, and the distinction is the
/// whole fix. The main replay ring and the broadcast channel are both
/// [`OBSERVER_BUFFER_CAP`] slots, which means the set of frames a lagging
/// consumer misses is exactly the set the main ring has already evicted:
/// `snapshot()` provably cannot refill a broadcast gap, at any shared
/// capacity. Raising [`OBSERVER_BUFFER_CAP`] would not change that — it would
/// only move the number, and make the loss rarer and so harder to diagnose.
///
/// This ring is sized to *control-plane* volume (a handful of frames per
/// session) instead of to content volume, so an arbitrarily long storm of
/// chatty telemetry cannot evict a single `ready`, `failed`, `control_result`,
/// or `session_config_captured` frame. That is what makes a gap reconcilable
/// rather than merely detectable.
const OBSERVER_CONTROL_REPLAY_CAP: usize = 256;

/// Best-effort metadata attached to observer events.
#[derive(Clone, Debug, Default)]
pub struct ObserverContext {
    /// Buzz channel UUID for the current turn, when channel-scoped.
    pub channel_id: Option<String>,
    /// ACP session ID associated with the current turn, once known.
    pub session_id: Option<String>,
    /// Local UUID for one prompt turn.
    pub turn_id: Option<String>,
    /// RFC3339 timestamp at which the current turn began, when known.
    pub started_at: Option<String>,
}

/// Handle used by the harness to publish local observer events.
#[derive(Clone)]
pub struct ObserverHandle {
    inner: Arc<ObserverInner>,
}

/// Non-owning reference to the bus, for a consumer that needs to reach back
/// into the replay rings without keeping the broadcast channel open.
///
/// A consumer holding a strong [`ObserverHandle`] would also hold a
/// `broadcast::Sender`, so its own receiver would never observe `Closed` and
/// the publisher task would never shut down.
#[derive(Clone)]
pub struct ObserverWeak {
    inner: std::sync::Weak<ObserverInner>,
}

impl ObserverWeak {
    /// Reach the bus, if the harness still holds it.
    pub fn upgrade(&self) -> Option<ObserverHandle> {
        self.inner
            .upgrade()
            .map(|inner| ObserverHandle { inner })
    }
}

struct ObserverInner {
    tx: broadcast::Sender<ObserverEvent>,
    buffer: Mutex<VecDeque<ObserverEvent>>,
    control: Mutex<ControlReplayRing>,
    seq: AtomicU64,
}

/// Control-plane replay ring, plus an exact record of what it has evicted.
///
/// The evicted `seq` values are kept so a consumer can be told *how many*
/// control-plane frames after a given point are gone for good, rather than
/// being handed a shrug. An empty `evicted` list is a positive proof that
/// nothing control-plane was lost — which is the answer a consumer needs
/// before it may keep trusting the state it holds.
#[derive(Default)]
struct ControlReplayRing {
    events: VecDeque<ObserverEvent>,
    /// `seq` of every control frame this ring has dropped, oldest first,
    /// itself capped so the bookkeeping cannot outgrow the data.
    evicted: VecDeque<u64>,
    /// Evictions older than the tracked `evicted` window. Their `seq` values
    /// are all strictly below `evicted.front()`.
    evicted_untracked: u64,
}

/// Cap on remembered eviction `seq` values. Larger than the ring itself so a
/// consumer that misses several ring-fulls still gets an exact count.
const OBSERVER_CONTROL_EVICTION_MEMORY: usize = 4_096;

impl ControlReplayRing {
    fn push(&mut self, event: ObserverEvent) {
        if self.events.len() >= OBSERVER_CONTROL_REPLAY_CAP {
            if let Some(dropped) = self.events.pop_front() {
                if self.evicted.len() >= OBSERVER_CONTROL_EVICTION_MEMORY {
                    self.evicted.pop_front();
                    self.evicted_untracked += 1;
                }
                self.evicted.push_back(dropped.seq);
            }
        }
        self.events.push_back(event);
    }

    /// Exact count of control-plane frames with `seq > since` that this ring
    /// evicted and can no longer hand back.
    fn evicted_above(&self, since: u64) -> u64 {
        let tracked = self.evicted.iter().filter(|seq| **seq > since).count() as u64;
        // Untracked evictions are strictly older — and therefore lower `seq` —
        // than every tracked one, so they can only be above `since` when even
        // the oldest tracked eviction is.
        let untracked = match self.evicted.front() {
            Some(oldest) if *oldest > since => self.evicted_untracked,
            None => self.evicted_untracked,
            Some(_) => 0,
        };
        tracked + untracked
    }
}

fn new_observer_handle() -> ObserverHandle {
    let (tx, _) = broadcast::channel(OBSERVER_BUFFER_CAP);
    ObserverHandle {
        inner: Arc::new(ObserverInner {
            tx,
            buffer: Mutex::new(VecDeque::with_capacity(OBSERVER_BUFFER_CAP)),
            control: Mutex::new(ControlReplayRing::default()),
            seq: AtomicU64::new(1),
        }),
    }
}

/// Event delivered through the in-process observer bus.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ObserverEvent {
    /// Monotonic process-local sequence number.
    pub seq: u64,
    /// RFC3339 UTC timestamp.
    pub timestamp: String,
    /// Observer event kind, for example `acp_read` or `turn_started`.
    pub kind: String,
    /// Pool slot index for the agent process that emitted the event.
    pub agent_index: Option<usize>,
    /// Buzz channel UUID for channel-scoped events.
    pub channel_id: Option<String>,
    /// ACP session ID when known.
    pub session_id: Option<String>,
    /// Local UUID for one prompt turn.
    pub turn_id: Option<String>,
    /// RFC3339 timestamp at which the current turn began, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    /// Raw or semantic event payload.
    pub payload: serde_json::Value,
}

/// What the replay rings can still hand back for a consumer's `seq` gap.
pub struct ObserverReplay {
    /// Retained events with `seq` strictly greater than the requested point,
    /// ascending by `seq`, deduped across both rings.
    pub events: Vec<ObserverEvent>,
    /// Exact count of control-plane frames after the requested point that have
    /// been evicted and can never be handed back. **Non-zero means permanent
    /// loss of state** — a consumer must surface it, never absorb it. Zero is a
    /// positive proof, not an absence of evidence: the ring records every
    /// eviction it makes.
    pub lost_control: u64,
}

impl ObserverHandle {
    /// Create an in-process observer feed.
    pub fn in_process() -> Self {
        new_observer_handle()
    }

    /// Non-owning reference to this bus. See [`ObserverWeak`].
    pub fn downgrade(&self) -> ObserverWeak {
        ObserverWeak {
            inner: Arc::downgrade(&self.inner),
        }
    }

    /// Subscribe to live observer events.
    pub fn subscribe(&self) -> broadcast::Receiver<ObserverEvent> {
        self.inner.tx.subscribe()
    }

    /// Return the current replay buffer.
    pub fn snapshot(&self) -> Vec<ObserverEvent> {
        match self.inner.buffer.lock() {
            Ok(buffer) => buffer.iter().cloned().collect(),
            Err(error) => {
                tracing::warn!(target: "observer", "observer replay buffer lock poisoned: {error}");
                Vec::new()
            }
        }
    }

    /// Refill a consumer's `seq` gap from the replay rings.
    ///
    /// Returns every retained frame after `since` — content frames from the
    /// main ring and, crucially, control-plane frames from the dedicated ring
    /// that content churn cannot reach. [`ObserverReplay::lost_control`] is the
    /// exact count of control-plane frames after `since` that neither ring can
    /// still supply. A consumer must be able to tell "I refilled the gap" from
    /// "I have a permanent hole", and the two have to look different
    /// downstream; that field is the difference.
    pub fn replay_since(&self, since: u64) -> ObserverReplay {
        let mut events: Vec<ObserverEvent> = match self.inner.buffer.lock() {
            Ok(buffer) => buffer
                .iter()
                .filter(|event| event.seq > since)
                .cloned()
                .collect(),
            Err(error) => {
                tracing::warn!(target: "observer", "observer replay buffer lock poisoned: {error}");
                Vec::new()
            }
        };
        let lost_control = match self.inner.control.lock() {
            Ok(control) => {
                events.extend(
                    control
                        .events
                        .iter()
                        .filter(|event| event.seq > since)
                        .cloned(),
                );
                control.evicted_above(since)
            }
            Err(error) => {
                tracing::warn!(target: "observer", "observer control replay lock poisoned: {error}");
                // A poisoned ring is not an empty ring. Refusing to answer is
                // the only honest option: report the control plane as lost
                // rather than as intact, which is the one answer that is
                // certainly wrong.
                u64::MAX
            }
        };
        // A control frame lives in both rings until content churn evicts it
        // from the main one, so the union needs a dedupe. `seq` is assigned
        // under `fetch_add` and is unique per frame.
        events.sort_by_key(|event| event.seq);
        events.dedup_by_key(|event| event.seq);
        ObserverReplay {
            events,
            lost_control,
        }
    }

    /// Mint a discontinuity frame describing frames a consumer lost.
    ///
    /// Takes a `seq` from the same monotonic counter as every other frame so
    /// it sorts and dedupes downstream like one, and records it in the replay
    /// ring so a later reconnect still sees the hole. It is deliberately *not*
    /// broadcast: the consumer that minted it publishes it directly, ahead of
    /// the frames it recovered. Round-tripping it through the channel would
    /// deliver the announcement after the recovery it announces — and through
    /// the very channel that is currently dropping frames.
    pub fn mint_gap_event(&self, payload: serde_json::Value) -> ObserverEvent {
        let event = ObserverEvent {
            seq: self.inner.seq.fetch_add(1, Ordering::Relaxed),
            timestamp: chrono::Utc::now().to_rfc3339(),
            kind: OBSERVER_GAP_KIND.to_string(),
            agent_index: None,
            channel_id: None,
            session_id: None,
            turn_id: None,
            started_at: None,
            payload,
        };
        self.record(&event);
        event
    }

    /// Record a frame in the replay rings.
    ///
    /// Control-plane frames land in both: the main ring for ordering, and the
    /// dedicated ring so that content churn — which is what evicts the main
    /// ring, thousands of frames at a time — cannot take state frames with it.
    fn record(&self, event: &ObserverEvent) {
        match self.inner.buffer.lock() {
            Ok(mut buffer) => {
                if buffer.len() >= OBSERVER_BUFFER_CAP {
                    buffer.pop_front();
                }
                buffer.push_back(event.clone());
            }
            Err(error) => {
                tracing::warn!(target: "observer", "observer replay buffer lock poisoned: {error}");
            }
        }
        if !is_control_plane_kind(&event.kind) {
            return;
        }
        match self.inner.control.lock() {
            Ok(mut control) => control.push(event.clone()),
            Err(error) => {
                tracing::warn!(target: "observer", "observer control replay lock poisoned: {error}");
            }
        }
    }

    /// Emit a local observer event.
    pub fn emit(
        &self,
        kind: impl Into<String>,
        agent_index: Option<usize>,
        context: &ObserverContext,
        payload: serde_json::Value,
    ) {
        let event = ObserverEvent {
            seq: self.inner.seq.fetch_add(1, Ordering::Relaxed),
            timestamp: chrono::Utc::now().to_rfc3339(),
            kind: kind.into(),
            agent_index,
            channel_id: context.channel_id.clone(),
            session_id: context.session_id.clone(),
            turn_id: context.turn_id.clone(),
            started_at: context.started_at.clone(),
            payload,
        };

        self.record(&event);

        let _ = self.inner.tx.send(event);
    }
}

/// Build observer context values from optional channel/session/turn IDs.
pub fn context_for(
    channel_id: Option<uuid::Uuid>,
    session_id: Option<String>,
    turn_id: Option<String>,
) -> ObserverContext {
    ObserverContext {
        channel_id: channel_id.map(|id| id.to_string()),
        session_id,
        turn_id,
        started_at: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn emit_content(observer: &ObserverHandle, n: usize) {
        for _ in 0..n {
            observer.emit(
                "acp_read",
                None,
                &ObserverContext::default(),
                serde_json::json!({}),
            );
        }
    }

    fn emit_lifecycle(observer: &ObserverHandle, marker: &str) -> u64 {
        observer.emit(
            "managed_agent_runtime_lifecycle",
            None,
            &ObserverContext::default(),
            serde_json::json!({ "marker": marker }),
        );
        observer
            .snapshot()
            .last()
            .map(|event| event.seq)
            .expect("just-emitted frame is in the replay ring")
    }

    #[test]
    fn a_content_storm_cannot_evict_a_lifecycle_frame() {
        // This is the whole reason the control ring exists. The main ring and
        // the broadcast channel are both OBSERVER_BUFFER_CAP, so a consumer's
        // gap is exactly what the main ring has already thrown away — replay
        // from the main ring alone can never refill one.
        let observer = ObserverHandle::in_process();
        let ready = emit_lifecycle(&observer, "ready");
        emit_content(&observer, OBSERVER_BUFFER_CAP * 3);

        let main_ring_has_it = observer.snapshot().iter().any(|event| event.seq == ready);
        assert!(
            !main_ring_has_it,
            "precondition: content churn must have evicted the frame from the main ring"
        );

        let replay = observer.replay_since(0);
        assert!(
            replay.events.iter().any(|event| event.seq == ready),
            "the lifecycle frame must survive in the control ring"
        );
        assert_eq!(
            replay.lost_control, 0,
            "no control frame was evicted, so the gap is provably reconcilable"
        );
    }

    #[test]
    fn evicted_control_frames_are_counted_exactly_not_shrugged_at() {
        let observer = ObserverHandle::in_process();
        // Overfill the control ring by 5.
        let overflow = 5;
        let mut seqs = Vec::new();
        for index in 0..OBSERVER_CONTROL_REPLAY_CAP + overflow {
            seqs.push(emit_lifecycle(&observer, &format!("f{index}")));
        }

        let replay = observer.replay_since(0);
        assert_eq!(replay.lost_control, overflow as u64);
        // The evicted ones are the oldest; asking from just past them reports
        // a clean gap again.
        let last_evicted = seqs[overflow - 1];
        assert_eq!(observer.replay_since(last_evicted).lost_control, 0);
    }

    #[test]
    fn replay_since_excludes_frames_the_consumer_already_has() {
        let observer = ObserverHandle::in_process();
        let first = emit_lifecycle(&observer, "one");
        let second = emit_lifecycle(&observer, "two");

        let replay = observer.replay_since(first);
        assert_eq!(
            replay.events.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![second]
        );
    }

    #[test]
    fn replay_union_is_deduped_and_ascending() {
        // A control frame sits in both rings until content churn evicts it from
        // the main one. The union must not hand it back twice.
        let observer = ObserverHandle::in_process();
        emit_content(&observer, 2);
        emit_lifecycle(&observer, "ready");
        emit_content(&observer, 2);

        let replay = observer.replay_since(0);
        let seqs: Vec<u64> = replay.events.iter().map(|event| event.seq).collect();
        let mut expected = seqs.clone();
        expected.sort_unstable();
        expected.dedup();
        assert_eq!(seqs, expected, "replay must be ascending and deduped");
        assert_eq!(seqs.len(), 5);
    }

    #[test]
    fn a_minted_gap_frame_is_replayable_but_never_broadcast() {
        // Round-tripping the announcement through the channel that is dropping
        // frames would deliver it after the recovery it announces, and could
        // drop it outright.
        let observer = ObserverHandle::in_process();
        let mut rx = observer.subscribe();
        let gap = observer.mint_gap_event(serde_json::json!({ "complete": false }));

        assert_eq!(gap.kind, OBSERVER_GAP_KIND);
        assert!(
            rx.try_recv().is_err(),
            "the gap frame must not be broadcast"
        );
        assert!(observer
            .replay_since(0)
            .events
            .iter()
            .any(|event| event.seq == gap.seq));
    }

    #[test]
    fn control_plane_kinds_are_exactly_the_state_carrying_ones() {
        for kind in OBSERVER_CONTROL_PLANE_KINDS {
            assert!(is_control_plane_kind(kind), "{kind} must be control plane");
        }
        for kind in ["acp_read", "acp_write", "turn_started", "raw_json_rpc"] {
            assert!(
                !is_control_plane_kind(kind),
                "{kind} is telemetry, not control plane"
            );
        }
    }
}

/// Attach the authoritative start timestamp to every observer frame for a turn.
pub fn context_for_turn(
    channel_id: Option<uuid::Uuid>,
    session_id: Option<String>,
    turn_id: String,
    started_at: String,
) -> ObserverContext {
    ObserverContext {
        channel_id: channel_id.map(|id| id.to_string()),
        session_id,
        turn_id: Some(turn_id),
        started_at: Some(started_at),
    }
}
