//! Per-slot pool provisioning: grow on demand, reap when idle (BUG-064).
//!
//! # Why this exists
//!
//! [`AgentPool`](crate::pool::AgentPool) used to be provisioned exactly once,
//! eagerly, at `config.agents` width — ten full agent runtimes per harness,
//! spawned before a single message had arrived, and never shrunk. Measured on
//! one operator machine: 14 woken agents × 10 chains × ~5 processes each ≈ 700
//! processes and ~15 GB, of which 132 of 146 runtimes had burned under 20 CPU
//! seconds in eight hours. The cost was never CPU — it was memory and handles
//! held open by runtimes nobody was talking to.
//!
//! [`crate::pool_lifecycle`] already proved the lazy-wake pattern at *whole
//! pool* granularity. This module is the same idea at *slot* granularity, and
//! it is deliberately pure: it decides which slot to grow and which to reap
//! from a description of the pool, never from the pool itself. That is what
//! makes "a reaper must never shut down a checked-out slot" a property a unit
//! test can pin down without spawning a single subprocess.
//!
//! # The two decisions
//!
//! - **Grow** ([`pick_growth_slot`]): a `try_claim` that found nothing free is
//!   the demand signal. Promote one [`SlotIntent::Cold`] slot to
//!   [`SlotIntent::Live`]; the existing slot-refill machinery in the
//!   maintenance tick spawns it, circuit breaker and all.
//! - **Reap** ([`pick_reap_slot`]): a slot that has sat idle past
//!   [`IDLE_SLOT_TTL`] is shut down and returned to `Cold`, provided more than
//!   [`MIN_WARM_SLOTS`] slots are idle. This is the piece that makes the steady
//!   state *bounded* rather than one-way — without it, growth is a ratchet and
//!   the pool arrives back at `config.agents` and stays there forever.
//!
//! `Cold` is not the same as "empty". An empty `Live` slot is a crashed agent
//! and must be refilled; an empty `Cold` slot is deliberate and must not be.
//! Conflating the two is how a reaper turns into a respawn loop.

use std::time::{Duration, Instant};

/// How long a pool slot may sit idle before the reaper shuts it down.
///
/// Ten minutes is well past any conversational gap (a user who is mid-thread
/// re-prompts in seconds) and well short of the multi-hour idleness that was
/// actually holding the memory. A reaped slot costs one agent spawn to get
/// back, which the growth path pays lazily on the next starved claim.
pub(crate) const IDLE_SLOT_TTL: Duration = Duration::from_secs(600);

/// How many idle slots the reaper always leaves alone.
///
/// One. The next turn must never have to wait for a process spawn, so the pool
/// never shrinks to zero warm agents — it shrinks to exactly one, which is
/// also what a fresh startup now provisions.
pub(crate) const MIN_WARM_SLOTS: usize = 1;

/// Whether a slot is *meant* to have a live agent process behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SlotIntent {
    /// Deliberately not running: never started, or reaped for idleness. The
    /// refill path must leave it alone — an empty `Cold` slot is not a crash.
    Cold,
    /// Should have a process. An empty `Live` slot *is* a crash, and the
    /// maintenance tick's refill loop respawns it.
    Live,
}

/// What is physically in a slot right now.
///
/// Deliberately one field rather than a pair of `idle` / `checked_out` booleans.
/// With two booleans the reaper's "never touch a checked-out slot" guard can be
/// written twice over, and then deleting either copy still leaves the other one
/// enforcing it — a defect that no mutation can expose. One enum, one arm, one
/// guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SlotOccupancy {
    /// No agent: cold, crashed, or a spawn still in flight.
    Empty,
    /// Agent is sitting in its slot, and has been since this instant.
    Idle(Instant),
    /// Agent is checked out for an in-flight turn. **Never reapable** — the
    /// pool does not hold it, and a turn is running against it.
    CheckedOut,
}

/// Read-only description of one slot, built by `AgentPool` for the deciders.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SlotView {
    pub intent: SlotIntent,
    pub occupancy: SlotOccupancy,
}

/// The slot to promote when a claim was starved: the lowest-indexed `Cold` one.
///
/// Lowest-indexed so growth is deterministic and reuses slots the circuit
/// breaker already has history for. Returns `None` when every slot is already
/// `Live` — the pool is at its configured ceiling and demand must queue.
pub(crate) fn pick_growth_slot(views: &[SlotView]) -> Option<usize> {
    views
        .iter()
        .position(|view| view.intent == SlotIntent::Cold)
}

/// The slot to shut down, or `None` if nothing should be reaped right now.
///
/// A slot qualifies only when **all** of these hold:
///
/// - more than `min_warm` slots are idle, so the pool never gives up its last
///   warm agent;
/// - its occupancy is [`SlotOccupancy::Idle`] — the single guard that keeps a
///   checked-out slot safe;
/// - it has been idle for at least `idle_ttl`.
///
/// The longest-idle slot wins, with the highest index breaking ties so reaping
/// walks back down toward slot 0 (which growth refills first).
pub(crate) fn pick_reap_slot(
    views: &[SlotView],
    now: Instant,
    idle_ttl: Duration,
    min_warm: usize,
) -> Option<usize> {
    let idle_count = views
        .iter()
        .filter(|view| matches!(view.occupancy, SlotOccupancy::Idle(_)))
        .count();
    if idle_count <= min_warm {
        return None;
    }

    views
        .iter()
        .enumerate()
        .filter_map(|(index, view)| match view.occupancy {
            SlotOccupancy::Idle(since) => Some((index, now.saturating_duration_since(since))),
            _ => None,
        })
        .filter(|(_, idle_for)| *idle_for >= idle_ttl)
        .max_by_key(|(index, idle_for)| (*idle_for, *index))
        .map(|(index, _)| index)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cold() -> SlotView {
        SlotView {
            intent: SlotIntent::Cold,
            occupancy: SlotOccupancy::Empty,
        }
    }

    fn live_empty() -> SlotView {
        SlotView {
            intent: SlotIntent::Live,
            occupancy: SlotOccupancy::Empty,
        }
    }

    fn live_idle(since: Instant) -> SlotView {
        SlotView {
            intent: SlotIntent::Live,
            occupancy: SlotOccupancy::Idle(since),
        }
    }

    fn live_busy() -> SlotView {
        SlotView {
            intent: SlotIntent::Live,
            occupancy: SlotOccupancy::CheckedOut,
        }
    }

    #[test]
    fn growth_promotes_the_lowest_cold_slot() {
        let base = Instant::now();
        let views = [live_idle(base), cold(), cold()];
        assert_eq!(pick_growth_slot(&views), Some(1));
    }

    #[test]
    fn growth_stops_at_the_configured_ceiling() {
        let base = Instant::now();
        let views = [live_idle(base), live_busy(), live_empty()];
        assert_eq!(pick_growth_slot(&views), None);
    }

    /// The invariant the whole reaper rests on: a slot with a turn running
    /// against it is never selected, no matter how stale its last idle stamp.
    #[test]
    fn reaper_never_selects_a_checked_out_slot() {
        let base = Instant::now();
        let now = base + Duration::from_secs(10_000);
        // Three idle slots clear the min_warm floor, so the only reason to
        // skip the busy slot is that it is busy.
        let views = [
            live_busy(),
            live_idle(now - Duration::from_secs(1)),
            live_idle(now - Duration::from_secs(1)),
            live_idle(now - Duration::from_secs(1)),
        ];
        let picked = pick_reap_slot(&views, now, Duration::ZERO, MIN_WARM_SLOTS);
        assert_ne!(picked, Some(0), "checked-out slot 0 must never be reaped");
        assert!(matches!(picked, Some(1..=3)));
    }

    /// Even when every other slot is busy and the busy one is by far the
    /// stalest, the reaper returns nothing rather than reaching for it.
    #[test]
    fn reaper_returns_none_when_the_only_stale_slot_is_checked_out() {
        let base = Instant::now();
        let now = base + Duration::from_secs(10_000);
        let views = [live_busy(), live_busy(), live_idle(now)];
        assert_eq!(
            pick_reap_slot(&views, now, Duration::ZERO, MIN_WARM_SLOTS),
            None
        );
    }

    #[test]
    fn reaper_keeps_one_warm_slot() {
        let base = Instant::now();
        let now = base + Duration::from_secs(10_000);
        let views = [live_idle(base), cold(), cold()];
        assert_eq!(pick_reap_slot(&views, now, IDLE_SLOT_TTL, MIN_WARM_SLOTS), None);

        let views = [live_idle(base), live_idle(base), cold()];
        assert_eq!(
            pick_reap_slot(&views, now, IDLE_SLOT_TTL, MIN_WARM_SLOTS),
            Some(1)
        );
    }

    #[test]
    fn reaper_waits_for_the_idle_ttl() {
        let base = Instant::now();
        let views = [live_idle(base), live_idle(base)];

        let just_early = base + IDLE_SLOT_TTL - Duration::from_secs(1);
        assert_eq!(
            pick_reap_slot(&views, just_early, IDLE_SLOT_TTL, MIN_WARM_SLOTS),
            None
        );

        let due = base + IDLE_SLOT_TTL;
        assert_eq!(
            pick_reap_slot(&views, due, IDLE_SLOT_TTL, MIN_WARM_SLOTS),
            Some(1)
        );
    }

    #[test]
    fn reaper_prefers_the_longest_idle_slot() {
        let base = Instant::now();
        let now = base + Duration::from_secs(10_000);
        let views = [
            live_idle(now - Duration::from_secs(700)),
            live_idle(now - Duration::from_secs(5_000)),
            live_idle(now - Duration::from_secs(900)),
        ];
        assert_eq!(
            pick_reap_slot(&views, now, IDLE_SLOT_TTL, MIN_WARM_SLOTS),
            Some(1)
        );
    }

    /// A slot that is empty because it crashed is not idle and must not be
    /// counted toward the warm floor — otherwise the reaper would think the
    /// pool is roomier than it is and shut down its last live agent.
    #[test]
    fn empty_slots_do_not_count_as_warm() {
        let base = Instant::now();
        let now = base + Duration::from_secs(10_000);
        let views = [live_idle(base), live_empty(), live_empty(), cold()];
        assert_eq!(
            pick_reap_slot(&views, now, IDLE_SLOT_TTL, MIN_WARM_SLOTS),
            None
        );
    }
}
