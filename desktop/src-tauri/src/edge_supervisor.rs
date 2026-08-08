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
//!   consecutive failures, counted in a **persisted** ledger that is written
//!   **before** the effect it counts (see `ledger.rs` for why write-ahead is
//!   the whole point), after which the supervisor gives up and says so once;
//! * a repair counts as successful only when a **re-query confirms** it — an
//!   `Ok(())` from the effect layer is not evidence, which is precisely the
//!   assumption that produced the 1,316 attempts;
//! * a spawn counts as successful only when the sidecar **answers healthy**,
//!   polled up to [`spawn_confirm_window`] because a cold-starting Rust binary
//!   that opens SQLite and binds a loopback port is not listening the
//!   microsecond `CreateProcess` returns. Probing once and immediately would
//!   score every successful launch as a failure and latch `GIVING UP` against
//!   a spawn path that works;
//! * [`SidecarHealth::Indeterminate`] is its own outcome and never triggers a
//!   spawn, and neither does [`SidecarHealth::RunningUnhealthy`] — spawning a
//!   second copy onto the same loopback port makes a bad state worse;
//! * a state that stays unreadable is escalated after
//!   [`MAX_UNDETERMINED_LAUNCHES`] launches rather than reprinting the same
//!   line forever, which is the 1,316-restart bug wearing different clothes.
//!
//! # Off by default
//!
//! The whole feature is inert unless `BUZZ_EDGE_RELAY_URL` is set. With it
//! unset [`supervise_launch`] performs **no** task query, **no** probe, **no**
//! file write and emits **no** log line — an unset variable is exactly today's
//! canonical-only behavior, which is also the spec's first-line rollback.
//!
//! # Unverified: what needs a real machine (do not read these as settled)
//!
//! Nothing in this crate registers a scheduled task, so the following are
//! *open questions*, not passing checks. They are recorded here rather than
//! guessed at.
//!
//! * **S1 — quotes inside `<Command>`.** Both writers put a literal quoted
//!   path inside `<Command>`, which Task Scheduler documents as a *program
//!   path* field, with `<Arguments>` absent. If Task Scheduler strips the
//!   quotes when it stores the definition, the read-back in
//!   `host::WindowsHost::query_task` sees an unquoted path containing a space,
//!   [`command_line_matches`] returns false **by design**, and three launches
//!   latch `GIVING UP` against a task that works perfectly. This is the
//!   highest-impact unknown here. Verifying it needs one registration on a
//!   real machine followed by `schtasks /Query /XML`.
//! * **S2 — element order in `<Settings>`.** Both writers place
//!   `<RestartOnFailure>` between `<RunOnlyIfNetworkAvailable>` and
//!   `<AllowStartOnDemand>`; exported task XML places it after `<Priority>`,
//!   and the task schema is a *sequence*, so order is significant. Because the
//!   two writers agree with each other, the drift test is green either way —
//!   it proves agreement, not validity.
//! * **S3 — task existence by substring.** `query_task` decides existence with
//!   `listing.contains(task_name)` over every task on the machine. A task
//!   named `Buzz Edge Sidecar (old)`, a same-named task in a subfolder, or the
//!   string appearing in another task's fields all read as "exists"; a
//!   subfolder match then makes the follow-up `/TN` query fail, which returns
//!   `Unknown` → [`TaskAction::Undetermined`] forever. The escalation added
//!   for [`MAX_UNDETERMINED_LAUNCHES`] makes that state *loud* instead of
//!   silent, but it does not fix the matching, which needs a real listing to
//!   settle.
//! * **S4 — doubled timeouts in the probe.** [`probe_with_deadline`]'s
//!   deadline and reqwest's own client timeout are both
//!   [`READINESS_DEADLINE`], so the outer timeout branch is probably
//!   unreachable in production, and each timed-out probe leaks a thread plus a
//!   blocking runtime until the inner timeout expires.
//! * **S5 — the uninstall taskkill filter.** `windows/edge-task.nsi` scopes
//!   its `taskkill` with `/FI "USERNAME eq <user>"` read from the environment.
//!   Whether `taskkill` matches a bare user name or requires `DOMAIN\user` is
//!   not settled here. If it does not match, the kill is a no-op and the
//!   scheduled task's `/End` is the only stop — which is the normal case
//!   anyway. It never widens the kill.

use std::path::{Path, PathBuf};
use std::time::Duration;

pub mod host;
pub mod ledger;

pub use ledger::{LedgerLoad, SupervisorLedger};

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

/// How many consecutive launches may report an unreadable task state before it
/// is escalated. An unreadable state takes no action, so it is not a failure
/// cap — it is the point at which "I cannot tell" stops being transient.
pub const MAX_UNDETERMINED_LAUNCHES: u32 = 5;

/// How many times the post-spawn confirmation probes before giving up, and how
/// long it waits between probes.
///
/// A sidecar that has just been started has to open its SQLite database and
/// bind a loopback port before it will accept a connection; until it does, the
/// probe gets ECONNREFUSED, which [`classify_probe_failure`] correctly reports
/// as [`SidecarHealth::NotRunning`]. Confirming with a single immediate probe
/// therefore scores **every successful spawn** as a failure. This window runs
/// on the detached supervision worker (`host::start` spawns it), so waiting
/// here cannot delay app startup, and the routing decision for this launch was
/// already taken from the first probe against [`READINESS_DEADLINE`].
pub const SPAWN_CONFIRM_ATTEMPTS: u32 = 6;
pub const SPAWN_CONFIRM_INTERVAL: Duration = Duration::from_millis(500);

/// Total time the post-spawn confirmation may take, for the operator-facing
/// message. Only the gaps between probes are waits, hence `- 1`.
pub fn spawn_confirm_window() -> Duration {
    SPAWN_CONFIRM_INTERVAL * (SPAWN_CONFIRM_ATTEMPTS.saturating_sub(1))
}

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
    /// The sidecar binary is not on disk, so nothing is registered. A logon
    /// task pointing at a file that does not exist fails at *every* logon,
    /// forever, and is exactly the orphan the installer refuses to create.
    SkippedMissingSidecar { path: PathBuf },
    /// The write-ahead attempt record could not be persisted, so the repair
    /// was not attempted. Produced by the orchestration, never by
    /// [`decide_task_repair`]: an attempt counter that cannot be written is
    /// not a cap, and acting without one is the unbounded loop.
    Deferred { reason: String },
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
    /// Proven not running, but there is no binary to start. Spawning would
    /// fail every launch until the cap latched against a fault that is really
    /// "this build ships no sidecar".
    SkippedMissingSidecar { path: PathBuf },
    /// Write-ahead counterpart of [`TaskAction::Deferred`].
    Deferred { reason: String },
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
/// Pure. `sidecar_present` is passed in rather than read from the filesystem
/// so the missing-binary rule is testable; the caller gets it from
/// [`SupervisorHost::sidecar_exists`].
///
/// The order matters: a healthy registration is recognised *before* the
/// give-up latch is consulted, so a genuine one-off failure still self-heals
/// once the task is observed correct (GRD-009's "cleared the moment the agent
/// is heard from again").
pub fn decide_task_repair(
    registration: &TaskRegistration,
    spec: &TaskSpec,
    ledger: &SupervisorLedger,
    sidecar_present: bool,
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

    // `schtasks /Create /XML` does not check that the <Command> path exists,
    // so without this the supervisor happily creates the orphaned logon task
    // the installer goes out of its way to delete.
    if !sidecar_present {
        return TaskAction::SkippedMissingSidecar {
            path: spec.sidecar_exe.clone(),
        };
    }
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
/// a *refused* loopback connection — authorises a spawn, and only when there
/// is a binary to spawn.
pub fn decide_health_action(
    health: &SidecarHealth,
    ledger: &SupervisorLedger,
    sidecar_present: bool,
    sidecar_exe: &Path,
) -> HealthAction {
    match health {
        SidecarHealth::Healthy => HealthAction::None,
        SidecarHealth::RunningUnhealthy { reason } => HealthAction::ReportUnhealthy {
            reason: reason.clone(),
        },
        SidecarHealth::Indeterminate { reason } => HealthAction::Observe {
            reason: reason.clone(),
        },
        SidecarHealth::NotRunning => {
            if !sidecar_present {
                HealthAction::SkippedMissingSidecar {
                    path: sidecar_exe.to_path_buf(),
                }
            } else if ledger.spawn_cap_spent() {
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
///
/// The result still has to be XML-escaped before it goes into a task
/// definition — see [`host::escape_xml_text`]. Quoting is not escaping.
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
    /// Whether the sidecar binary is on disk. A required method rather than a
    /// filesystem call inside the policy, so the missing-binary rule is
    /// testable without a real install.
    fn sidecar_exists(&self, exe: &Path) -> bool;
    /// Wait between post-spawn confirmation probes. A required method rather
    /// than a `thread::sleep` in the policy, so a test cannot accidentally
    /// spend real seconds.
    fn wait_before_reprobe(&self, delay: Duration);
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

    // The *outcome* write. Every attempt was already charged and persisted
    // before its effect ran, so losing this one can only leave a counter too
    // high — never too low, which is the direction that unbounds the loop.
    if let Err(error) = host.store_ledger(&ledger) {
        report.guardrail(format!(
            "could not persist the supervisor ledger outcome ({error}); attempt counters may read \
             high until the next successful write"
        ));
    }
    report
}

/// Charge an attempt and persist it before the effect runs. `Err` carries the
/// operator message for why the effect is being skipped.
fn charge_and_persist<H: SupervisorHost>(
    host: &H,
    ledger: &mut SupervisorLedger,
    charge: impl FnOnce(&mut SupervisorLedger),
) -> Result<(), String> {
    charge(ledger);
    host.store_ledger(ledger)
}

fn supervise_task<H: SupervisorHost>(
    host: &H,
    spec: &TaskSpec,
    ledger: &mut SupervisorLedger,
    report: &mut SupervisionReport,
) {
    let registration = host.query_task(&spec.task_name);
    let sidecar_present = host.sidecar_exists(&spec.sidecar_exe);
    let action = decide_task_repair(&registration, spec, ledger, sidecar_present);
    if !matches!(action, TaskAction::Undetermined { .. }) {
        ledger.clear_undetermined();
    }
    match &action {
        TaskAction::LeaveAlone => {
            ledger.record_task_success(&spec.app_version);
        }
        TaskAction::Undetermined { reason } => {
            // Not a pass. No repair is attempted, and it is said out loud.
            ledger.charge_undetermined();
            report.guardrail(format!(
                "scheduled-task state for \"{}\" could not be determined ({reason}); no repair \
                 attempted this launch",
                spec.task_name
            ));
            escalate_undetermined(spec, ledger, report);
        }
        TaskAction::SkippedMissingSidecar { path } => {
            report.guardrail(format!(
                "the edge sidecar is not installed at {}; refusing to register a logon task that \
                 would fail at every logon. Routing stays canonical-only",
                path.display()
            ));
        }
        // Produced only by the write-ahead branch below, never by the decision.
        TaskAction::Deferred { .. } => {}
        TaskAction::GiveUp { .. } => latch_task_give_up(spec, ledger, report),
        TaskAction::Register { reason } => {
            // Write-ahead: the attempt is durable before the effect happens.
            if let Err(error) =
                charge_and_persist(host, ledger, SupervisorLedger::charge_task_attempt)
            {
                report.guardrail(format!(
                    "could not persist the scheduled-task repair attempt ({error}); skipping the \
                     repair this launch. An attempt counter that cannot be written is not a cap, \
                     and repairing without one is the unbounded retry loop GRD-009 exists to stop"
                ));
                report.task_action = Some(TaskAction::Deferred { reason: error });
                return;
            }
            report.auto_heal(format!(
                "repairing scheduled task \"{}\" ({reason:?}), attempt {} of {MAX_REPAIR_ATTEMPTS}",
                spec.task_name, ledger.task_failures
            ));
            match host.register_task(spec) {
                Ok(()) => confirm_task_repair(host, spec, ledger, report),
                Err(error) => {
                    ledger.note_failure(format!("register task: {error}"));
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

/// A task state that never resolves is reported once as persistent rather than
/// producing the same line at every launch forever. An unreadable state takes
/// no action, so this is not a cap — it is the point at which "I cannot tell"
/// stops being a transient hiccup and becomes something to escalate.
fn escalate_undetermined(
    spec: &TaskSpec,
    ledger: &mut SupervisorLedger,
    report: &mut SupervisionReport,
) {
    if !ledger.undetermined_cap_spent() || ledger.undetermined_escalated {
        return;
    }
    ledger.undetermined_escalated = true;
    report.guardrail(format!(
        "the state of scheduled task \"{}\" has been unreadable for {} consecutive launches; this \
         is not transient and nothing will repair it. Edge routing stays canonical-only until a \
         human looks at Task Scheduler",
        spec.task_name, ledger.undetermined_streak
    ));
}

/// Re-query after a repair. An `Ok(())` from the effect layer is a claim, not
/// evidence — trusting it is exactly how a broken restart path logged success
/// 1,316 times while nothing recovered (GRD-009).
///
/// The attempt was already charged before `register_task` ran, so the failure
/// arms here only annotate it; charging again would halve the cap.
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
            ledger.note_failure(format!("repair could not be verified: {reason}"));
            report.guardrail(format!(
                "scheduled-task repair could not be verified ({reason}); counted as failure {} of \
                 {MAX_REPAIR_ATTEMPTS}",
                ledger.task_failures
            ));
            latch_task_give_up(spec, ledger, report);
        }
        other => {
            ledger.note_failure(format!("repair reported success but query shows {other:?}"));
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
    // Taken from the first probe only. The post-spawn confirmation below may
    // run past READINESS_DEADLINE, and this launch's routing decision is not
    // allowed to wait on it.
    report.readiness = Some(decide_readiness(&health));
    let sidecar_present = host.sidecar_exists(&spec.sidecar_exe);
    let action = decide_health_action(&health, ledger, sidecar_present, &spec.sidecar_exe);
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
        HealthAction::SkippedMissingSidecar { path } => {
            report.guardrail(format!(
                "the edge sidecar is not installed at {}; there is nothing to start. Routing stays \
                 canonical-only",
                path.display()
            ));
        }
        // Produced only by the write-ahead branch below, never by the decision.
        HealthAction::Deferred { .. } => {}
        HealthAction::GiveUp { .. } => latch_spawn_give_up(ledger, report),
        HealthAction::Spawn => {
            // Write-ahead: the attempt is durable before the effect happens.
            if let Err(error) =
                charge_and_persist(host, ledger, SupervisorLedger::charge_spawn_attempt)
            {
                report.guardrail(format!(
                    "could not persist the sidecar spawn attempt ({error}); not starting the \
                     sidecar this launch. An attempt counter that cannot be written is not a cap, \
                     and spawning without one is the unbounded retry loop GRD-009 exists to stop"
                ));
                report.health_action = Some(HealthAction::Deferred { reason: error });
                return;
            }
            report.auto_heal(format!(
                "sidecar is not running; starting it, attempt {} of {MAX_REPAIR_ATTEMPTS}",
                ledger.spawn_failures
            ));
            match host.spawn_sidecar(&spec.sidecar_exe) {
                Ok(()) => confirm_spawn(host, ledger, report),
                Err(error) => {
                    ledger.note_failure(format!("spawn sidecar: {error}"));
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

/// Re-probe after a spawn, polling until the sidecar answers healthy or the
/// confirmation window is spent. Only a healthy answer clears the cap — a
/// spawn that returns `Ok` and then dies must still count.
///
/// The polling is the point. `CreateProcess` returns before the child has
/// opened its database or bound its port, so an immediate probe gets
/// ECONNREFUSED and classifies as `NotRunning` — a *successful* launch scored
/// as a failure, three of which latch `GIVING UP` against a working spawn
/// path. The attempt was charged before the spawn, so the failure arm here
/// only annotates it.
fn confirm_spawn<H: SupervisorHost>(
    host: &H,
    ledger: &mut SupervisorLedger,
    report: &mut SupervisionReport,
) {
    let mut last = SidecarHealth::Indeterminate {
        reason: "the confirmation probe never ran".to_string(),
    };
    for attempt in 0..SPAWN_CONFIRM_ATTEMPTS {
        if attempt > 0 {
            host.wait_before_reprobe(SPAWN_CONFIRM_INTERVAL);
        }
        match host.probe_health() {
            SidecarHealth::Healthy => {
                ledger.record_sidecar_healthy();
                report.auto_heal(format!(
                    "sidecar started and answered healthy after {} probe(s)",
                    attempt + 1
                ));
                return;
            }
            other => last = other,
        }
    }
    ledger.note_failure(format!("spawn not confirmed healthy: {last:?}"));
    report.guardrail(format!(
        "sidecar spawn was not confirmed healthy within {:?} ({last:?}); {} of \
         {MAX_REPAIR_ATTEMPTS} consecutive failures",
        spawn_confirm_window(),
        ledger.spawn_failures
    ));
    latch_spawn_give_up(ledger, report);
}

/// Run `probe` on a detached worker and give up on it at `deadline`.
///
/// The readiness gate must never hold up startup. A sidecar that accepts the
/// connection and then never answers would otherwise block for as long as it
/// pleases, so the wait is bounded here rather than trusted to the caller, and
/// a blown deadline is `Indeterminate` — unknown, not dead.
///
/// See S4 in the module header: in production the caller passes the same
/// duration reqwest is already using as its own timeout, so this branch is
/// probably unreachable there and each blown deadline leaks the worker until
/// the inner timeout fires.
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
#[path = "edge_supervisor_test_host.rs"]
mod test_host;

#[cfg(test)]
#[path = "edge_supervisor_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "edge_supervisor_loop_tests.rs"]
mod loop_tests;

#[cfg(test)]
#[path = "edge_supervisor_packaging_tests.rs"]
mod packaging_tests;
