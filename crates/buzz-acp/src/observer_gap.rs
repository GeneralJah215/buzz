//! Gap detection and recovery for the single consumer of the observer bus.
//!
//! The bus is a 1000-slot `tokio::sync::broadcast` feeding a consumer that is
//! rate-limited to 90 frames a minute. Loss under sustained load is guaranteed
//! by construction, not merely possible — `dropped=3318` has been measured on a
//! busy agent. Everything downstream (managed-agent lifecycle, the transcript
//! accumulator, `control_result` RPC completion, persisted session config)
//! reads those frames as if they were reliable.
//!
//! Enlarging the buffer is not a fix: 1000 slots against an unbounded producer
//! is the same bet at a different number, and a rarer loss is a harder one to
//! diagnose. What this module does instead is make the loss *visible* and, for
//! the frames that carry state rather than telemetry, *recoverable*:
//!
//! 1. `seq` is monotonic, so the consumer can bound the hole exactly — from the
//!    last frame it processed to the first frame delivered after the lag.
//! 2. [`crate::observer::ObserverHandle::replay_since`] hands the hole's
//!    control-plane frames back out of a ring that content churn cannot reach,
//!    so they can be republished. (The *main* replay ring is the same size as
//!    the broadcast channel, so it evicts exactly what the consumer missed and
//!    provably cannot refill a gap on its own — see
//!    `OBSERVER_CONTROL_REPLAY_CAP`.)
//! 3. Whatever the ring cannot hand back is **named and published**, as an
//!    `observer_gap` frame, not absorbed. A consumer that cannot reconcile must
//!    say so; it must never report a hole as a quiet period.

use crate::observer::{is_control_plane_kind, ObserverEvent, ObserverReplay};

/// Ceiling on how many frames a single gap may replay.
///
/// Recovery publishes unpaced (see the call site), so this bound is what keeps
/// a recovery burst from becoming the cause of the next lag. Control-plane
/// frames are rare enough that reaching this ceiling means something is very
/// wrong — and reaching it is reported as unrecoverable loss rather than
/// silently truncated.
pub(crate) const MAX_GAP_REPLAY_FRAMES: usize = 64;

/// A planned reconciliation for one detected `seq` discontinuity.
pub(crate) struct GapRecovery {
    /// Control-plane frames pulled back out of the replay ring, ascending by
    /// `seq`. Republished in place of the live frames that were dropped.
    pub replay: Vec<ObserverEvent>,
    /// How many frames [`Self::replay`] held at plan time. Kept separately so
    /// draining the vec to publish it cannot quietly shrink the denominator the
    /// receipt is judged against.
    pub planned: usize,
    /// Last `seq` the consumer definitely processed. The hole starts after it.
    pub from_seq: u64,
    /// First `seq` delivered after the hole, when one has arrived yet.
    pub to_seq: Option<u64>,
    /// Frames the broadcast channel reported as skipped.
    pub dropped: u64,
    /// Frames inside the hole that the ring could not hand back at all, plus
    /// any control-plane frames trimmed by [`MAX_GAP_REPLAY_FRAMES`]. Their
    /// kinds are unknown, so **any non-zero value means downstream control
    /// state may be wrong and nothing here can correct it.**
    pub unrecoverable: u64,
    /// Telemetry frames inside the hole that the ring held but that are not
    /// replayed by design. Not recoverable state — but the transcript still
    /// needs to know its text has a hole in it rather than splicing across one.
    pub lost_content: u64,
}

impl GapRecovery {
    /// True only when every frame in the hole was either known telemetry or a
    /// control-plane frame that was replayed **and actually published**.
    ///
    /// `delivered` is a receipt, not a plan: a replay frame the relay refused
    /// is exactly as lost as one the ring had evicted, and calling it complete
    /// would be the fifth instance of a surface reporting good news it had not
    /// earned. False is the loud state.
    pub fn is_complete(&self, delivered: usize) -> bool {
        self.unrecoverable == 0 && delivered == self.planned
    }

    /// The `observer_gap` frame payload announcing this discontinuity.
    ///
    /// Downstream reads `controlComplete` to decide whether it may keep
    /// trusting the state it holds. It is named for the control plane
    /// specifically — a gap can be control-complete and still have destroyed
    /// hundreds of content frames, which `lostContent` reports separately — and
    /// it is derived from the delivered count, never passed in, so a caller
    /// cannot report a reconciled gap it did not reconcile.
    pub fn receipt(&self, delivered: usize) -> serde_json::Value {
        serde_json::json!({
            "fromSeq": self.from_seq,
            "toSeq": self.to_seq,
            "dropped": self.dropped,
            "planned": self.planned,
            "recovered": delivered,
            "unrecoverable": self.unrecoverable,
            "lostContent": self.lost_content,
            "controlComplete": self.is_complete(delivered),
        })
    }
}

/// Plan what to republish for a hole between `last_seq` and `next_seq`.
///
/// `next_seq` is the `seq` of the first frame delivered after the lag — the
/// exclusive upper bound of the hole. It is `None` when the stream went quiet
/// or closed before another frame arrived; the gap is then still announced
/// (with an open upper bound) rather than held back, because an agent that
/// falls silent immediately after a lag storm is exactly when a stuck
/// `waking` badge would otherwise persist unchallenged.
pub(crate) fn plan_gap_recovery(
    last_seq: u64,
    next_seq: Option<u64>,
    dropped: u64,
    replay: ObserverReplay,
) -> GapRecovery {
    let in_hole = |event: &ObserverEvent| {
        event.seq > last_seq && next_seq.is_none_or(|next| event.seq < next)
    };

    let mut control: Vec<ObserverEvent> = Vec::new();
    let mut retained_content = 0u64;
    for event in replay.events.into_iter().filter(in_hole) {
        if is_control_plane_kind(&event.kind) {
            control.push(event);
        } else {
            retained_content += 1;
        }
    }
    control.sort_by_key(|event| event.seq);

    // Trim to the newest frames: for lifecycle and session config the newest
    // frame *is* the current state, so keeping the tail converges correctly.
    // For `control_result` the trimmed ones are RPC completions nobody will
    // ever receive, which is why they are counted as unrecoverable rather than
    // quietly discarded.
    let trimmed = control.len().saturating_sub(MAX_GAP_REPLAY_FRAMES) as u64;
    if trimmed > 0 {
        control.drain(..trimmed as usize);
    }
    let planned = control.len();
    let unrecoverable = replay.lost_control.saturating_add(trimmed);

    // With both ends of the hole known, every `seq` in it is accounted for:
    // replayed, unrecoverable control, or content. Derive content loss from
    // that identity rather than from what the ring happened to retain, which
    // would undercount the frames the ring had already thrown away.
    let lost_content = match next_seq {
        Some(next) => next
            .saturating_sub(last_seq)
            .saturating_sub(1)
            .saturating_sub(planned as u64)
            .saturating_sub(unrecoverable),
        None => retained_content,
    };

    GapRecovery {
        planned,
        replay: control,
        from_seq: last_seq,
        to_seq: next_seq,
        dropped,
        unrecoverable,
        lost_content,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observer::OBSERVER_GAP_KIND;

    fn event(seq: u64, kind: &str) -> ObserverEvent {
        ObserverEvent {
            seq,
            timestamp: "2026-08-09T00:00:00Z".into(),
            kind: kind.into(),
            agent_index: None,
            channel_id: None,
            session_id: None,
            turn_id: None,
            started_at: None,
            payload: serde_json::json!({}),
        }
    }

    fn replay(events: Vec<ObserverEvent>, lost_control: u64) -> ObserverReplay {
        ObserverReplay {
            events,
            lost_control,
        }
    }

    #[test]
    fn lifecycle_frames_inside_the_hole_are_replayed() {
        // The whole point of consumer 1: a dropped `ready` must come back, or
        // the desktop shows an agent as starting forever while it is running.
        let recovery = plan_gap_recovery(
            10,
            Some(20),
            9,
            replay(
                vec![
                    event(12, "managed_agent_runtime_lifecycle"),
                    event(15, "acp_read"),
                    event(18, "managed_agent_runtime_lifecycle"),
                ],
                0,
            ),
        );
        assert_eq!(
            recovery
                .replay
                .iter()
                .map(|event| event.seq)
                .collect::<Vec<_>>(),
            vec![12, 18]
        );
        assert!(recovery.is_complete(recovery.planned));
        assert_eq!(recovery.unrecoverable, 0);
        // Nine seqs in the hole, two of them replayed control frames: the other
        // seven were content and are gone. Counting only the content frames the
        // ring happened to still hold would have said "1" and understated the
        // hole the transcript has to mark.
        assert_eq!(recovery.lost_content, 7);
    }

    #[test]
    fn every_control_plane_kind_is_replayed_and_content_is_not() {
        let events = vec![
            event(2, "managed_agent_runtime_lifecycle"),
            event(3, "control_result"),
            event(4, "session_config_captured"),
            event(5, OBSERVER_GAP_KIND),
            event(6, "acp_write"),
            event(7, "turn_started"),
        ];
        let recovery = plan_gap_recovery(1, Some(8), 6, replay(events, 0));
        assert_eq!(recovery.replay.len(), 4);
        assert_eq!(recovery.lost_content, 2);
    }

    #[test]
    fn frames_outside_the_hole_are_never_republished() {
        // Everything at or below `last_seq` was already delivered, and
        // everything at or above `next_seq` is still coming down the live
        // stream. Replaying either would duplicate a frame, which for
        // `control_result` means completing an RPC twice.
        let events = vec![
            event(5, "control_result"),
            event(9, "control_result"),
            event(12, "control_result"),
            event(20, "control_result"),
            event(25, "control_result"),
        ];
        let recovery = plan_gap_recovery(10, Some(20), 9, replay(events, 0));
        assert_eq!(
            recovery
                .replay
                .iter()
                .map(|event| event.seq)
                .collect::<Vec<_>>(),
            vec![12]
        );
    }

    #[test]
    fn evicted_frames_make_the_gap_incomplete_and_are_counted() {
        // The ring wrapped past the hole. Nothing here can say what those
        // frames were, so the gap must not claim to be reconciled.
        let recovery = plan_gap_recovery(
            10,
            Some(4000),
            3318,
            replay(vec![event(3100, "managed_agent_runtime_lifecycle")], 3089),
        );
        assert!(!recovery.is_complete(recovery.planned));
        assert_eq!(recovery.unrecoverable, 3089);
        assert_eq!(recovery.replay.len(), 1);
        let receipt = recovery.receipt(recovery.planned);
        assert_eq!(receipt["controlComplete"], serde_json::json!(false));
        assert_eq!(receipt["unrecoverable"], serde_json::json!(3089));
    }

    #[test]
    fn replay_is_bounded_and_the_overflow_counts_as_unrecoverable() {
        let events: Vec<_> = (2..=200)
            .map(|seq| event(seq, "control_result"))
            .collect();
        let recovery = plan_gap_recovery(1, Some(1000), 198, replay(events, 0));
        assert_eq!(recovery.replay.len(), MAX_GAP_REPLAY_FRAMES);
        // Newest kept: for lifecycle/config the newest frame is the current
        // state, so the tail is the half that converges.
        assert_eq!(recovery.replay.last().unwrap().seq, 200);
        assert_eq!(recovery.unrecoverable, 199 - MAX_GAP_REPLAY_FRAMES as u64);
        assert!(!recovery.is_complete(recovery.planned));
    }

    #[test]
    fn an_open_ended_hole_is_still_announced() {
        // Agent went silent right after the lag storm. Holding the
        // announcement until the next frame would leave a stuck badge
        // unchallenged for as long as the agent stays quiet.
        let recovery = plan_gap_recovery(
            10,
            None,
            5,
            replay(vec![event(14, "managed_agent_runtime_lifecycle")], 3),
        );
        assert_eq!(recovery.replay.len(), 1);
        assert_eq!(recovery.to_seq, None);
        assert_eq!(
            recovery.receipt(recovery.planned)["toSeq"],
            serde_json::Value::Null
        );
        assert!(!recovery.is_complete(recovery.planned));
    }

    #[test]
    fn receipt_reports_completeness_it_actually_achieved() {
        // Guards against the failure mode this project has recorded five times:
        // a surface reporting good news it had not earned. `complete` is
        // derived, so no caller can set it independently.
        let clean = plan_gap_recovery(1, Some(3), 1, replay(vec![event(2, "control_result")], 0));
        assert_eq!(clean.receipt(1)["controlComplete"], serde_json::json!(true));
        assert_eq!(clean.receipt(1)["recovered"], serde_json::json!(1));

        let dirty = plan_gap_recovery(1, Some(3), 1, replay(Vec::new(), 1));
        assert_eq!(dirty.receipt(0)["controlComplete"], serde_json::json!(false));
        assert_eq!(dirty.receipt(0)["recovered"], serde_json::json!(0));
    }

    #[test]
    fn a_replay_frame_that_failed_to_publish_is_not_reported_as_recovered() {
        // A frame the relay refused is exactly as lost as one the ring evicted.
        // Planning two and delivering one must not read as a reconciled gap.
        let recovery = plan_gap_recovery(
            1,
            Some(9),
            7,
            replay(
                vec![
                    event(3, "managed_agent_runtime_lifecycle"),
                    event(6, "control_result"),
                ],
                0,
            ),
        );
        assert_eq!(recovery.planned, 2);
        assert!(recovery.is_complete(2));
        assert!(!recovery.is_complete(1));
        let receipt = recovery.receipt(1);
        assert_eq!(receipt["controlComplete"], serde_json::json!(false));
        assert_eq!(receipt["planned"], serde_json::json!(2));
        assert_eq!(receipt["recovered"], serde_json::json!(1));
    }
}
