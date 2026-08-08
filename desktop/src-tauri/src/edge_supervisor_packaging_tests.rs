//! Packaging-side supervisor tests: command quoting, XML escaping, user scope,
//! and the agreement between `windows/edge-task.nsi`, `tauri.conf.json`, and
//! the supervisor's own repair path (spec acceptance item 17).
//!
//! Nothing here registers, starts, or installs anything.
//!
//! Where a property can only be checked against source text, the check is made
//! specific enough to fail the mutations that matter, and the comment says what
//! it still cannot catch. The task-definition drift check is not a source-text
//! check at all any more: it rebuilds the XML the installer writes from the
//! `FileWriteUTF16LE` lines and compares it line for line against
//! `task_definition_xml`'s actual output. The previous version compared six
//! substrings out of roughly twenty elements and stayed green through a
//! complete absence of XML escaping in both writers.

use std::path::{Path, PathBuf};

use super::host::{
    escape_xml_text, extract_xml_element, query_definition_args, register_task_args,
    task_definition_xml, unescape_xml_text, RESTART_COUNT, RESTART_INTERVAL,
};
use super::{
    command_line_matches, decide_task_repair, quoted_task_command, RepairReason, SupervisorLedger,
    TaskAction, TaskRegistration, TaskSpec, SIDECAR_EXE, TASK_NAME,
};

const EXE: &str = r"C:\Program Files\Buzz\buzz-edge.exe";
const VERSION: &str = "0.5.5";

/// A path that is entirely legal on Windows and entirely illegal in raw XML.
/// With Tauri's default `currentUser` install mode `$INSTDIR` sits under
/// `%LOCALAPPDATA%`, which contains the Windows user name.
const AMPERSAND_EXE: &str = r"C:\Users\A & B\Buzz\buzz-edge.exe";

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
            },
            true,
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

// ───────────────────────────── XML escaping ─────────────────────────────

#[test]
fn xml_escaping_covers_every_character_that_breaks_a_document() {
    assert_eq!(
        escape_xml_text(r#"a & b < c > d " e"#),
        "a &amp; b &lt; c &gt; d &quot; e"
    );
    // The ampersand must be replaced first, or the replacements' own
    // ampersands are escaped a second time.
    assert_eq!(escape_xml_text("&lt;"), "&amp;lt;");
    assert_eq!(unescape_xml_text("&amp;lt;"), "&lt;");
    assert_eq!(
        unescape_xml_text(&escape_xml_text(r#"a & b < c > d " e"#)),
        r#"a & b < c > d " e"#
    );
}

/// **Defect 6.** A user called `A & B` produces an install path that is legal
/// on Windows and malformed as raw XML. Interpolated unescaped, `schtasks
/// /Create /XML` fails, the installer's only signal is a `DetailPrint` nobody
/// sees under `/S`, and the supervisor then generates the same malformed XML
/// at every launch until it latches `GIVING UP` against a machine where
/// nothing is wrong.
#[test]
fn a_path_containing_an_ampersand_produces_well_formed_xml() {
    let xml = task_definition_xml(&TaskSpec {
        task_name: TASK_NAME.to_string(),
        sidecar_exe: PathBuf::from(AMPERSAND_EXE),
        app_version: "1.0 <beta> & final".to_string(),
    })
    .expect("xml");

    // A bare "&" anywhere in the document is the defect itself: it is the one
    // character that cannot appear unescaped in XML content.
    for (index, _) in xml.match_indices('&') {
        let tail = &xml[index..];
        assert!(
            ["&amp;", "&lt;", "&gt;", "&quot;", "&apos;"]
                .iter()
                .any(|entity| tail.starts_with(entity)),
            "unescaped '&' at byte {index} in:\n{xml}"
        );
    }
    assert!(
        xml.contains(r"C:\Users\A &amp; B\Buzz\buzz-edge.exe"),
        "{xml}"
    );
    assert!(
        xml.contains("app version 1.0 &lt;beta&gt; &amp; final"),
        "the version is interpolated too, and it is not guaranteed to be a semver: {xml}"
    );
}

/// Escaping only the write side would turn a hard failure into a permanent
/// one: Task Scheduler re-emits `&` as `&amp;`, a raw read never equals the
/// expected path, and the supervisor "repairs" a correct task at every launch.
#[test]
fn an_escaped_command_survives_the_read_back_round_trip() {
    let path = PathBuf::from(AMPERSAND_EXE);
    let xml = task_definition_xml(&TaskSpec {
        task_name: TASK_NAME.to_string(),
        sidecar_exe: path.clone(),
        app_version: VERSION.to_string(),
    })
    .expect("xml");

    let read_back = extract_xml_element(&xml, "Command").expect("Command element");
    assert_eq!(read_back, format!("\"{AMPERSAND_EXE}\""));
    assert!(
        command_line_matches(&read_back, &path),
        "a task registered for {AMPERSAND_EXE} must read back as already correct, or the \
         supervisor repairs it forever: {read_back}"
    );
    assert_eq!(
        decide_task_repair(
            &TaskRegistration::Registered {
                command_line: read_back
            },
            &TaskSpec {
                task_name: TASK_NAME.to_string(),
                sidecar_exe: path,
                app_version: VERSION.to_string(),
            },
            &SupervisorLedger {
                registered_version: Some(VERSION.to_string()),
                ..SupervisorLedger::default()
            },
            true,
        ),
        TaskAction::LeaveAlone
    );
}

/// The other half of the read-back path: the element reader must unescape, and
/// it must not stop at the first `<`.
#[test]
fn the_element_reader_unescapes_what_it_reads() {
    assert_eq!(
        extract_xml_element("<Command>&quot;C:\\a &amp; b\\x.exe&quot;</Command>", "Command")
            .as_deref(),
        Some("\"C:\\a & b\\x.exe\"")
    );
    assert_eq!(extract_xml_element("<Other>x</Other>", "Command"), None);
    assert_eq!(extract_xml_element("<Command>x", "Command"), None);
}

// ───────────────────── the registration call itself ─────────────────────

/// The "user scope" half of acceptance item 17, asserted against the argument
/// vector that is actually passed to `schtasks` rather than against the text of
/// `host.rs`. The previous version asserted `source.contains("\"/XML\".into()")`,
/// which passes even when the vector is built and never used.
///
/// What this still cannot prove: that `register_task` passes the returned
/// vector to the process rather than a different one. The single-occurrence
/// check below is what closes that: `"/Create"` appears in exactly one place in
/// the file, so there is no second argument vector to pass instead.
#[test]
fn the_registration_call_never_names_another_account_or_elevates() {
    let args = register_task_args(TASK_NAME, r"C:\Temp\buzz-edge-task.xml");
    assert_eq!(
        args,
        vec![
            "/Create",
            "/TN",
            TASK_NAME,
            "/XML",
            r"C:\Temp\buzz-edge-task.xml",
            "/F",
        ]
    );
    for forbidden in ["/RU", "/RP", "/RL", "HIGHEST", "HighestAvailable"] {
        assert!(
            !args.iter().any(|arg| arg.eq_ignore_ascii_case(forbidden)),
            "registering the task must never pass {forbidden}"
        );
    }

    let source = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src/edge_supervisor/host.rs"),
    )
    .expect("read host.rs");
    assert_eq!(
        source.matches("\"/Create\"").count(),
        1,
        "there must be exactly one place that builds the create arguments, and it is the one \
         asserted above"
    );
    assert!(
        source.contains("register_task_args(") && source.matches("register_task_args(").count() >= 2,
        "register_task must call register_task_args rather than build its own vector"
    );
    for forbidden in ["\"/RU\"", "\"/RP\"", "\"/RL\"", "HIGHEST", "HighestAvailable"] {
        assert!(
            !source.contains(forbidden),
            "host.rs must never pass {forbidden} when registering the task"
        );
    }
}

#[test]
fn the_definition_query_reads_one_task_as_xml() {
    assert_eq!(
        query_definition_args(TASK_NAME),
        vec!["/Query", "/TN", TASK_NAME, "/XML", "ONE"]
    );
}

#[test]
fn the_task_xml_quotes_the_command_and_stays_unelevated() {
    let xml = task_definition_xml(&spec()).expect("xml");
    assert!(
        xml.contains(&format!("<Command>&quot;{EXE}&quot;</Command>")),
        "the XML Command must be quoted (as escaped entities, which parse to the same \
         characters): {xml}"
    );
    let quoted = format!("\"{EXE}\"");
    assert_eq!(
        extract_xml_element(&xml, "Command").as_deref(),
        Some(quoted.as_str()),
        "and it must read back as a quoted path"
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

/// Split a command line into arguments, dropping the NSIS quoting. A flag is
/// then a whole token, so `/F` and `/FO` are different things — the check that
/// `post.contains("/F")` was satisfied by `/FO` is what this exists to stop.
fn command_tokens(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(|token| token.trim_matches(['\'', '"']).to_string())
        .collect()
}

fn nsi_macro_body(nsi: &str, name: &str) -> String {
    nsi.split(&format!("!macro {name}"))
        .nth(1)
        .and_then(|rest| rest.split("!macroend").next())
        .unwrap_or_else(|| panic!("{name} body"))
        .to_string()
}

/// Rebuild the XML document `BuzzEdgeWriteTaskXml` writes, with the escaped
/// runtime values substituted back in.
fn nsi_task_xml(command_path: &str, working_directory: &str, version: &str) -> Vec<String> {
    nsi_macro_body(&nsi_source(), "BuzzEdgeWriteTaskXml")
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("FileWriteUTF16LE"))
        .map(|line| {
            let start = line.find('\'').expect("opening quote") + 1;
            let end = line.rfind('\'').expect("closing quote");
            let payload = &line[start..end];
            payload
                .strip_suffix(r"$\r$\n")
                .unwrap_or(payload)
                // The installer escapes these at run time and writes them
                // through registers; substitute the escaped values back so the
                // document can be compared with the supervisor's.
                .replace("$R8", command_path)
                .replace("$R7", working_directory)
                .replace("$R6", version)
        })
        .collect()
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
/// with — all of it, not six substrings of it. Drift means a fight between the
/// two at every launch.
///
/// Forward slashes in the placeholder path so `Path::parent` behaves the same
/// on every platform this crate is compiled for; the separator is irrelevant to
/// what is being compared.
#[test]
fn the_installer_registers_the_same_definition_the_supervisor_repairs_with() {
    const DIR: &str = "NSIS_INSTDIR";
    const APP_VERSION: &str = "NSIS_VERSION";

    let supervisor = task_definition_xml(&TaskSpec {
        task_name: TASK_NAME.to_string(),
        sidecar_exe: PathBuf::from(format!("{DIR}/{SIDECAR_EXE}")),
        app_version: APP_VERSION.to_string(),
    })
    .expect("xml");
    let supervisor: Vec<String> = supervisor
        .lines()
        .map(|line| line.trim_end_matches('\r').to_string())
        .filter(|line| !line.is_empty())
        .collect();

    let installer = nsi_task_xml(&format!("{DIR}/{SIDECAR_EXE}"), DIR, APP_VERSION);

    assert!(
        supervisor.len() > 20,
        "the comparison must cover the whole document, not a fragment: {supervisor:?}"
    );
    assert_eq!(
        installer, supervisor,
        "edge-task.nsi and host.rs::task_definition_xml have drifted"
    );
}

/// The installer must escape the values it interpolates, exactly as the
/// supervisor does. Asserted against the source because there is no way to run
/// NSIS here; specific enough that removing the escape call, or interpolating
/// `$INSTDIR` straight into an element, fails it.
///
/// What this cannot catch: an escape routine that is called but wrong. The
/// Rust half of the same routine is tested for real above.
#[test]
fn the_installer_escapes_every_value_it_interpolates() {
    let body = nsi_macro_body(&nsi_source(), "BuzzEdgeWriteTaskXml");
    for required in [
        "!insertmacro BuzzEdgeXmlEscape $R8",
        "!insertmacro BuzzEdgeXmlEscape $R7",
        "!insertmacro BuzzEdgeXmlEscape $R6",
    ] {
        assert!(body.contains(required), "edge-task.nsi is missing {required}");
    }
    for line in body.lines().filter(|line| line.contains("FileWriteUTF16LE")) {
        assert!(
            !line.contains("$INSTDIR") && !line.contains("${VERSION}"),
            "raw values must never reach the document; escape them first: {line}"
        );
    }
    let escape = nsi_macro_body(&nsi_source(), "BuzzEdgeXmlEscape");
    for entity in ["&amp;", "&lt;", "&gt;", "&quot;"] {
        assert!(escape.contains(entity), "the escape macro is missing {entity}");
    }
}

/// A failed `FileOpen` leaves an empty handle and every `FileWriteUTF16LE`
/// then silently no-ops. Registering from the resulting file and blaming the
/// schtasks exit code sends whoever reads the log to the wrong place.
#[test]
fn the_installer_checks_that_the_task_definition_was_written() {
    let write = nsi_macro_body(&nsi_source(), "BuzzEdgeWriteTaskXml");
    assert!(
        write.contains("FileOpen $9") && write.contains("StrCmp $9 \"\""),
        "the FileOpen result must be checked: {write}"
    );

    let post = nsi_macro_body(&nsi_source(), "NSIS_HOOK_POSTINSTALL");
    let register = post
        .lines()
        .position(|line| line.contains("/Create"))
        .expect("the create call");
    let guard = post
        .lines()
        .position(|line| line.contains("StrCmp $R5"))
        .expect("the write-succeeded guard");
    assert!(
        guard < register,
        "the create call must be guarded by the write result: {post}"
    );
}

/// NSIS registers are global and this file is spliced into Tauri's generated
/// script, which uses `$0`-`$9` and `$R0`-`$R9` itself. Every register a hook
/// touches — including inside the macros it inserts — must be saved on entry
/// and restored, in reverse order, on every exit path.
///
/// What this cannot catch: a register written by something other than the
/// statements `used_registers` knows about, and an exit path that leaves the
/// macro before reaching the trailing pops. Only makensis or a real install
/// would settle either.
#[test]
fn every_hook_saves_and_restores_the_registers_it_uses() {
    let nsi = nsi_source();
    for hook in [
        "NSIS_HOOK_PREINSTALL",
        "NSIS_HOOK_POSTINSTALL",
        "NSIS_HOOK_PREUNINSTALL",
        "NSIS_HOOK_POSTUNINSTALL",
    ] {
        let body = nsi_macro_body(&nsi, hook);
        let pushes = leading(&body, "Push ");
        let mut expected_pops = pushes.clone();
        expected_pops.reverse();
        assert!(!pushes.is_empty(), "{hook} saves nothing: {body}");
        assert_eq!(
            trailing(&body, "Pop "),
            expected_pops,
            "{hook} must restore its registers in reverse order"
        );

        for register in used_registers(&expand_inserted_macros(&nsi, &body)) {
            assert!(
                pushes.contains(&register),
                "{hook} clobbers {register} without saving it"
            );
        }
    }
}

/// The unbroken run of `Push $x` at the top of a macro body.
fn leading(body: &str, keyword: &str) -> Vec<String> {
    body.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take_while(|line| line.starts_with(keyword))
        .map(|line| line[keyword.len()..].trim().to_string())
        .collect()
}

/// The unbroken run of `Pop $x` at the bottom of a macro body.
fn trailing(body: &str, keyword: &str) -> Vec<String> {
    let mut found: Vec<String> = body
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .rev()
        .take_while(|line| line.starts_with(keyword))
        .map(|line| line[keyword.len()..].trim().to_string())
        .collect();
    found.reverse();
    found
}

/// One level of macro expansion is enough: the hooks insert leaf macros only.
fn expand_inserted_macros(nsi: &str, body: &str) -> String {
    let mut expanded = body.to_string();
    for line in body.lines() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix("!insertmacro ") else {
            continue;
        };
        let name = rest.split_whitespace().next().unwrap_or_default();
        expanded.push_str(&nsi_macro_body(nsi, name));
        // The escape macro is inserted by the XML writer, one level deeper.
        if name == "BuzzEdgeWriteTaskXml" {
            expanded.push_str(&nsi_macro_body(nsi, "BuzzEdgeXmlEscape"));
        }
    }
    expanded
}

/// Every `$0`-`$9` / `$R0`-`$R9` the body writes to. Only the destination of a
/// write matters: reading a register the surrounding script owns is fine, and
/// `${OUT}`-style macro parameters are not registers at all.
fn used_registers(body: &str) -> Vec<String> {
    let mut found: Vec<String> = body
        .lines()
        .map(str::trim)
        .filter_map(|line| {
            let mut tokens = line.split_whitespace();
            let keyword = tokens.next()?;
            if !["StrCpy", "Pop", "IntOp", "ReadEnvStr", "FileOpen"].contains(&keyword) {
                return None;
            }
            let target = tokens.next()?;
            let name = target.strip_prefix('$')?;
            let digits = name.strip_prefix('R').unwrap_or(name);
            (digits.len() == 1 && digits.chars().all(|c| c.is_ascii_digit()))
                .then(|| target.to_string())
        })
        .collect();
    found.sort();
    found.dedup();
    found
}

/// An orphaned logon task that starts a deleted binary is a bad thing to leave
/// on someone's machine, and so is a sidecar left running with an open
/// database after its files have been removed. Both uninstall hooks must do
/// both.
#[test]
fn uninstall_removes_the_task_and_stops_the_process() {
    let nsi = nsi_source();
    assert!(nsi.contains("!macro NSIS_HOOK_PREUNINSTALL"));
    assert!(nsi.contains("!macro NSIS_HOOK_POSTUNINSTALL"));

    for hook in ["NSIS_HOOK_PREUNINSTALL", "NSIS_HOOK_POSTUNINSTALL"] {
        let body = nsi_macro_body(&nsi, hook);
        assert!(body.contains("BuzzEdgeDeleteTask"), "{hook}: {body}");
        assert!(
            body.contains("BuzzEdgeStopSidecar"),
            "{hook} must stop the running sidecar too, or the documented fallback path leaves one \
             running with an open database: {body}"
        );
    }

    // Tokenised, not `contains("/F")`: the old check was satisfied by `/FO`.
    let delete = nsi_macro_body(&nsi, "BuzzEdgeDeleteTask");
    let tokens = command_tokens(&delete);
    assert!(tokens.iter().any(|t| t == "/Delete"), "{delete}");
    assert!(
        tokens.iter().any(|t| t == "/F"),
        "the delete must be forced: {delete}"
    );
}

/// The uninstall kill must be scoped to one user, must ask before it forces,
/// and must wait before files are removed. An unfiltered `taskkill /IM ... /F`
/// under an elevated uninstall kills every logged-on user's sidecar, and any
/// unrelated process sharing the image name, mid-write.
#[test]
fn the_uninstall_kill_is_scoped_graceful_and_waits() {
    let stop = nsi_macro_body(&nsi_source(), "BuzzEdgeStopSidecar");
    let kills: Vec<&str> = stop
        .lines()
        .map(str::trim)
        .filter(|line| line.contains("taskkill.exe"))
        .collect();
    assert!(!kills.is_empty(), "{stop}");

    for kill in &kills {
        assert!(
            kill.contains("/FI \"USERNAME eq"),
            "every taskkill must be scoped to one user: {kill}"
        );
        assert!(
            kill.contains("/T"),
            "every taskkill must include child processes: {kill}"
        );
    }
    assert!(
        kills.iter().any(|kill| !kill.contains("/F ")),
        "the sidecar must be asked to close before it is forced: {stop}"
    );
    assert!(
        kills.iter().any(|kill| kill.contains("/F ")),
        "and forced if it does not: {stop}"
    );

    let graceful = stop
        .lines()
        .position(|line| line.contains("taskkill.exe") && !line.contains("/F "))
        .expect("graceful kill");
    let forced = stop
        .lines()
        .position(|line| line.contains("taskkill.exe") && line.contains("/F "))
        .expect("forced kill");
    let wait = stop
        .lines()
        .position(|line| line.trim().starts_with("Sleep "))
        .expect("a wait between them");
    assert!(
        graceful < wait && wait < forced,
        "the grace period must sit between asking and forcing: {stop}"
    );
    assert!(
        stop.matches("Sleep ").count() >= 2,
        "and a second wait must let handles close before files are removed: {stop}"
    );
}

/// The upgrade path: the pre-install hook must stop the running sidecar, or
/// Windows refuses to overwrite the open image; the post-install hook must
/// re-register with `/F` so the task points at the new binary.
#[test]
fn upgrade_stops_the_sidecar_and_re_registers_the_task() {
    let nsi = nsi_source();
    let pre = nsi_macro_body(&nsi, "NSIS_HOOK_PREINSTALL");
    assert!(pre.contains("BuzzEdgeStopSidecar"), "{pre}");

    let post = nsi_macro_body(&nsi, "NSIS_HOOK_POSTINSTALL");
    let create = post
        .lines()
        .find(|line| line.contains("/Create"))
        .expect("the create call");
    let tokens = command_tokens(create);
    for required in ["/Create", "/TN", "/XML"] {
        assert!(tokens.iter().any(|t| t == required), "{create}");
    }
    // Tokenised, not `contains("/F")`: the old check was satisfied by `/FO`.
    assert!(
        tokens.iter().any(|t| t == "/F"),
        "re-registration must overwrite with a standalone /F: {create}"
    );
    for forbidden in ["/RU", "/RP", "/RL"] {
        assert!(
            !tokens.iter().any(|t| t == forbidden),
            "the installer must not move or elevate the task: {create}"
        );
    }
}

// ─────────────────────────── tauri.conf.json ───────────────────────────

fn tauri_config() -> serde_json::Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tauri.conf.json");
    serde_json::from_str(&std::fs::read_to_string(&path).expect("read tauri.conf.json"))
        .expect("parse tauri.conf.json")
}

/// The hook is only reachable if `tauri.conf.json` points at it.
#[test]
fn tauri_config_wires_the_installer_hook() {
    assert_eq!(
        tauri_config()["bundle"]["windows"]["nsis"]["installerHooks"].as_str(),
        Some("windows/edge-task.nsi")
    );
}

/// **Defect 5.** The other half of acceptance item 17, and the half that had
/// no test: the bundle must actually contain the sidecar. Without the
/// `externalBin` entry Tauri does not bundle `buzz-edge`, the installer's
/// `IfFileExists "$INSTDIR\buzz-edge.exe"` is false in every shipped build,
/// every installer takes the skip-and-delete branch, and the whole
/// registration and repair path is unreachable code.
#[test]
fn tauri_config_bundles_the_sidecar() {
    let config = tauri_config();
    let binaries: Vec<&str> = config["bundle"]["externalBin"]
        .as_array()
        .expect("externalBin")
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect();
    assert!(
        binaries.contains(&"binaries/buzz-edge"),
        "the sidecar must be bundled or the installer hook can never fire: {binaries:?}"
    );

    // Tauri appends the target triple on disk and strips it on install, so the
    // installed name is the entry's basename plus the platform extension. That
    // installed name is what edge-task.nsi and SIDECAR_EXE both look for.
    assert_eq!(
        SIDECAR_EXE, "buzz-edge.exe",
        "the installed sidecar name must match the externalBin basename"
    );
    assert!(nsi_source().contains("!define BUZZ_EDGE_EXE \"buzz-edge.exe\""));
}

/// An `externalBin` entry with nothing staging the binary is a bundle-step
/// failure, not a feature. The two have to move together.
#[test]
fn the_bundling_script_stages_the_sidecar() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../scripts/bundle-sidecars.sh")
        .canonicalize()
        .expect("locate bundle-sidecars.sh");
    let script = std::fs::read_to_string(&path).expect("read bundle-sidecars.sh");
    let sidecars = script
        .lines()
        .find(|line| line.starts_with("SIDECARS=("))
        .expect("the SIDECARS list");
    assert!(
        sidecars.split(|c: char| c == '(' || c == ')' || c == ' ')
            .any(|name| name == "buzz-edge"),
        "bundle-sidecars.sh must stage buzz-edge for the externalBin entry: {sidecars}"
    );
}
