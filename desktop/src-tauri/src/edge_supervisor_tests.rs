//! Edge supervisor tests.
//!
//! Every test here drives the **pure** half of the supervisor through a fake
//! host that records calls and changes nothing. No test registers a scheduled
//! task, starts a process, or writes outside a `tempfile` directory that is
//! dropped when the test ends. `WindowsHost` is never constructed.
//!
//! What that means for confidence: the *policy* is covered exhaustively, and
//! the *effects* (schtasks argument handling, NSIS execution) are covered only
//! as far as their inputs. See the report accompanying this milestone for the
//! list of things that genuinely cannot be verified without registering
//! something on a real machine.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::host::health_probe_url;
use super::{
    classify_probe_failure, classify_probe_response, decide_health_action, decide_readiness,
    decide_task_repair, probe_with_deadline, supervise_launch, HealthAction, LedgerLoad,
    ReadinessDecision, RepairReason, SidecarHealth, SupervisionReport, SupervisorEnv,
    SupervisorHost, SupervisorLedger, TaskAction, TaskRegistration, TaskSpec, MAX_REPAIR_ATTEMPTS,
    TASK_NAME,
};

const EXE: &str = r"C:\Program Files\Buzz\buzz-edge.exe";
const VERSION: &str = "0.5.5";

fn exe_path() -> PathBuf {
    PathBuf::from(EXE)
}

fn spec() -> TaskSpec {
    TaskSpec {
        task_name: TASK_NAME.to_string(),
        sidecar_exe: exe_path(),
        app_version: VERSION.to_string(),
    }
}

fn env_on() -> SupervisorEnv {
    SupervisorEnv {
        edge_relay_url: Some("ws://127.0.0.1:7777".to_string()),
        sidecar_exe: exe_path(),
        app_version: VERSION.to_string(),
    }
}

fn env_off() -> SupervisorEnv {
    SupervisorEnv {
        edge_relay_url: None,
        ..env_on()
    }
}

fn registered_correctly() -> TaskRegistration {
    TaskRegistration::Registered {
        command_line: format!("\"{EXE}\""),
    }
}

// ─────────────────────────── the fake host ───────────────────────────

/// Records every call and performs none of them. `query_task` returns a
/// scripted sequence so a repair-then-verify cycle can be driven exactly.
struct FakeHost {
    queries: RefCell<Vec<TaskRegistration>>,
    probes: RefCell<Vec<SidecarHealth>>,
    register_result: Result<(), String>,
    spawn_result: Result<(), String>,
    ledger: RefCell<SupervisorLedger>,
    ledger_warning: Option<String>,
    calls: RefCell<Vec<String>>,
    stored: RefCell<Vec<SupervisorLedger>>,
}

impl FakeHost {
    fn new() -> Self {
        Self {
            queries: RefCell::new(vec![registered_correctly()]),
            probes: RefCell::new(vec![SidecarHealth::Healthy]),
            register_result: Ok(()),
            spawn_result: Ok(()),
            ledger: RefCell::new(SupervisorLedger::default()),
            ledger_warning: None,
            calls: RefCell::new(Vec::new()),
            stored: RefCell::new(Vec::new()),
        }
    }

    fn with_queries(mut self, queries: Vec<TaskRegistration>) -> Self {
        self.queries = RefCell::new(queries);
        self
    }

    fn with_probes(mut self, probes: Vec<SidecarHealth>) -> Self {
        self.probes = RefCell::new(probes);
        self
    }

    fn with_ledger(self, ledger: SupervisorLedger) -> Self {
        *self.ledger.borrow_mut() = ledger;
        self
    }

    fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }

    fn final_ledger(&self) -> SupervisorLedger {
        self.stored
            .borrow()
            .last()
            .cloned()
            .expect("a ledger should have been stored")
    }

    /// Pop the next scripted value, repeating the last one forever so a test
    /// only has to script the steps it cares about.
    fn next<T: Clone>(queue: &RefCell<Vec<T>>) -> T {
        let mut queue = queue.borrow_mut();
        if queue.len() > 1 {
            queue.remove(0)
        } else {
            queue.first().cloned().expect("scripted value")
        }
    }
}

impl SupervisorHost for FakeHost {
    fn query_task(&self, task_name: &str) -> TaskRegistration {
        self.calls.borrow_mut().push(format!("query:{task_name}"));
        Self::next(&self.queries)
    }

    fn register_task(&self, spec: &TaskSpec) -> Result<(), String> {
        self.calls
            .borrow_mut()
            .push(format!("register:{}", spec.task_name));
        self.register_result.clone()
    }

    fn probe_health(&self) -> SidecarHealth {
        self.calls.borrow_mut().push("probe".to_string());
        Self::next(&self.probes)
    }

    fn spawn_sidecar(&self, exe: &Path) -> Result<(), String> {
        self.calls
            .borrow_mut()
            .push(format!("spawn:{}", exe.display()));
        self.spawn_result.clone()
    }

    fn load_ledger(&self) -> LedgerLoad {
        self.calls.borrow_mut().push("load_ledger".to_string());
        LedgerLoad {
            ledger: self.ledger.borrow().clone(),
            warning: self.ledger_warning.clone(),
        }
    }

    fn store_ledger(&self, ledger: &SupervisorLedger) -> Result<(), String> {
        self.calls.borrow_mut().push("store_ledger".to_string());
        self.stored.borrow_mut().push(ledger.clone());
        *self.ledger.borrow_mut() = ledger.clone();
        Ok(())
    }
}

// ───────────────────── feature-off must be truly inert ─────────────────────

/// The whole feature is off unless `BUZZ_EDGE_RELAY_URL` is set. "Off" means
/// no task query, no probe, no ledger read or write, and no log line — not
/// "runs quietly". Every effect is routed through the host, so counting host
/// calls is a real measurement of that.
#[test]
fn unset_edge_url_touches_nothing_at_all() {
    let host = FakeHost::new();
    let report = supervise_launch(&host, &env_off());

    assert!(
        host.calls().is_empty(),
        "supervisor made host calls with the feature off: {:?}",
        host.calls()
    );
    assert_eq!(report, SupervisionReport::default());
    assert!(report.logs.is_empty(), "{:?}", report.logs);
    assert!(report.task_action.is_none());
    assert!(report.health_action.is_none());
    assert!(report.readiness.is_none());
}

/// Counterpart to the test above: with the feature ON the same host *is*
/// called. Without this, `unset_edge_url_touches_nothing_at_all` would still
/// pass if `supervise_launch` returned early unconditionally.
#[test]
fn set_edge_url_does_exercise_the_host() {
    let host = FakeHost::new();
    supervise_launch(&host, &env_on());
    assert!(host.calls().contains(&format!("query:{TASK_NAME}")));
    assert!(host.calls().contains(&"probe".to_string()));
}

/// The one stderr write in the module lives in `SupervisionReport::emit`. If a
/// direct print is added anywhere else, "the feature is off ⇒ nothing is
/// logged" stops being provable from the empty log vector.
#[test]
fn the_module_prints_only_through_the_report() {
    let source = include_str!("edge_supervisor.rs");
    assert_eq!(
        source.matches("eprintln!").count(),
        1,
        "edge_supervisor.rs must print only via SupervisionReport::emit"
    );
}

/// `start` must establish that the feature is off *before* it reaches
/// anything that can print. The one stderr write outside
/// `SupervisionReport::emit` is in `start`'s missing-app-data-dir branch; if
/// that branch ever moved above the env check, a machine with the feature off
/// would get a log line it should never see.
#[test]
fn the_launch_hook_checks_the_env_var_before_anything_that_can_log() {
    let source = include_str!("edge_supervisor/host.rs");
    let body = source
        .split("pub fn start(app_data_dir")
        .nth(1)
        .expect("windows start body");
    let env_check = body.find("launch_env(").expect("env check");
    let first_print = body.find("eprintln!").expect("guarded print");
    assert!(
        env_check < first_print,
        "start must return early on an unset BUZZ_EDGE_RELAY_URL before it can log"
    );
}

// ─────────────────────── task repair decisions ───────────────────────

#[test]
fn a_matching_registration_is_left_alone() {
    let ledger = SupervisorLedger {
        registered_version: Some(VERSION.to_string()),
        ..SupervisorLedger::default()
    };
    assert_eq!(
        decide_task_repair(&registered_correctly(), &spec(), &ledger),
        TaskAction::LeaveAlone
    );
}

#[test]
fn a_missing_task_is_registered() {
    assert_eq!(
        decide_task_repair(
            &TaskRegistration::Missing,
            &spec(),
            &SupervisorLedger::default()
        ),
        TaskAction::Register {
            reason: RepairReason::Missing
        }
    );
}

/// The upgrade case. The install path does not change between Buzz versions,
/// so a path comparison alone would never notice an upgrade; the confirmed
/// version in the ledger is what makes "re-register after upgrade" real.
#[test]
fn a_task_registered_by_an_older_version_is_re_registered() {
    let ledger = SupervisorLedger {
        registered_version: Some("0.5.4".to_string()),
        ..SupervisorLedger::default()
    };
    assert_eq!(
        decide_task_repair(&registered_correctly(), &spec(), &ledger),
        TaskAction::Register {
            reason: RepairReason::VersionChanged
        }
    );
}

/// A task that has never been confirmed by this supervisor (no recorded
/// version) is re-registered once, so a task registered by some other tool is
/// brought onto the known-good definition rather than trusted.
#[test]
fn a_task_with_no_confirmed_version_is_re_registered() {
    assert_eq!(
        decide_task_repair(
            &registered_correctly(),
            &spec(),
            &SupervisorLedger::default()
        ),
        TaskAction::Register {
            reason: RepairReason::VersionChanged
        }
    );
}

/// "I cannot tell" is its own outcome. Reading it as `Missing` is the
/// GRD-010 defect that turned 22 healthy agents into 1,316 restarts.
#[test]
fn an_undeterminable_task_state_is_never_repaired() {
    let action = decide_task_repair(
        &TaskRegistration::Unknown {
            reason: "schtasks listing failed".to_string(),
        },
        &spec(),
        &SupervisorLedger::default(),
    );
    match action {
        TaskAction::Undetermined { reason } => assert!(reason.contains("schtasks")),
        other => panic!("unknown task state must not be repaired, got {other:?}"),
    }
}

#[test]
fn an_undeterminable_task_state_makes_no_registration_call() {
    let host = FakeHost::new().with_queries(vec![TaskRegistration::Unknown {
        reason: "access denied".to_string(),
    }]);
    let report = supervise_launch(&host, &env_on());
    assert!(
        !host.calls().iter().any(|call| call.starts_with("register")),
        "unknown state must not trigger a registration: {:?}",
        host.calls()
    );
    assert!(
        report
            .logs
            .iter()
            .any(|log| log.message.contains("could not be determined")),
        "the unknown state must be reported, not silently passed: {:?}",
        report.logs
    );
}

// ────────────────────── the 1,316-restart guardrail ──────────────────────

/// GRD-009. Three consecutive failures and the supervisor stops.
#[test]
fn task_repair_stops_at_the_attempt_cap() {
    let ledger = SupervisorLedger {
        task_failures: MAX_REPAIR_ATTEMPTS,
        ..SupervisorLedger::default()
    };
    assert_eq!(
        decide_task_repair(&TaskRegistration::Missing, &spec(), &ledger),
        TaskAction::GiveUp {
            attempts: MAX_REPAIR_ATTEMPTS
        }
    );
}

/// The failure counter must survive restarts, so drive the whole cap through
/// repeated `supervise_launch` calls carrying the ledger forward — the shape a
/// user restarting Buzz all day actually produces.
#[test]
fn repeated_launches_stop_registering_after_three_failures() {
    let mut ledger = SupervisorLedger::default();
    let mut register_calls = 0;
    let mut give_up_logs = 0;

    for _ in 0..8 {
        let mut host = FakeHost::new().with_queries(vec![TaskRegistration::Missing]);
        host.register_result = Err("access denied".to_string());
        let host = host.with_ledger(ledger.clone());
        let report = supervise_launch(&host, &env_on());
        register_calls += host
            .calls()
            .iter()
            .filter(|call| call.starts_with("register"))
            .count();
        give_up_logs += report
            .logs
            .iter()
            .filter(|log| log.message.contains("GIVING UP"))
            .count();
        ledger = host.final_ledger();
    }

    assert_eq!(
        register_calls, MAX_REPAIR_ATTEMPTS as usize,
        "the supervisor must not keep re-registering a task that keeps failing"
    );
    assert_eq!(
        give_up_logs, 1,
        "GIVING UP must be said exactly once, not on every launch"
    );
    assert!(ledger.task_gave_up);
}

/// The precise defect behind the 1,316 attempts: the effect layer reported
/// success and nothing had recovered. An `Ok(())` from `register_task` is a
/// claim; only a re-query is evidence.
#[test]
fn a_repair_that_claims_success_but_changes_nothing_counts_as_a_failure() {
    let host = FakeHost::new()
        // First query: missing. Second (the verification): still missing.
        .with_queries(vec![TaskRegistration::Missing, TaskRegistration::Missing]);
    let report = supervise_launch(&host, &env_on());

    assert_eq!(
        host.calls()
            .iter()
            .filter(|call| call.starts_with("query"))
            .count(),
        2,
        "a repair must be verified by a second query"
    );
    assert_eq!(host.final_ledger().task_failures, 1);
    assert!(
        report
            .logs
            .iter()
            .any(|log| log.message.contains("still wrong")),
        "{:?}",
        report.logs
    );
}

/// A verification that cannot be read is not a pass either — it consumes an
/// attempt, so a permanently unreadable task cannot be retried forever.
#[test]
fn a_repair_that_cannot_be_verified_counts_as_a_failure() {
    let host = FakeHost::new().with_queries(vec![
        TaskRegistration::Missing,
        TaskRegistration::Unknown {
            reason: "query timed out".to_string(),
        },
    ]);
    supervise_launch(&host, &env_on());
    assert_eq!(host.final_ledger().task_failures, 1);
    assert!(host.final_ledger().registered_version.is_none());
}

/// GRD-009's other half: the cap clears the moment the thing is healthy
/// again, so a genuine one-off still self-heals instead of latching forever.
#[test]
fn a_confirmed_repair_clears_a_latched_give_up() {
    let host = FakeHost::new()
        .with_queries(vec![TaskRegistration::Missing, registered_correctly()])
        .with_ledger(SupervisorLedger {
            task_failures: 2,
            ..SupervisorLedger::default()
        });
    supervise_launch(&host, &env_on());
    let ledger = host.final_ledger();
    assert_eq!(ledger.task_failures, 0);
    assert!(!ledger.task_gave_up);
    assert_eq!(ledger.registered_version.as_deref(), Some(VERSION));
}

#[test]
fn a_healthy_task_clears_a_latched_give_up_without_registering() {
    let host = FakeHost::new()
        .with_queries(vec![registered_correctly()])
        .with_ledger(SupervisorLedger {
            task_failures: MAX_REPAIR_ATTEMPTS,
            task_gave_up: true,
            registered_version: Some(VERSION.to_string()),
            ..SupervisorLedger::default()
        });
    supervise_launch(&host, &env_on());
    assert!(!host.calls().iter().any(|call| call.starts_with("register")));
    assert!(!host.final_ledger().task_gave_up);
}

// ───────────────────────── health / respawn ─────────────────────────

#[test]
fn only_a_proven_dead_sidecar_is_respawned() {
    let ledger = SupervisorLedger::default();
    assert_eq!(
        decide_health_action(&SidecarHealth::NotRunning, &ledger),
        HealthAction::Spawn
    );
}

/// Running-but-unhealthy is not dead. Spawning a second copy onto the same
/// loopback port makes a bad state worse, so it is reported instead.
#[test]
fn a_running_but_unhealthy_sidecar_is_never_respawned() {
    let action = decide_health_action(
        &SidecarHealth::RunningUnhealthy {
            reason: "database locked".to_string(),
        },
        &SupervisorLedger::default(),
    );
    match action {
        HealthAction::ReportUnhealthy { reason } => assert_eq!(reason, "database locked"),
        other => panic!("unhealthy must not respawn, got {other:?}"),
    }
}

/// The GRD-010 rule, restated for the process: unknown is not dead.
#[test]
fn an_indeterminate_health_result_is_never_respawned() {
    let action = decide_health_action(
        &SidecarHealth::Indeterminate {
            reason: "probe timed out".to_string(),
        },
        &SupervisorLedger::default(),
    );
    match action {
        HealthAction::Observe { reason } => assert_eq!(reason, "probe timed out"),
        other => panic!("indeterminate must not respawn, got {other:?}"),
    }
}

#[test]
fn indeterminate_health_makes_no_spawn_call() {
    let host = FakeHost::new().with_probes(vec![SidecarHealth::Indeterminate {
        reason: "connection reset".to_string(),
    }]);
    supervise_launch(&host, &env_on());
    assert!(
        !host.calls().iter().any(|call| call.starts_with("spawn")),
        "{:?}",
        host.calls()
    );
    assert_eq!(host.final_ledger().spawn_failures, 0);
}

#[test]
fn spawn_stops_at_the_attempt_cap() {
    let ledger = SupervisorLedger {
        spawn_failures: MAX_REPAIR_ATTEMPTS,
        ..SupervisorLedger::default()
    };
    assert_eq!(
        decide_health_action(&SidecarHealth::NotRunning, &ledger),
        HealthAction::GiveUp {
            attempts: MAX_REPAIR_ATTEMPTS
        }
    );
}

/// A spawn that returns `Ok` and then does not answer is a failed spawn.
#[test]
fn a_spawn_that_does_not_become_healthy_counts_as_a_failure() {
    let host = FakeHost::new().with_probes(vec![SidecarHealth::NotRunning]);
    let report = supervise_launch(&host, &env_on());
    assert!(host.calls().iter().any(|call| call.starts_with("spawn")));
    assert_eq!(host.final_ledger().spawn_failures, 1);
    assert!(report
        .logs
        .iter()
        .any(|log| log.message.contains("not confirmed healthy")));
}

#[test]
fn repeated_launches_stop_spawning_after_three_failures() {
    let mut ledger = SupervisorLedger::default();
    let mut spawn_calls = 0;
    let mut give_up_logs = 0;

    for _ in 0..8 {
        let host = FakeHost::new()
            .with_probes(vec![SidecarHealth::NotRunning])
            .with_ledger(ledger.clone());
        let report = supervise_launch(&host, &env_on());
        spawn_calls += host
            .calls()
            .iter()
            .filter(|call| call.starts_with("spawn"))
            .count();
        give_up_logs += report
            .logs
            .iter()
            .filter(|log| log.message.contains("GIVING UP"))
            .count();
        ledger = host.final_ledger();
    }

    assert_eq!(spawn_calls, MAX_REPAIR_ATTEMPTS as usize);
    assert_eq!(give_up_logs, 1);
    assert!(ledger.spawn_gave_up);
}

#[test]
fn a_healthy_sidecar_clears_a_latched_spawn_give_up() {
    let host = FakeHost::new().with_ledger(SupervisorLedger {
        spawn_failures: MAX_REPAIR_ATTEMPTS,
        spawn_gave_up: true,
        ..SupervisorLedger::default()
    });
    supervise_launch(&host, &env_on());
    let ledger = host.final_ledger();
    assert_eq!(ledger.spawn_failures, 0);
    assert!(!ledger.spawn_gave_up);
}

// ───────────────────────── readiness gate ─────────────────────────

#[test]
fn only_a_healthy_sidecar_enables_edge_routing() {
    assert_eq!(
        decide_readiness(&SidecarHealth::Healthy),
        ReadinessDecision::EdgeReady
    );
    for degraded in [
        SidecarHealth::NotRunning,
        SidecarHealth::RunningUnhealthy {
            reason: "x".to_string(),
        },
        SidecarHealth::Indeterminate {
            reason: "x".to_string(),
        },
    ] {
        assert!(
            matches!(
                decide_readiness(&degraded),
                ReadinessDecision::CanonicalOnlyRetry { .. }
            ),
            "{degraded:?} must degrade to canonical-only"
        );
    }
}

/// The gate must be a ceiling on waiting, not a hope. A probe that never
/// answers has to return control at the deadline, because the alternative is
/// an app that will not start.
#[test]
fn the_readiness_gate_returns_at_the_deadline_when_the_probe_hangs() {
    let started = Instant::now();
    let health = probe_with_deadline(
        || {
            std::thread::sleep(Duration::from_secs(30));
            SidecarHealth::Healthy
        },
        Duration::from_millis(50),
    );
    let elapsed = started.elapsed();

    assert!(
        matches!(health, SidecarHealth::Indeterminate { .. }),
        "a blown deadline is unknown, not dead: {health:?}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "the readiness gate blocked for {elapsed:?}"
    );
    assert_eq!(
        decide_readiness(&health),
        ReadinessDecision::CanonicalOnlyRetry {
            reason: match &health {
                SidecarHealth::Indeterminate { reason } => reason.clone(),
                other => panic!("{other:?}"),
            }
        }
    );
}

#[test]
fn the_readiness_gate_returns_a_fast_probe_unchanged() {
    assert_eq!(
        probe_with_deadline(|| SidecarHealth::Healthy, Duration::from_secs(5)),
        SidecarHealth::Healthy
    );
}

// ───────────────────────── probe classification ─────────────────────────

/// Only a refused connection proves nothing is listening. This is the exact
/// line between "dead" and "I cannot tell".
#[test]
fn probe_failures_are_classified_without_guessing() {
    assert_eq!(
        classify_probe_failure(true, false),
        SidecarHealth::NotRunning
    );
    assert!(matches!(
        classify_probe_failure(false, true),
        SidecarHealth::Indeterminate { .. }
    ));
    assert!(
        matches!(
            classify_probe_failure(false, false),
            SidecarHealth::Indeterminate { .. }
        ),
        "an unclassified transport error must never be read as a dead sidecar"
    );
}

#[test]
fn probe_responses_are_classified_by_content() {
    assert_eq!(
        classify_probe_response(200, r#"{"ok":true}"#),
        SidecarHealth::Healthy
    );
    assert!(matches!(
        classify_probe_response(200, r#"{"ok":false,"reason":"outbox stalled"}"#),
        SidecarHealth::RunningUnhealthy { .. }
    ));
    assert!(matches!(
        classify_probe_response(503, "{}"),
        SidecarHealth::RunningUnhealthy { .. }
    ));
    // Answering in an unexpected shape means we cannot tell, which must not
    // be rounded up to healthy or down to dead.
    assert!(matches!(
        classify_probe_response(200, r#"{"status":"fine"}"#),
        SidecarHealth::Indeterminate { .. }
    ));
    assert!(matches!(
        classify_probe_response(200, "<html>proxy</html>"),
        SidecarHealth::Indeterminate { .. }
    ));
}

#[test]
fn the_health_probe_url_is_loopback_only() {
    assert_eq!(
        health_probe_url("ws://127.0.0.1:7777").as_deref(),
        Some("http://127.0.0.1:7777/health")
    );
    assert_eq!(
        health_probe_url("http://localhost:7777/").as_deref(),
        Some("http://localhost:7777/health")
    );
    for rejected in [
        "ws://example.com:7777",
        "wss://127.0.0.1:7777",
        "https://127.0.0.1:7777",
        "ws://8.8.8.8:7777",
        "not a url",
        "",
    ] {
        assert!(
            health_probe_url(rejected).is_none(),
            "{rejected} must not become a health probe target"
        );
    }
}

// ───────────────────────── ledger persistence ─────────────────────────

/// The cap has to survive a restart: an in-memory counter makes "3 attempts
/// per launch, forever" — still an unbounded loop over a day of restarts.
#[test]
fn the_ledger_round_trips_through_a_file() {
    let directory = tempfile::tempdir().expect("temp dir");
    let path = directory.path().join("edge-supervisor.json");

    let mut ledger = SupervisorLedger::default();
    ledger.record_task_failure("access denied".to_string());
    ledger.record_spawn_failure("exe missing".to_string());
    ledger.store_to(&path).expect("store");

    let loaded = SupervisorLedger::load_from(&path);
    assert_eq!(loaded.ledger, ledger);
    assert!(loaded.warning.is_none());
}

#[test]
fn a_missing_ledger_is_a_fresh_install_not_a_warning() {
    let directory = tempfile::tempdir().expect("temp dir");
    let loaded = SupervisorLedger::load_from(&directory.path().join("absent.json"));
    assert_eq!(loaded.ledger, SupervisorLedger::default());
    assert!(loaded.warning.is_none());
}

/// A corrupt ledger resets the counters, which quietly re-opens the retry
/// loop. It is bounded per launch, but it must never be silent.
#[test]
fn a_corrupt_ledger_is_reported_not_swallowed() {
    let directory = tempfile::tempdir().expect("temp dir");
    let path = directory.path().join("edge-supervisor.json");
    std::fs::write(&path, b"{ this is not json").expect("write");

    let loaded = SupervisorLedger::load_from(&path);
    assert_eq!(loaded.ledger, SupervisorLedger::default());
    let warning = loaded.warning.expect("corruption must be reported");
    assert!(warning.contains("corrupt"), "{warning}");
}

#[test]
fn a_ledger_warning_reaches_the_report() {
    let mut host = FakeHost::new();
    host.ledger_warning = Some("ledger corrupt".to_string());
    let report = supervise_launch(&host, &env_on());
    assert!(report
        .logs
        .iter()
        .any(|log| log.message.contains("ledger corrupt")));
}

/// Walk the decision table at 0, 1, 2, 3 and 4 prior failures, the way GRD-009
/// was verified. The cap must bite at exactly three.
#[test]
fn the_cap_bites_at_exactly_three_prior_failures() {
    for prior in 0..=4u32 {
        let ledger = SupervisorLedger {
            task_failures: prior,
            spawn_failures: prior,
            ..SupervisorLedger::default()
        };
        let task = decide_task_repair(&TaskRegistration::Missing, &spec(), &ledger);
        let health = decide_health_action(&SidecarHealth::NotRunning, &ledger);
        if prior < MAX_REPAIR_ATTEMPTS {
            assert!(
                matches!(task, TaskAction::Register { .. }),
                "{prior} prior failures should still repair, got {task:?}"
            );
            assert_eq!(health, HealthAction::Spawn, "{prior} prior failures");
        } else {
            assert!(
                matches!(task, TaskAction::GiveUp { .. }),
                "{prior} prior failures must stop, got {task:?}"
            );
            assert!(
                matches!(health, HealthAction::GiveUp { .. }),
                "{prior} prior failures must stop, got {health:?}"
            );
        }
    }
}

/// The failure recorder must not latch silently: the latch is what suppresses
/// the `GIVING UP` line, so setting it outside the reporting path would hide
/// the one message the operator needs.
#[test]
fn recording_a_failure_never_latches_give_up_on_its_own() {
    let mut ledger = SupervisorLedger::default();
    for attempt in 1..=MAX_REPAIR_ATTEMPTS + 2 {
        ledger.record_task_failure(format!("attempt {attempt}"));
        ledger.record_spawn_failure(format!("attempt {attempt}"));
        assert!(!ledger.task_gave_up, "latched silently at {attempt}");
        assert!(!ledger.spawn_gave_up, "latched silently at {attempt}");
    }
    assert!(ledger.task_cap_spent());
    assert!(ledger.spawn_cap_spent());

    ledger.record_task_success(VERSION);
    ledger.record_sidecar_healthy();
    assert_eq!(ledger.task_failures, 0);
    assert_eq!(ledger.spawn_failures, 0);
    assert!(!ledger.task_cap_spent());
    assert!(!ledger.spawn_cap_spent());
}
