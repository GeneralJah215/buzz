//! Edge supervisor tests: decisions, health, readiness, and the ledger.
//!
//! Every test here drives the **pure** half of the supervisor through the fake
//! host in `edge_supervisor_test_host.rs`, which records calls and changes
//! nothing. No test registers a scheduled task, starts a process, sleeps, or
//! writes outside a `tempfile` directory that is dropped when the test ends.
//! `WindowsHost` is never constructed.
//!
//! The write-ahead ledger loop and the missing-sidecar rules live in
//! `edge_supervisor_loop_tests.rs`.
//!
//! What that means for confidence: the *policy* is covered exhaustively, and
//! the *effects* (schtasks argument handling, NSIS execution) are covered only
//! as far as their inputs. The parent module header lists, as S1 to S5, the
//! things that genuinely cannot be verified without registering something on a
//! real machine.

use std::time::{Duration, Instant};

use super::host::health_probe_url;
use super::test_host::{
    env_off, env_on, exe_path, registered_correctly, spec, FakeHost, EXE, VERSION,
};
use super::{
    classify_probe_failure, classify_probe_response, decide_health_action, decide_readiness,
    decide_task_repair, probe_with_deadline, supervise_launch, HealthAction, ReadinessDecision,
    RepairReason, SidecarHealth, SupervisionReport, SupervisorLedger, TaskAction, TaskRegistration,
    MAX_REPAIR_ATTEMPTS, MAX_UNDETERMINED_LAUNCHES, SPAWN_CONFIRM_ATTEMPTS, TASK_NAME,
};

/// Every way this module could write to a terminal without going through
/// `SupervisionReport`. Counted after normalising, because `eprintln!`
/// contains `println!` contains `print!` and a naive sum triple-counts.
fn count_prints(text: &str) -> usize {
    let normalized = text
        .replace("eprintln!", "@PRINT@")
        .replace("eprint!", "@PRINT@")
        .replace("println!", "@PRINT@")
        .replace("print!", "@PRINT@")
        .replace("writeln!", "@PRINT@")
        .replace("dbg!", "@PRINT@")
        .replace("tracing::", "@PRINT@")
        .replace("log::", "@PRINT@");
    normalized.matches("@PRINT@").count()
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

/// The one stderr write in the policy lives in `SupervisionReport::emit`, and
/// the one in the effect half lives in `start`'s missing-app-data-dir branch.
/// If a direct print is added anywhere else, "the feature is off ⇒ nothing is
/// logged" stops being provable from the empty log vector.
///
/// This replaces a check that counted `eprintln!` in one file and required
/// exactly 1: it ignored `host.rs`, which has its own, and any `tracing::warn!`
/// passed it. What it still cannot catch: a print reached through a helper
/// whose name is not in `count_prints`.
#[test]
fn the_supervisor_prints_only_through_the_report_and_the_launch_hook() {
    let policy = include_str!("edge_supervisor.rs");
    let (before_emit, from_emit) = policy
        .split_once("pub fn emit(&self)")
        .expect("SupervisionReport::emit");
    assert_eq!(
        count_prints(before_emit),
        0,
        "edge_supervisor.rs must not print outside SupervisionReport::emit"
    );
    assert_eq!(
        count_prints(from_emit),
        1,
        "SupervisionReport::emit must hold exactly one print"
    );

    assert_eq!(
        count_prints(include_str!("edge_supervisor/ledger.rs")),
        0,
        "the ledger must report through its returned warning, never by printing"
    );

    let host = include_str!("edge_supervisor/host.rs");
    let (before_start, from_start) = host
        .split_once("pub fn start(app_data_dir")
        .expect("windows start");
    assert_eq!(
        count_prints(before_start),
        0,
        "host.rs must not print outside the launch hook"
    );
    assert_eq!(
        count_prints(from_start),
        1,
        "the launch hook must hold exactly one print"
    );
}

/// `start` must establish that the feature is off *before* it reaches anything
/// that can log or touch the machine.
///
/// This replaces a byte-offset comparison against `eprintln!` alone, which a
/// host call, a file write, or any non-`eprintln` log added before the env
/// check passed straight through. What it still cannot catch: a side effect
/// reached through a helper whose name is not on this list.
#[test]
fn the_launch_hook_checks_the_env_var_before_anything_with_an_effect() {
    let source = include_str!("edge_supervisor/host.rs");
    let body = source
        .split("pub fn start(app_data_dir")
        .nth(1)
        .expect("windows start body");
    let (before_env, _) = body.split_once("launch_env(").expect("env check");

    for forbidden in [
        "ledger_path(",
        "WindowsHost::new",
        "thread::spawn",
        "std::fs",
        "File::",
        "Command::new",
        "schtasks",
        "supervise_launch",
    ] {
        assert!(
            !before_env.contains(forbidden),
            "start must return early on an unset BUZZ_EDGE_RELAY_URL before it reaches {forbidden}"
        );
    }
    assert_eq!(
        count_prints(before_env),
        0,
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
        decide_task_repair(&registered_correctly(), &spec(), &ledger, true),
        TaskAction::LeaveAlone
    );
}

#[test]
fn a_missing_task_is_registered() {
    assert_eq!(
        decide_task_repair(
            &TaskRegistration::Missing,
            &spec(),
            &SupervisorLedger::default(),
            true
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
        decide_task_repair(&registered_correctly(), &spec(), &ledger, true),
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
            &SupervisorLedger::default(),
            true
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
        true,
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
    assert_eq!(
        host.count_calls("register"),
        0,
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

/// A state that never resolves must not just reprint the same line at every
/// launch until the end of time — that is the 1,316-restart bug wearing
/// different clothes. It escalates once, and only once.
#[test]
fn a_permanently_unreadable_task_state_escalates_exactly_once() {
    let mut ledger = SupervisorLedger::default();
    let mut escalations = 0;

    for _ in 0..(MAX_UNDETERMINED_LAUNCHES + 4) {
        let host = FakeHost::new()
            .with_queries(vec![TaskRegistration::Unknown {
                reason: "access denied".to_string(),
            }])
            .with_ledger(ledger.clone());
        let report = supervise_launch(&host, &env_on());
        escalations += report
            .logs
            .iter()
            .filter(|log| log.message.contains("is not transient"))
            .count();
        assert_eq!(host.count_calls("register"), 0);
        ledger = host.persisted_ledger();
    }

    assert_eq!(
        escalations, 1,
        "a permanently unreadable state must escalate once, not never and not every launch"
    );
    assert!(ledger.undetermined_escalated);
}

/// A one-off unreadable query must not creep toward the escalation: the streak
/// clears as soon as the state is readable again.
#[test]
fn a_readable_task_state_clears_the_undetermined_streak() {
    let host = FakeHost::new().with_ledger(SupervisorLedger {
        undetermined_streak: MAX_UNDETERMINED_LAUNCHES - 1,
        registered_version: Some(VERSION.to_string()),
        ..SupervisorLedger::default()
    });
    supervise_launch(&host, &env_on());
    assert_eq!(host.persisted_ledger().undetermined_streak, 0);
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
        decide_task_repair(&TaskRegistration::Missing, &spec(), &ledger, true),
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
        register_calls += host.count_calls("register");
        give_up_logs += report
            .logs
            .iter()
            .filter(|log| log.message.contains("GIVING UP"))
            .count();
        ledger = host.persisted_ledger();
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
        host.count_calls("query"),
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
    assert_eq!(host.count_calls("register"), 0);
    assert!(!host.final_ledger().task_gave_up);
}

// ───────────────────────── health / respawn ─────────────────────────

#[test]
fn only_a_proven_dead_sidecar_is_respawned() {
    let ledger = SupervisorLedger::default();
    assert_eq!(
        decide_health_action(&SidecarHealth::NotRunning, &ledger, true, &exe_path()),
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
        true,
        &exe_path(),
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
        true,
        &exe_path(),
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
    assert_eq!(host.count_calls("spawn"), 0, "{:?}", host.calls());
    assert_eq!(host.final_ledger().spawn_failures, 0);
}

#[test]
fn spawn_stops_at_the_attempt_cap() {
    let ledger = SupervisorLedger {
        spawn_failures: MAX_REPAIR_ATTEMPTS,
        ..SupervisorLedger::default()
    };
    assert_eq!(
        decide_health_action(&SidecarHealth::NotRunning, &ledger, true, &exe_path()),
        HealthAction::GiveUp {
            attempts: MAX_REPAIR_ATTEMPTS
        }
    );
}

/// A spawn that returns `Ok` and then never answers, through the whole
/// confirmation window, is a failed spawn.
#[test]
fn a_spawn_that_never_becomes_healthy_counts_as_a_failure() {
    let host = FakeHost::new().with_probes(vec![SidecarHealth::NotRunning]);
    let report = supervise_launch(&host, &env_on());
    assert_eq!(host.count_calls("spawn"), 1);
    assert_eq!(
        host.count_calls("probe"),
        1 + SPAWN_CONFIRM_ATTEMPTS as usize,
        "the confirmation must use its whole window before declaring failure"
    );
    assert_eq!(host.final_ledger().spawn_failures, 1);
    assert!(report
        .logs
        .iter()
        .any(|log| log.message.contains("not confirmed healthy")));
}

/// **Defect 2.** `CreateProcess` returns before the child has opened SQLite or
/// bound its loopback port, so the first confirmation probe of a *successful*
/// spawn gets ECONNREFUSED — which classifies as `NotRunning`, not
/// `Indeterminate`. Confirming with one immediate probe therefore scored every
/// working launch as a failure, and three of them latched `GIVING UP` against
/// a spawn path that works.
///
/// The old test scripted the probe result, so it could never observe this.
#[test]
fn a_spawn_that_is_still_cold_starting_is_not_a_failure() {
    let host = FakeHost::new().with_probes(vec![
        // The pre-spawn probe: proven dead, so a spawn is authorised.
        SidecarHealth::NotRunning,
        // Confirmation, still binding its port.
        SidecarHealth::NotRunning,
        SidecarHealth::NotRunning,
        // Up and answering.
        SidecarHealth::Healthy,
    ]);
    let report = supervise_launch(&host, &env_on());

    assert_eq!(host.count_calls("spawn"), 1);
    assert_eq!(
        host.final_ledger().spawn_failures,
        0,
        "a spawn that becomes healthy is not a failure: {:?}",
        report.logs
    );
    assert!(
        !host.final_ledger().spawn_gave_up,
        "a working spawn path must never latch GIVING UP"
    );
    assert!(
        !report
            .logs
            .iter()
            .any(|log| log.message.contains("not confirmed healthy")),
        "a successful spawn must not be reported as a failure: {:?}",
        report.logs
    );
    assert!(
        report
            .logs
            .iter()
            .any(|log| log.message.contains("answered healthy")),
        "{:?}",
        report.logs
    );
    assert!(
        host.calls().iter().any(|call| call.starts_with("wait:")),
        "the confirmation must wait between probes rather than spinning: {:?}",
        host.calls()
    );
}

/// The cold-start allowance must not become an unbounded wait: three cold
/// starts in a row that never answer still latch, they just take the whole
/// window to do it.
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
        spawn_calls += host.count_calls("spawn");
        give_up_logs += report
            .logs
            .iter()
            .filter(|log| log.message.contains("GIVING UP"))
            .count();
        ledger = host.persisted_ledger();
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

/// This launch's routing decision comes from the first probe alone. The
/// post-spawn confirmation may run past `READINESS_DEADLINE`, and routing is
/// not allowed to wait on it.
#[test]
fn readiness_is_decided_before_the_spawn_confirmation_window() {
    let host = FakeHost::new().with_probes(vec![SidecarHealth::NotRunning, SidecarHealth::Healthy]);
    let report = supervise_launch(&host, &env_on());
    assert_eq!(
        report.readiness,
        Some(ReadinessDecision::CanonicalOnlyRetry {
            reason: "sidecar not running".to_string()
        })
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
    ledger.charge_task_attempt();
    ledger.note_failure("access denied".to_string());
    ledger.charge_spawn_attempt();
    ledger.note_failure("exe missing".to_string());
    ledger.charge_undetermined();
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
        let task = decide_task_repair(&TaskRegistration::Missing, &spec(), &ledger, true);
        let health =
            decide_health_action(&SidecarHealth::NotRunning, &ledger, true, &exe_path());
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

/// Charging an attempt must not latch silently: the latch is what suppresses
/// the `GIVING UP` line, so setting it outside the reporting path would hide
/// the one message the operator needs.
#[test]
fn charging_an_attempt_never_latches_give_up_on_its_own() {
    let mut ledger = SupervisorLedger::default();
    for attempt in 1..=MAX_REPAIR_ATTEMPTS + 2 {
        ledger.charge_task_attempt();
        ledger.charge_spawn_attempt();
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

/// `note_failure` annotates an attempt that was already charged. If it
/// incremented as well, every failure would count twice and the cap would bite
/// after one and a half attempts.
#[test]
fn noting_a_failure_does_not_charge_a_second_attempt() {
    let mut ledger = SupervisorLedger::default();
    ledger.charge_task_attempt();
    ledger.note_failure("register task: access denied".to_string());
    assert_eq!(ledger.task_failures, 1);
    assert_eq!(
        ledger.last_failure.as_deref(),
        Some("register task: access denied")
    );
}

#[test]
fn the_expected_exe_fixture_is_the_bundled_sidecar_name() {
    assert!(EXE.ends_with(super::SIDECAR_EXE));
}
