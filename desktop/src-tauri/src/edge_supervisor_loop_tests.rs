//! The two ways the attempt cap could evaporate, driven end to end.
//!
//! 1. **The ledger cannot be written.** The cap is entirely cross-launch and
//!    entirely dependent on that write. If the effect runs first and the write
//!    is only attempted afterwards, a read-only app data directory (or
//!    antivirus, or a sync client, or a crash) turns a bounded three-attempt
//!    repair into an unbounded one: every launch loads a fresh ledger, acts,
//!    fails to persist, and discards the evidence. `GIVING UP` is never
//!    reached because `task_cap_spent()` is never true.
//! 2. **The sidecar binary is not there.** `schtasks /Create /XML` does not
//!    check the `<Command>` path, so nothing but this policy stops the
//!    supervisor from creating exactly the orphaned logon task the installer
//!    deletes on purpose.
//!
//! Neither had a test before, and neither *could*: the old fake host hardcoded
//! `store_ledger` to `Ok(())` with no override, and nothing asked whether the
//! binary existed.

use super::test_host::{env_on, exe_path, spec, FakeHost, VERSION};
use super::{
    decide_health_action, decide_task_repair, supervise_launch, HealthAction, SidecarHealth,
    SupervisorLedger, TaskAction, TaskRegistration, MAX_REPAIR_ATTEMPTS,
};

fn store_always_fails() -> Vec<Result<(), String>> {
    vec![Err("app data directory is read-only".to_string())]
}

// ───────── defect 3: the cap must not evaporate with the ledger ─────────

/// The whole loop, run the way a user restarting Buzz all day produces it. If
/// the attempt were charged *after* the effect, this would register eight
/// times and never say `GIVING UP`.
#[test]
fn a_ledger_that_cannot_be_written_stops_the_repair_instead_of_unbounding_it() {
    let mut register_calls = 0;
    let mut reported = 0;

    for _ in 0..8 {
        let host = FakeHost::new()
            .with_queries(vec![TaskRegistration::Missing])
            .with_store_results(store_always_fails());
        let report = supervise_launch(&host, &env_on());
        register_calls += host.count_calls("register");
        reported += report
            .logs
            .iter()
            .filter(|log| log.message.contains("skipping the repair this launch"))
            .count();
    }

    assert_eq!(
        register_calls, 0,
        "an attempt that cannot be recorded must not be made: an unrecordable counter is not a cap"
    );
    assert_eq!(
        reported, 8,
        "every skipped repair must be reported, never silently passed"
    );
}

/// Same rule on the spawn side.
#[test]
fn a_ledger_that_cannot_be_written_stops_the_spawn_instead_of_unbounding_it() {
    let mut spawn_calls = 0;
    let mut reported = 0;

    for _ in 0..8 {
        let host = FakeHost::new()
            .with_probes(vec![SidecarHealth::NotRunning])
            .with_store_results(store_always_fails());
        let report = supervise_launch(&host, &env_on());
        spawn_calls += host.count_calls("spawn");
        reported += report
            .logs
            .iter()
            .filter(|log| log.message.contains("not starting the sidecar this launch"))
            .count();
    }

    assert_eq!(spawn_calls, 0, "a spawn attempt that cannot be recorded must not be made");
    assert_eq!(reported, 8);
}

/// The report must name what happened rather than claiming a repair was made.
#[test]
fn a_deferred_repair_is_reported_as_deferred_not_as_a_repair() {
    let host = FakeHost::new()
        .with_queries(vec![TaskRegistration::Missing])
        .with_store_results(store_always_fails());
    let report = supervise_launch(&host, &env_on());
    assert!(
        matches!(report.task_action, Some(TaskAction::Deferred { .. })),
        "{:?}",
        report.task_action
    );
}

/// Write-ahead is the point, not "write twice". The attempt has to be on disk
/// *before* the effect runs, so a crash between the two still costs an
/// attempt.
#[test]
fn the_attempt_is_persisted_before_the_effect_runs() {
    let host = FakeHost::new().with_queries(vec![TaskRegistration::Missing]);
    supervise_launch(&host, &env_on());

    let calls = host.calls();
    let first_store = calls
        .iter()
        .position(|call| call == "store_ledger")
        .expect("the attempt must be stored");
    let register = calls
        .iter()
        .position(|call| call.starts_with("register"))
        .expect("the repair must be attempted");
    assert!(
        first_store < register,
        "the attempt must be durable before the effect: {calls:?}"
    );
}

#[test]
fn the_spawn_attempt_is_persisted_before_the_process_starts() {
    let host = FakeHost::new().with_probes(vec![SidecarHealth::NotRunning]);
    supervise_launch(&host, &env_on());

    let calls = host.calls();
    let first_store = calls
        .iter()
        .position(|call| call == "store_ledger")
        .expect("the attempt must be stored");
    let spawn = calls
        .iter()
        .position(|call| call.starts_with("spawn"))
        .expect("the spawn must be attempted");
    assert!(
        first_store < spawn,
        "the attempt must be durable before the effect: {calls:?}"
    );
}

/// A ledger whose *outcome* write fails still bounds the loop, because the
/// attempt itself was already persisted. Losing the outcome can only leave a
/// counter reading high, which stops sooner — never later.
///
/// Known consequence, asserted rather than hidden: the `task_gave_up` latch
/// also lives in the outcome write, so when that write is lost the `GIVING UP`
/// line repeats at every launch instead of appearing once. That is noise, not
/// the GRD-009 failure — no repair is attempted on any of those launches.
#[test]
fn losing_the_outcome_write_still_bounds_the_loop() {
    let mut ledger = SupervisorLedger::default();
    let mut register_calls = 0;
    let mut give_up_logs = 0;

    for launch in 0..8 {
        let lost = || Err("held by another process".to_string());
        // Under the cap a launch writes twice: the write-ahead charge, then
        // the outcome. Past it there is no effect, so only the outcome.
        let stores = if launch < MAX_REPAIR_ATTEMPTS as usize {
            vec![Ok(()), lost()]
        } else {
            vec![lost()]
        };
        let mut host = FakeHost::new()
            .with_queries(vec![TaskRegistration::Missing])
            .with_store_results(stores);
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
        "the charged attempts must still cap the loop when the outcome write is lost"
    );
    assert_eq!(
        ledger.task_failures, MAX_REPAIR_ATTEMPTS,
        "the charges are what survive, and they are what the cap reads"
    );
    assert!(
        give_up_logs >= 1,
        "reaching the cap must be reported at least once"
    );
}

/// A successful launch whose outcome write is lost must not drift upward into
/// a false `GIVING UP`: the next launch sees a correct task, clears, and the
/// counter goes nowhere.
#[test]
fn a_working_task_never_latches_give_up_when_the_outcome_write_is_lost() {
    let mut ledger = SupervisorLedger::default();

    for _ in 0..8 {
        let host = FakeHost::new()
            .with_ledger(ledger.clone())
            .with_store_results(vec![Err("held by another process".to_string())]);
        let report = supervise_launch(&host, &env_on());
        assert!(
            !report
                .logs
                .iter()
                .any(|log| log.message.contains("GIVING UP")),
            "a healthy machine must never be told the supervisor gave up: {:?}",
            report.logs
        );
        ledger = host.persisted_ledger();
    }

    assert_eq!(ledger.task_failures, 0);
    assert_eq!(ledger.spawn_failures, 0);
}

// ───────── defect 4: never register a task for a binary that is not there ─────────

/// `schtasks /Create /XML` accepts any `<Command>` string. Registering a logon
/// task for a file that does not exist produces a task that fails at every
/// logon forever — the exact orphan `NSIS_HOOK_POSTINSTALL` deletes rather
/// than create.
#[test]
fn a_missing_sidecar_binary_is_never_registered() {
    assert_eq!(
        decide_task_repair(
            &TaskRegistration::Missing,
            &spec(),
            &SupervisorLedger::default(),
            false,
        ),
        TaskAction::SkippedMissingSidecar {
            path: exe_path()
        }
    );
}

#[test]
fn a_missing_sidecar_binary_blocks_every_repair_reason() {
    let ledger = SupervisorLedger {
        registered_version: Some("0.5.4".to_string()),
        ..SupervisorLedger::default()
    };
    for registration in [
        TaskRegistration::Missing,
        TaskRegistration::Registered {
            command_line: r"C:\Temp\other.exe".to_string(),
        },
        TaskRegistration::Registered {
            command_line: format!("\"{}\"", exe_path().display()),
        },
    ] {
        assert!(
            matches!(
                decide_task_repair(&registration, &spec(), &ledger, false),
                TaskAction::SkippedMissingSidecar { .. }
            ),
            "{registration:?} must not be repaired when the binary is absent"
        );
    }
}

#[test]
fn a_missing_sidecar_binary_makes_no_registration_call() {
    let host = FakeHost::new()
        .with_queries(vec![TaskRegistration::Missing])
        .with_missing_sidecar();
    let report = supervise_launch(&host, &env_on());

    assert_eq!(
        host.count_calls("register"),
        0,
        "the supervisor must not create the orphan the installer refuses to create: {:?}",
        host.calls()
    );
    assert!(
        report
            .logs
            .iter()
            .any(|log| log.message.contains("refusing to register")),
        "the skip must be reported, not silent: {:?}",
        report.logs
    );
}

/// Nothing to start is not a spawn failure. Counting it would latch
/// `GIVING UP` after three launches of a build that simply ships no sidecar,
/// and the latch would still be there after the sidecar was installed.
#[test]
fn a_missing_sidecar_binary_is_never_spawned_and_never_counted() {
    assert_eq!(
        decide_health_action(
            &SidecarHealth::NotRunning,
            &SupervisorLedger::default(),
            false,
            &exe_path(),
        ),
        HealthAction::SkippedMissingSidecar { path: exe_path() }
    );

    let mut ledger = SupervisorLedger::default();
    for _ in 0..8 {
        let host = FakeHost::new()
            .with_probes(vec![SidecarHealth::NotRunning])
            .with_missing_sidecar()
            .with_ledger(ledger.clone());
        let report = supervise_launch(&host, &env_on());
        assert_eq!(host.count_calls("spawn"), 0);
        assert!(!report
            .logs
            .iter()
            .any(|log| log.message.contains("GIVING UP")));
        ledger = host.persisted_ledger();
    }
    assert_eq!(
        ledger.spawn_failures, 0,
        "an absent binary is a packaging fact, not a run of spawn failures"
    );
}

/// The present-binary path must still work, or the two tests above would pass
/// on a supervisor that never registers anything at all.
#[test]
fn a_present_sidecar_binary_is_still_registered() {
    let host = FakeHost::new().with_queries(vec![TaskRegistration::Missing]);
    supervise_launch(&host, &env_on());
    assert_eq!(host.count_calls("register"), 1, "{:?}", host.calls());
    assert_eq!(
        host.persisted_ledger().registered_version.as_deref(),
        None,
        "the verification query still decides success"
    );

    let host = FakeHost::new().with_queries(vec![
        TaskRegistration::Missing,
        TaskRegistration::Registered {
            command_line: format!("\"{}\"", exe_path().display()),
        },
    ]);
    supervise_launch(&host, &env_on());
    assert_eq!(
        host.persisted_ledger().registered_version.as_deref(),
        Some(VERSION)
    );
}
