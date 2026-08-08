//! Packaging-side supervisor tests: command quoting, user scope, and the
//! agreement between `windows/edge-task.nsi`, `tauri.conf.json`, and the
//! supervisor's own repair path (spec acceptance item 17).
//!
//! Nothing here registers, starts, or installs anything. The NSIS hook is
//! asserted by reading its source, which is as far as it can be checked
//! without running an installer on a real machine.

use std::path::{Path, PathBuf};

use super::host::{task_definition_xml, RESTART_COUNT, RESTART_INTERVAL};
use super::{
    command_line_matches, decide_task_repair, quoted_task_command, RepairReason, SupervisorLedger,
    TaskAction, TaskRegistration, TaskSpec, TASK_NAME,
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

#[test]
fn the_task_command_is_quoted() {
    assert_eq!(
        quoted_task_command(&exe_path()).expect("quoting"),
        format!("\"{EXE}\"")
    );
}

#[test]
fn a_path_containing_a_quote_is_refused_rather_than_mangled() {
    assert!(quoted_task_command(Path::new(r#"C:\we"ird\buzz-edge.exe"#)).is_err());
}

/// The registration must not be considered correct when the stored action is
/// unquoted and contains a space: Windows would run `C:\Program.exe`, so that
/// registration is broken and has to be repaired.
#[test]
fn an_unquoted_path_with_a_space_does_not_count_as_registered() {
    assert!(
        !command_line_matches(EXE, &exe_path()),
        "an unquoted Program Files path must be treated as a broken registration"
    );
    assert_eq!(
        decide_task_repair(
            &TaskRegistration::Registered {
                command_line: EXE.to_string()
            },
            &spec(),
            &SupervisorLedger {
                registered_version: Some(VERSION.to_string()),
                ..SupervisorLedger::default()
            }
        ),
        TaskAction::Register {
            reason: RepairReason::CommandMismatch
        }
    );
}

#[test]
fn command_line_matching_is_case_and_separator_insensitive() {
    assert!(command_line_matches(
        r#""c:/program files/buzz/BUZZ-EDGE.EXE""#,
        &exe_path()
    ));
    assert!(command_line_matches(&format!("  \"{EXE}\"  "), &exe_path()));
}

#[test]
fn a_command_line_pointing_somewhere_else_does_not_match() {
    for other in [
        r#""C:\Program Files\Buzz\buzz.exe""#,
        r#""C:\Temp\buzz-edge.exe""#,
        r#""C:\Program Files\Buzz\buzz-edge.exe"#, // unterminated quote
        "",
    ] {
        assert!(
            !command_line_matches(other, &exe_path()),
            "{other} must not match the expected sidecar path"
        );
    }
}

/// The "user scope" half of acceptance item 17, asserted against the code that
/// actually registers the task.
///
/// The supervisor repairs via `schtasks /Create /XML`, so the scope lives in
/// the XML principal, not in command-line flags. What must never appear is a
/// flag that moves the task off the invoking user (`/RU`, `/RP`) or elevates
/// it (`/RL HIGHEST`) — those would silently defeat the XML principal.
#[test]
fn the_registration_call_never_names_another_account_or_elevates() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/edge_supervisor/host.rs");
    let source = std::fs::read_to_string(&path).expect("read host.rs");
    assert!(
        source.contains("\"/XML\".into()"),
        "registration must go through the XML definition, which carries the principal"
    );
    for forbidden in ["\"/RU\"", "\"/RP\"", "HIGHEST", "HighestAvailable"] {
        assert!(
            !source.contains(forbidden),
            "host.rs must never pass {forbidden} when registering the task"
        );
    }
}

#[test]
fn the_task_xml_quotes_the_command_and_stays_unelevated() {
    let xml = task_definition_xml(&spec()).expect("xml");
    assert!(
        xml.contains(&format!("<Command>\"{EXE}\"</Command>")),
        "the XML Command must be quoted: {xml}"
    );
    assert!(xml.contains("<RunLevel>LeastPrivilege</RunLevel>"));
    assert!(xml.contains("<LogonType>InteractiveToken</LogonType>"));
    assert!(xml.contains("<LogonTrigger>"));
    assert!(
        !xml.contains("<UserId>"),
        "naming a user would break the installing-user scope"
    );
    assert!(!xml.contains("HighestAvailable"));
    assert!(xml.contains(&format!("<Interval>{RESTART_INTERVAL}</Interval>")));
    assert!(xml.contains(&format!("<Count>{RESTART_COUNT}</Count>")));
}

// ─────────────── the installer hook and the supervisor must agree ───────────────

fn nsi_source() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("windows/edge-task.nsi");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

/// If the installer registers "Buzz Edge Sidecar" and the supervisor looks for
/// anything else, the supervisor "repairs" a missing task on every launch
/// forever — until the cap stops it and reports a failure that is not real.
#[test]
fn the_installer_and_the_supervisor_use_the_same_task_name() {
    assert!(
        nsi_source().contains(&format!("!define BUZZ_EDGE_TASK_NAME \"{TASK_NAME}\"")),
        "edge-task.nsi must define the task name as {TASK_NAME}"
    );
}

/// The installer registers the same definition the supervisor would repair
/// with. Drift here means a fight between the two at every launch.
#[test]
fn the_installer_registers_the_same_definition_the_supervisor_repairs_with() {
    let nsi = nsi_source();
    for required in [
        "<Command>\"$INSTDIR\\${BUZZ_EDGE_EXE}\"</Command>",
        "<RunLevel>LeastPrivilege</RunLevel>",
        "<LogonType>InteractiveToken</LogonType>",
        "<LogonTrigger>",
        "<Interval>PT1M</Interval>",
        "<Count>3</Count>",
    ] {
        assert!(
            nsi.contains(required),
            "edge-task.nsi is missing {required}"
        );
    }
    assert!(
        !nsi.contains("<UserId>"),
        "edge-task.nsi must not name a user account"
    );
    assert!(
        !nsi.contains("HighestAvailable") && !nsi.contains("/RL HIGHEST"),
        "the logon task must never be elevated"
    );
}

/// An orphaned logon task that starts a deleted binary is a bad thing to leave
/// on someone's machine, so both uninstall hooks must delete it.
#[test]
fn uninstall_removes_the_task() {
    let nsi = nsi_source();
    assert!(nsi.contains("!macro NSIS_HOOK_PREUNINSTALL"));
    assert!(nsi.contains("!macro NSIS_HOOK_POSTUNINSTALL"));
    assert!(nsi.contains("/Delete /TN \"${BUZZ_EDGE_TASK_NAME}\" /F"));

    let pre = nsi
        .split("!macro NSIS_HOOK_PREUNINSTALL")
        .nth(1)
        .and_then(|rest| rest.split("!macroend").next())
        .expect("pre-uninstall hook body");
    assert!(pre.contains("BuzzEdgeDeleteTask"), "{pre}");
    assert!(
        pre.contains("BuzzEdgeStopSidecar"),
        "uninstall must stop the running sidecar too: {pre}"
    );

    let post = nsi
        .split("!macro NSIS_HOOK_POSTUNINSTALL")
        .nth(1)
        .and_then(|rest| rest.split("!macroend").next())
        .expect("post-uninstall hook body");
    assert!(post.contains("BuzzEdgeDeleteTask"), "{post}");
}

/// The upgrade path: the pre-install hook must stop the running sidecar, or
/// Windows refuses to overwrite the open image; the post-install hook must
/// re-register with `/F` so the task points at the new binary.
#[test]
fn upgrade_stops_the_sidecar_and_re_registers_the_task() {
    let nsi = nsi_source();
    let pre = nsi
        .split("!macro NSIS_HOOK_PREINSTALL")
        .nth(1)
        .and_then(|rest| rest.split("!macroend").next())
        .expect("pre-install hook body");
    assert!(pre.contains("BuzzEdgeStopSidecar"), "{pre}");

    let post = nsi
        .split("!macro NSIS_HOOK_POSTINSTALL")
        .nth(1)
        .and_then(|rest| rest.split("!macroend").next())
        .expect("post-install hook body");
    assert!(
        post.contains("/Create /TN \"${BUZZ_EDGE_TASK_NAME}\""),
        "{post}"
    );
    assert!(
        post.contains("/F"),
        "re-registration must overwrite: {post}"
    );
}

/// The hook is only reachable if `tauri.conf.json` points at it.
#[test]
fn tauri_config_wires_the_installer_hook() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tauri.conf.json");
    let config: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("read tauri.conf.json"))
            .expect("parse tauri.conf.json");
    assert_eq!(
        config["bundle"]["windows"]["nsis"]["installerHooks"].as_str(),
        Some("windows/edge-task.nsi")
    );
}
