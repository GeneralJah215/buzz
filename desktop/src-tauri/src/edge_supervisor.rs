//! Scheduled-task supervisor for the optional `buzz-edge` sidecar
//! (SPEC-2026-08-05, Behavior contract §16 / packaging note).
//!
//! The NSIS installer registers a logon Scheduled Task (`windows/edge-task.nsi`).
//! A logon task alone guarantees neither start-before-Desktop nor crash
//! recovery, so this module closes both gaps at every Desktop launch:
//!
//! 1. **Verify and repair** the task registration (missing, wrong path, or
//!    stale after an upgrade).
//! 2. **Readiness gate** — a bounded health probe, after which Desktop
//!    cleanly selects canonical-only routing instead of waiting.
//! 3. **Health-check respawn** — start the sidecar when, and only when, the
//!    probe proves it is not running.
//!
//! # Why the decisions are pure and the effects are behind a trait
//!
//! Every decision here is a pure function over inputs; every effect (querying
//! the task, registering it, spawning the process) goes through
//! [`SupervisorHost`]. That is not a style preference — it is what lets the
//! entire policy be tested without registering a task, starting a service, or
//! spawning a process on a real machine. The test suite drives the pure half
//! exclusively; `WindowsHost` is never constructed by a test.
//!
//! # The failure mode this module is shaped around
//!
//! A supervisor that keeps repairing something that keeps failing is worse
//! than no supervisor. The agent watchdog on this project did exactly that:
//! 1,316 restart attempts against 22 agents over 26 hours, zero successes
//! (buzz-ops GRD-009), because (a) there was no attempt cap and (b) an
//! *unknown* state was read as *dead* (GRD-010). Both disciplines are applied
//! here:
//!
//! * every repair and every spawn is capped at [`MAX_REPAIR_ATTEMPTS`]
//!   consecutive failures, counted in a **persisted** ledger, after which the
//!   supervisor gives up and says so exactly once;
//! * a repair counts as successful only when a **re-query confirms** it — an
//!   `Ok(())` from the effect layer is not evidence, which is precisely the
//!   assumption that produced the 1,316 attempts;
//! * [`SidecarHealth::Indeterminate`] is its own outcome and never triggers a
//!   spawn, and neither does [`SidecarHealth::RunningUnhealthy`] — spawning a
//!   second copy onto the same loopback port makes a bad state worse.
//!
//! # Off by default
//!
//! The whole feature is inert unless `BUZZ_EDGE_RELAY_URL` is set. With it
//! unset [`supervise_launch`] performs **no** task query, **no** probe, **no**
//! file write and emits **no** log line — an unset variable is exactly today's
//! canonical-only behavior, which is also the spec's first-line rollback.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub mod host;

/// Scheduled-task name. MUST stay identical to the `TASK_NAME` define in
/// `windows/edge-task.nsi`; a test asserts both files agree, because a drift
/// here means the installer registers one task and the supervisor endlessly
/// "repairs" a different missing one.
pub const TASK_NAME: &str = "Buzz Edge Sidecar";

/// Sidecar file name, as installed next to the Desktop executable by Tauri's
/// `externalBin` handling (the target-triple suffix is stripped at bundle time).
pub const SIDECAR_EXE: &str = "buzz-edge.exe";

/// Readiness deadline from the spec's packaging note. After this the client
/// selects canonical-only routing and retries the edge in the background. It
/// is a *ceiling on waiting*, never a target — nothing may block on it.
pub const READINESS_DEADLINE: Duration = Duration::from_secs(2);

/// Consecutive-failure cap for both task repair and sidecar spawn, mirroring
/// buzz-ops GRD-009. Something that has failed three times in a row is not
/// going to succeed on the fourth; stop and leave it for a human.
pub const MAX_REPAIR_ATTEMPTS: u32 = 3;

/// Persisted ledger file name, stored in the app data directory.
pub const LEDGER_FILE: &str = "edge-supervisor.json";

const GUARDRAIL: &str = "[GUARDRAIL]";
const AUTO_HEAL: &str = "[AUTO-HEAL]";

// ───────────────────────────── inputs ──────────────────────────────

/// Everything the supervisor needs to know about this launch. Built by
/// [`launch_env`] in production and by hand in tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisorEnv {
    /// `BUZZ_EDGE_RELAY_URL`. `None` means the feature is off and the
    /// supervisor must do nothing at all.
    pub edge_relay_url: Option<String>,
    /// Absolute path the scheduled task must point at.
    pub sidecar_exe: PathBuf,
    /// Current app version. A change re-registers the task, which is how an
    /// upgrade is detected when the install path is unchanged.
    pub app_version: String,
}

/// The registration the task query returned.
///
/// `Unknown` exists because "I could not tell" is a real answer with its own
/// handling. Collapsing it into `Missing` is the GRD-010 defect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskRegistration {
    /// The query completed and no such task exists.
    Missing,
    /// The query completed; `command_line` is the action the task will run.
    Registered { command_line: String },
    /// The query itself failed. Never treated as `Missing`.
    Unknown { reason: String },
}

/// What the health probe established about the sidecar process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidecarHealth {
    /// Definitively not running — the loopback connection was *refused*.
    NotRunning,
    /// Answering and reporting itself healthy.
    Healthy,
    /// Answering, but reporting a fault. Respawning would put a second copy
    /// on the same port; the scheduled task owns process lifecycle here.
    RunningUnhealthy { reason: String },
    /// Could not be established (timeout, unreadable response, probe error
    /// that does not distinguish dead from busy). Never a spawn trigger.
    Indeterminate { reason: String },
}

/// The exact task registration the installer and the supervisor must agree on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSpec {
    pub task_name: String,
    pub sidecar_exe: PathBuf,
    pub app_version: String,
}

// ───────────────────────────── decisions ──────────────────────────────

/// Why the task needs re-registering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepairReason {
    /// No task registered at all.
    Missing,
    /// Registered, but the action does not resolve to the expected binary.
    /// An unquoted path containing a space lands here on purpose.
    CommandMismatch,
    /// Registered and pointing at the right path, but registered by an older
    /// app version — the upgrade case.
    VersionChanged,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskAction {
    /// Registration already matches. Nothing to do; failure counters clear.
    LeaveAlone,
    /// Re-register the task.
    Register { reason: RepairReason },
    /// The consecutive-failure cap is spent. Stop; a human has to look.
    GiveUp { attempts: u32 },
    /// The task's state could not be determined, so no repair is attempted.
    /// This is *not* a pass — it is reported and retried at the next launch.
    Undetermined { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HealthAction {
    /// Sidecar answered healthy. Counters clear.
    None,
    /// Proven not running, under the cap: start it.
    Spawn,
    /// Running but faulted. Reported, never respawned.
    ReportUnhealthy { reason: String },
    /// Unknown state. Observed and reported, never respawned.
    Observe { reason: String },
    /// Spawn cap spent.
    GiveUp { attempts: u32 },
}

/// Whether message traffic may use the edge on this launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadinessDecision {
    /// Sidecar answered healthy inside the deadline.
    EdgeReady,
    /// Canonical-only for now; the edge is retried in the background.
    CanonicalOnlyRetry { reason: String },
}

/// Decide what to do about the scheduled-task registration.
///
/// Pure. The order matters: a healthy registration is recognised *before* the
/// give-up latch is consulted, so a genuine one-off failure still self-heals
/// once the task is observed correct (GRD-009's "cleared the moment the agent
/// is heard from again").
pub fn decide_task_repair(
    registration: &TaskRegistration,
    spec: &TaskSpec,
    ledger: &SupervisorLedger,
) -> TaskAction {
    let reason = match registration {
        TaskRegistration::Unknown { reason } => {
            return TaskAction::Undetermined {
                reason: reason.clone(),
            }
        }
        TaskRegistration::Missing => RepairReason::Missing,
        TaskRegistration::Registered { command_line } => {
            if !command_line_matches(command_line, &spec.sidecar_exe) {
                RepairReason::CommandMismatch
            } else if ledger.registered_version.as_deref() != Some(spec.app_version.as_str()) {
                RepairReason::VersionChanged
            } else {
                return TaskAction::LeaveAlone;
            }
        }
    };

    if ledger.task_cap_spent() {
        return TaskAction::GiveUp {
            attempts: ledger.task_failures,
        };
    }
    TaskAction::Register { reason }
}

/// Decide what to do about the sidecar process.
///
/// Pure, and deliberately conservative: only [`SidecarHealth::NotRunning`] —
/// a *refused* loopback connection — authorises a spawn.
pub fn decide_health_action(health: &SidecarHealth, ledger: &SupervisorLedger) -> HealthAction {
    match health {
        SidecarHealth::Healthy => HealthAction::None,
        SidecarHealth::RunningUnhealthy { reason } => HealthAction::ReportUnhealthy {
            reason: reason.clone(),
        },
        SidecarHealth::Indeterminate { reason } => HealthAction::Observe {
            reason: reason.clone(),
        },
        SidecarHealth::NotRunning => {
            if ledger.spawn_cap_spent() {
                HealthAction::GiveUp {
                    attempts: ledger.spawn_failures,
                }
            } else {
                HealthAction::Spawn
            }
        }
    }
}

/// Map the probe result onto the routing decision.
///
/// Anything short of `Healthy` is canonical-only. This is the feature's
/// first-line rollback expressed as code: an edge that never becomes ready
/// degrades, it does not hold anything up.
pub fn decide_readiness(health: &SidecarHealth) -> ReadinessDecision {
    match health {
        SidecarHealth::Healthy => ReadinessDecision::EdgeReady,
        SidecarHealth::NotRunning => ReadinessDecision::CanonicalOnlyRetry {
            reason: "sidecar not running".to_string(),
        },
        SidecarHealth::RunningUnhealthy { reason } | SidecarHealth::Indeterminate { reason } => {
            ReadinessDecision::CanonicalOnlyRetry {
                reason: reason.clone(),
            }
        }
    }
}

/// Classify a probe transport failure without guessing.
///
/// A *refused* connection is the only failure that proves nothing is
/// listening. A timeout, a DNS-ish error, a TLS error, a body that will not
/// parse — none of those distinguish "dead" from "busy", so they are
/// `Indeterminate`. Reading them as "dead" is the GRD-010 defect that turned
/// 22 healthy agents into 1,316 restarts.
pub fn classify_probe_failure(is_connect_refused: bool, is_timeout: bool) -> SidecarHealth {
    if is_connect_refused {
        return SidecarHealth::NotRunning;
    }
    if is_timeout {
        return SidecarHealth::Indeterminate {
            reason: format!("health probe timed out after {READINESS_DEADLINE:?}"),
        };
    }
    SidecarHealth::Indeterminate {
        reason: "health probe failed without proving the sidecar is down".to_string(),
    }
}

/// Classify a probe that got an HTTP answer back.
pub fn classify_probe_response(status: u16, body: &str) -> SidecarHealth {
    if !(200..300).contains(&status) {
        return SidecarHealth::RunningUnhealthy {
            reason: format!("health endpoint returned HTTP {status}"),
        };
    }
    match serde_json::from_str::<serde_json::Value>(body) {
        Ok(value) => match value.get("ok").and_then(serde_json::Value::as_bool) {
            Some(true) => SidecarHealth::Healthy,
            Some(false) => SidecarHealth::RunningUnhealthy {
                reason: value
                    .get("reason")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("sidecar reported itself unhealthy")
                    .to_string(),
            },
            // Answering but not in the agreed shape: we cannot tell.
            None => SidecarHealth::Indeterminate {
                reason: "health response has no `ok` field".to_string(),
            },
        },
        Err(error) => SidecarHealth::Indeterminate {
            reason: format!("health response is not JSON: {error}"),
        },
    }
}

// ───────────────────────── command-line handling ─────────────────────────

/// The task action string: the sidecar path, quoted.
///
/// Acceptance item 17 requires a quoted path, and this path contains
/// `Program Files`. An unquoted `C:\Program Files\Buzz\buzz-edge.exe` is read
/// by Windows as `C:\Program.exe` with arguments — the classic unquoted
/// service-path privilege problem. A path that itself contains a double quote
/// cannot be quoted safely, so it is refused rather than mangled.
pub fn quoted_task_command(exe: &Path) -> Result<String, String> {
    let text = exe
        .to_str()
        .ok_or_else(|| format!("sidecar path is not valid UTF-8: {}", exe.display()))?;
    if text.contains('"') {
        return Err(format!(
            "refusing to register a scheduled task for a path containing a quote: {text}"
        ));
    }
    Ok(format!("\"{text}\""))
}

/// Extract the executable from an observed task action and compare it with
/// the expected path.
///
/// An *unquoted* action containing a space does not match, even when the full
/// string looks right: Windows would not run that binary, so the registration
/// is broken and must be repaired.
pub fn command_line_matches(command_line: &str, expected_exe: &Path) -> bool {
    let Some(expected) = expected_exe.to_str() else {
        return false;
    };
    let observed = command_line.trim();
    let extracted = if let Some(rest) = observed.strip_prefix('"') {
        match rest.find('"') {
            Some(end) => &rest[..end],
            // Unterminated quote: broken registration.
            None => return false,
        }
    } else {
        // Unquoted. Windows stops at the first space, so only a space-free
        // action can possibly name the whole path.
        match observed.split_once(' ') {
            Some(_) => return false,
            None => observed,
        }
    };
    normalize_path(extracted) == normalize_path(expected)
}

/// Windows paths are case-insensitive and accept either separator.
fn normalize_path(value: &str) -> String {
    value.replace('/', "\\").to_ascii_lowercase()
}

// ───────────────────────────── ledger ──────────────────────────────

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
    /// Record a failed task repair.
    ///
    /// The `gave_up` latch is deliberately *not* set here. It exists only to
    /// make `GIVING UP` appear exactly once, and it is set by the reporting
    /// path (`latch_task_give_up`) at the moment that line is written. Setting
    /// it here would latch silently and the operator would never be told.
    pub fn record_task_failure(&mut self, reason: String) {
        self.task_failures = self.task_failures.saturating_add(1);
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

    /// Record a failed spawn. See [`Self::record_task_failure`] for why the
    /// latch is not set here.
    pub fn record_spawn_failure(&mut self, reason: String) {
        self.spawn_failures = self.spawn_failures.saturating_add(1);
        self.last_failure = Some(reason);
    }

    /// The sidecar was heard from. Clear the spawn cap.
    pub fn record_sidecar_healthy(&mut self) {
        self.spawn_failures = 0;
        self.spawn_gave_up = false;
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

// ───────────────────────── effects seam + report ─────────────────────────

/// Every effect the supervisor can have on the machine. Production wires
/// `WindowsHost`; tests wire a fake and assert on the calls it recorded.
pub trait SupervisorHost {
    /// Query the scheduled task. Must return [`TaskRegistration::Unknown`] —
    /// never `Missing` — when the query itself fails.
    fn query_task(&self, task_name: &str) -> TaskRegistration;
    /// Register (or overwrite) the task.
    fn register_task(&self, spec: &TaskSpec) -> Result<(), String>;
    /// Probe sidecar health. Must return within [`READINESS_DEADLINE`].
    fn probe_health(&self) -> SidecarHealth;
    /// Start the sidecar.
    fn spawn_sidecar(&self, exe: &Path) -> Result<(), String>;
    fn load_ledger(&self) -> LedgerLoad;
    fn store_ledger(&self, ledger: &SupervisorLedger) -> Result<(), String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisorLog {
    pub prefix: &'static str,
    pub message: String,
}

/// Outcome of one launch. `SupervisionReport::default()` is the inert result
/// produced when `BUZZ_EDGE_RELAY_URL` is unset: no actions, no logs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SupervisionReport {
    pub task_action: Option<TaskAction>,
    pub health_action: Option<HealthAction>,
    pub readiness: Option<ReadinessDecision>,
    pub logs: Vec<SupervisorLog>,
}

impl SupervisionReport {
    fn guardrail(&mut self, message: impl Into<String>) {
        self.logs.push(SupervisorLog {
            prefix: GUARDRAIL,
            message: message.into(),
        });
    }

    fn auto_heal(&mut self, message: impl Into<String>) {
        self.logs.push(SupervisorLog {
            prefix: AUTO_HEAL,
            message: message.into(),
        });
    }

    /// Print the report. The single write to stderr in this module lives here,
    /// so "the feature is off ⇒ nothing is logged" is a property of the empty
    /// log vector rather than of scattered print statements.
    pub fn emit(&self) {
        for log in &self.logs {
            eprintln!(
                "buzz-desktop: {} edge-supervisor: {}",
                log.prefix, log.message
            );
        }
    }
}

// ───────────────────────────── orchestration ──────────────────────────────

/// Run one launch's supervision: verify/repair the task, gate on readiness,
/// respawn if — and only if — the sidecar is proven down.
///
/// With `edge_relay_url` unset this returns immediately having touched
/// nothing: no host call, no file, no log line.
pub fn supervise_launch<H: SupervisorHost>(host: &H, env: &SupervisorEnv) -> SupervisionReport {
    let mut report = SupervisionReport::default();
    if env.edge_relay_url.is_none() {
        return report;
    }

    let spec = TaskSpec {
        task_name: TASK_NAME.to_string(),
        sidecar_exe: env.sidecar_exe.clone(),
        app_version: env.app_version.clone(),
    };

    let LedgerLoad {
        mut ledger,
        warning,
    } = host.load_ledger();
    if let Some(warning) = warning {
        report.guardrail(warning);
    }

    supervise_task(host, &spec, &mut ledger, &mut report);
    supervise_health(host, &spec, &mut ledger, &mut report);

    if let Err(error) = host.store_ledger(&ledger) {
        // Losing the ledger means losing the attempt cap, so this is loud.
        report.guardrail(format!(
            "could not persist supervisor ledger ({error}); the repair cap is not durable this \
             launch"
        ));
    }
    report
}

fn supervise_task<H: SupervisorHost>(
    host: &H,
    spec: &TaskSpec,
    ledger: &mut SupervisorLedger,
    report: &mut SupervisionReport,
) {
    let registration = host.query_task(&spec.task_name);
    let action = decide_task_repair(&registration, spec, ledger);
    match &action {
        TaskAction::LeaveAlone => {
            ledger.record_task_success(&spec.app_version);
        }
        TaskAction::Undetermined { reason } => {
            // Not a pass. No repair is attempted, and it is said out loud.
            report.guardrail(format!(
                "scheduled-task state for \"{}\" could not be determined ({reason}); no repair \
                 attempted this launch",
                spec.task_name
            ));
        }
        TaskAction::GiveUp { .. } => latch_task_give_up(spec, ledger, report),
        TaskAction::Register { reason } => {
            report.auto_heal(format!(
                "repairing scheduled task \"{}\" ({reason:?}), attempt {} of {MAX_REPAIR_ATTEMPTS}",
                spec.task_name,
                ledger.task_failures + 1
            ));
            match host.register_task(spec) {
                Ok(()) => confirm_task_repair(host, spec, ledger, report),
                Err(error) => {
                    ledger.record_task_failure(format!("register task: {error}"));
                    report.guardrail(format!(
                        "scheduled-task repair failed ({error}); {} of {MAX_REPAIR_ATTEMPTS} \
                         consecutive failures",
                        ledger.task_failures
                    ));
                    latch_task_give_up(spec, ledger, report);
                }
            }
        }
    }
    report.task_action = Some(action);
}

/// Say `GIVING UP` once, on the launch the cap is reached, and latch it.
///
/// The latch lives here rather than in the ledger's failure recorder so it can
/// never be set without the operator being told (GRD-009: the watchdog's worst
/// property was failing 1,316 times and "never once reporting that anything
/// was wrong").
fn latch_task_give_up(
    spec: &TaskSpec,
    ledger: &mut SupervisorLedger,
    report: &mut SupervisionReport,
) {
    if !ledger.task_cap_spent() || ledger.task_gave_up {
        return;
    }
    ledger.task_gave_up = true;
    report.guardrail(format!(
        "GIVING UP repairing scheduled task \"{}\" after {} consecutive failures; edge routing \
         stays canonical-only until a human fixes it",
        spec.task_name, ledger.task_failures
    ));
}

/// Spawn counterpart of [`latch_task_give_up`].
fn latch_spawn_give_up(ledger: &mut SupervisorLedger, report: &mut SupervisionReport) {
    if !ledger.spawn_cap_spent() || ledger.spawn_gave_up {
        return;
    }
    ledger.spawn_gave_up = true;
    report.guardrail(format!(
        "GIVING UP starting the edge sidecar after {} consecutive failures; routing stays \
         canonical-only until a human fixes it",
        ledger.spawn_failures
    ));
}

/// Re-query after a repair. An `Ok(())` from the effect layer is a claim, not
/// evidence — trusting it is exactly how a broken restart path logged success
/// 1,316 times while nothing recovered (GRD-009).
fn confirm_task_repair<H: SupervisorHost>(
    host: &H,
    spec: &TaskSpec,
    ledger: &mut SupervisorLedger,
    report: &mut SupervisionReport,
) {
    match host.query_task(&spec.task_name) {
        TaskRegistration::Registered { command_line }
            if command_line_matches(&command_line, &spec.sidecar_exe) =>
        {
            ledger.record_task_success(&spec.app_version);
        }
        TaskRegistration::Unknown { reason } => {
            // Unverified is not verified. It consumes an attempt so a
            // permanently unreadable task cannot be retried forever.
            ledger.record_task_failure(format!("repair could not be verified: {reason}"));
            report.guardrail(format!(
                "scheduled-task repair could not be verified ({reason}); counted as failure {} of \
                 {MAX_REPAIR_ATTEMPTS}",
                ledger.task_failures
            ));
            latch_task_give_up(spec, ledger, report);
        }
        other => {
            ledger
                .record_task_failure(format!("repair reported success but query shows {other:?}"));
            report.guardrail(format!(
                "scheduled-task repair reported success but the task is still wrong; failure {} \
                 of {MAX_REPAIR_ATTEMPTS}",
                ledger.task_failures
            ));
            latch_task_give_up(spec, ledger, report);
        }
    }
}

fn supervise_health<H: SupervisorHost>(
    host: &H,
    spec: &TaskSpec,
    ledger: &mut SupervisorLedger,
    report: &mut SupervisionReport,
) {
    let health = host.probe_health();
    report.readiness = Some(decide_readiness(&health));
    let action = decide_health_action(&health, ledger);
    match &action {
        HealthAction::None => ledger.record_sidecar_healthy(),
        HealthAction::ReportUnhealthy { reason } => {
            report.guardrail(format!(
                "sidecar is running but unhealthy ({reason}); not respawning — a second copy on \
                 the same port would make this worse. Routing stays canonical-only"
            ));
        }
        HealthAction::Observe { reason } => {
            report.guardrail(format!(
                "sidecar health could not be determined ({reason}); not respawning. Routing stays \
                 canonical-only"
            ));
        }
        HealthAction::GiveUp { .. } => latch_spawn_give_up(ledger, report),
        HealthAction::Spawn => {
            report.auto_heal(format!(
                "sidecar is not running; starting it, attempt {} of {MAX_REPAIR_ATTEMPTS}",
                ledger.spawn_failures + 1
            ));
            match host.spawn_sidecar(&spec.sidecar_exe) {
                Ok(()) => confirm_spawn(host, ledger, report),
                Err(error) => {
                    ledger.record_spawn_failure(format!("spawn sidecar: {error}"));
                    report.guardrail(format!(
                        "sidecar spawn failed ({error}); {} of {MAX_REPAIR_ATTEMPTS} consecutive \
                         failures",
                        ledger.spawn_failures
                    ));
                    latch_spawn_give_up(ledger, report);
                }
            }
        }
    }
    report.health_action = Some(action);
}

/// Re-probe after a spawn. Only a healthy answer clears the cap — a spawn that
/// returns `Ok` and then dies must still count.
fn confirm_spawn<H: SupervisorHost>(
    host: &H,
    ledger: &mut SupervisorLedger,
    report: &mut SupervisionReport,
) {
    match host.probe_health() {
        SidecarHealth::Healthy => {
            ledger.record_sidecar_healthy();
            report.auto_heal("sidecar started and answered healthy");
        }
        other => {
            ledger.record_spawn_failure(format!("spawn not confirmed healthy: {other:?}"));
            report.guardrail(format!(
                "sidecar spawn was not confirmed healthy ({other:?}); {} of \
                 {MAX_REPAIR_ATTEMPTS} consecutive failures",
                ledger.spawn_failures
            ));
            latch_spawn_give_up(ledger, report);
        }
    }
}

/// Run `probe` on a detached worker and give up on it at `deadline`.
///
/// The readiness gate must never hold up startup. A sidecar that accepts the
/// connection and then never answers would otherwise block for as long as it
/// pleases, so the wait is bounded here rather than trusted to the caller, and
/// a blown deadline is `Indeterminate` — unknown, not dead.
pub fn probe_with_deadline<F>(probe: F, deadline: Duration) -> SidecarHealth
where
    F: FnOnce() -> SidecarHealth + Send + 'static,
{
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // A closed receiver means the deadline already fired; dropping the
        // result is the intended outcome, not a swallowed error.
        let _ = sender.send(probe());
    });
    match receiver.recv_timeout(deadline) {
        Ok(health) => health,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => SidecarHealth::Indeterminate {
            reason: format!("health probe exceeded the {deadline:?} readiness deadline"),
        },
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => SidecarHealth::Indeterminate {
            reason: "health probe worker ended without answering".to_string(),
        },
    }
}

/// Build this launch's inputs. Returns `None` when the feature is off, so the
/// caller cannot accidentally construct a live supervisor with it unset.
pub fn launch_env(app_version: &str) -> Option<SupervisorEnv> {
    let edge_relay_url = std::env::var("BUZZ_EDGE_RELAY_URL")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())?;
    let sidecar_exe = std::env::current_exe().ok()?.parent()?.join(SIDECAR_EXE);
    Some(SupervisorEnv {
        edge_relay_url: Some(edge_relay_url),
        sidecar_exe,
        app_version: app_version.to_string(),
    })
}

#[cfg(test)]
#[path = "edge_supervisor_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "edge_supervisor_packaging_tests.rs"]
mod packaging_tests;
