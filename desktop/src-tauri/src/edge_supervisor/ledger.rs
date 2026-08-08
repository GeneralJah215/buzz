//! The persisted failure ledger.
//!
//! Split out of `edge_supervisor.rs` so the policy file stays under the
//! repository's 1000-line ratchet, and because this file has one job: hold the
//! counters that bound every retry loop in the supervisor.
//!
//! # Why the counters are charged *before* the effect
//!
//! The ledger is the whole attempt cap. If the effect runs first and the
//! counter is written afterwards, then any launch where the write fails — a
//! read-only app data directory, antivirus or a sync client holding the file
//! during the create-then-rename, a crash between the effect and the write —
//! loses the attempt entirely. Every launch then loads a fresh ledger, retries,
//! fails to persist, and discards the evidence: `task_cap_spent()` is never
//! true, `GIVING UP` is never logged, and the repair is retried forever. That
//! is buzz-ops GRD-009 (1,316 restart attempts, zero successes) reproduced
//! exactly, through the one branch the old code warned about and then ignored.
//!
//! So the orchestration charges an attempt with [`SupervisorLedger::charge_task_attempt`]
//! or [`SupervisorLedger::charge_spawn_attempt`], persists it, and only then
//! runs the effect. A launch that cannot persist the charge does not act at
//! all. Over-counting (a charged attempt whose success write is later lost) is
//! the safe direction; under-counting is the unbounded loop.

use std::io::Write as _;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::{MAX_REPAIR_ATTEMPTS, MAX_UNDETERMINED_LAUNCHES};

/// Persisted failure state. Persistence is the point: an in-memory counter
/// resets every launch, and "3 attempts per launch, forever" is still an
/// unbounded loop across a day of restarts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SupervisorLedger {
    /// Consecutive failed task repairs.
    pub task_failures: u32,
    /// Latched once the task cap is spent, so `GIVING UP` is logged once.
    pub task_gave_up: bool,
    /// App version the task was last confirmed registered for.
    pub registered_version: Option<String>,
    /// Consecutive failed sidecar spawns.
    pub spawn_failures: u32,
    /// Latched once the spawn cap is spent.
    pub spawn_gave_up: bool,
    /// Most recent failure text, for the operator.
    pub last_failure: Option<String>,
    /// Consecutive launches on which the task state could not be read at all.
    /// Not a failure count — nothing was attempted — but a state that never
    /// resolves needs an escalation, or the supervisor prints the same line at
    /// every launch forever and nobody ever looks.
    pub undetermined_streak: u32,
    /// Latched once the undetermined streak has been escalated.
    pub undetermined_escalated: bool,
}

/// A ledger read plus anything that went wrong reading it. The warning is
/// carried rather than swallowed: a corrupt ledger silently resetting the
/// failure cap is how a bounded retry becomes an unbounded one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerLoad {
    pub ledger: SupervisorLedger,
    pub warning: Option<String>,
}

impl SupervisorLedger {
    /// Charge a task-repair attempt **before** it is made.
    ///
    /// The `gave_up` latch is deliberately *not* set here. It exists only to
    /// make `GIVING UP` appear exactly once, and it is set by the reporting
    /// path (`latch_task_give_up`) at the moment that line is written. Setting
    /// it here would latch silently and the operator would never be told.
    pub fn charge_task_attempt(&mut self) {
        self.task_failures = self.task_failures.saturating_add(1);
        self.last_failure = Some("scheduled-task repair attempted; outcome not yet known".into());
    }

    /// Charge a spawn attempt **before** it is made. See
    /// [`Self::charge_task_attempt`] for why the latch is not set here.
    pub fn charge_spawn_attempt(&mut self) {
        self.spawn_failures = self.spawn_failures.saturating_add(1);
        self.last_failure = Some("sidecar spawn attempted; outcome not yet known".into());
    }

    /// Replace the placeholder reason on an already-charged attempt with what
    /// actually went wrong. Does **not** increment: the attempt was charged
    /// before the effect ran, and charging it twice would halve the cap.
    pub fn note_failure(&mut self, reason: String) {
        self.last_failure = Some(reason);
    }

    /// True once the task cap is spent.
    pub fn task_cap_spent(&self) -> bool {
        self.task_gave_up || self.task_failures >= MAX_REPAIR_ATTEMPTS
    }

    /// True once the spawn cap is spent.
    pub fn spawn_cap_spent(&self) -> bool {
        self.spawn_gave_up || self.spawn_failures >= MAX_REPAIR_ATTEMPTS
    }

    /// Task confirmed registered for `version` — clear the cap.
    pub fn record_task_success(&mut self, version: &str) {
        self.task_failures = 0;
        self.task_gave_up = false;
        self.registered_version = Some(version.to_string());
    }

    /// The sidecar was heard from. Clear the spawn cap.
    pub fn record_sidecar_healthy(&mut self) {
        self.spawn_failures = 0;
        self.spawn_gave_up = false;
    }

    /// The task state could not be read on this launch.
    pub fn charge_undetermined(&mut self) {
        self.undetermined_streak = self.undetermined_streak.saturating_add(1);
    }

    /// The task state was readable, whatever it said. Clears the streak so a
    /// one-off unreadable query never escalates.
    pub fn clear_undetermined(&mut self) {
        self.undetermined_streak = 0;
        self.undetermined_escalated = false;
    }

    /// True once the task state has been unreadable for long enough that it is
    /// not a transient condition any more.
    pub fn undetermined_cap_spent(&self) -> bool {
        self.undetermined_streak >= MAX_UNDETERMINED_LAUNCHES
    }

    /// Read the ledger. A missing file is a fresh install; an unreadable or
    /// corrupt one falls back to a fresh ledger **and** reports why.
    pub fn load_from(path: &Path) -> LedgerLoad {
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return LedgerLoad {
                    ledger: Self::default(),
                    warning: None,
                }
            }
            Err(error) => {
                return LedgerLoad {
                    ledger: Self::default(),
                    warning: Some(format!(
                        "edge supervisor ledger unreadable at {}: {error} — failure counters \
                         restart from zero this launch",
                        path.display()
                    )),
                }
            }
        };
        match serde_json::from_str::<Self>(&raw) {
            Ok(ledger) => LedgerLoad {
                ledger,
                warning: None,
            },
            Err(error) => LedgerLoad {
                ledger: Self::default(),
                warning: Some(format!(
                    "edge supervisor ledger at {} is corrupt: {error} — failure counters restart \
                     from zero this launch",
                    path.display()
                )),
            },
        }
    }

    /// Write the ledger atomically. A torn write here would corrupt the exact
    /// state that bounds the retry loop.
    pub fn store_to(&self, path: &Path) -> Result<(), String> {
        use atomic_write_file::AtomicWriteFile;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                format!(
                    "create edge supervisor ledger directory {}: {error}",
                    parent.display()
                )
            })?;
        }
        let body = serde_json::to_vec_pretty(self)
            .map_err(|error| format!("serialize edge supervisor ledger: {error}"))?;
        let mut file = AtomicWriteFile::open(path)
            .map_err(|error| format!("open {} for atomic write: {error}", path.display()))?;
        file.write_all(&body)
            .map_err(|error| format!("write {}: {error}", path.display()))?;
        file.commit()
            .map_err(|error| format!("commit {}: {error}", path.display()))
    }
}
