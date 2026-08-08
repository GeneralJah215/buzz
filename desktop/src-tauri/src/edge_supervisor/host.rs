//! The effect half of the edge supervisor: the real Windows host and the
//! launch entry point.
//!
//! Everything that can change the machine lives here — querying Task
//! Scheduler, registering the task, starting the sidecar, writing the ledger.
//! The policy that decides *whether* to do any of it lives in the parent
//! module and is tested without this file ever being constructed.
//!
//! The pure helpers at the top of this file are the exceptions: XML escaping,
//! the task definition, the `schtasks` argument vectors, the element reader,
//! and the probe URL are all pure and all tested. The XML must stay in
//! lockstep with what `windows/edge-task.nsi` writes at install time — if a
//! repair used a plain `schtasks /Create /TR`, it would silently drop the
//! installer's restart-on-failure policy, so both paths register the same
//! definition, and a test compares them line by line.

use std::path::PathBuf;

use crate::edge_supervisor::{quoted_task_command, TaskSpec, LEDGER_FILE};

/// Restart-on-failure policy from the spec's packaging note: three restarts at
/// one-minute intervals per failure window.
pub const RESTART_COUNT: u32 = 3;
pub const RESTART_INTERVAL: &str = "PT1M";

/// Escape text for XML element content.
///
/// `&` is a legal character in a Windows path, and with Tauri's default
/// `currentUser` install mode `$INSTDIR` sits under `%LOCALAPPDATA%`, which
/// contains the Windows user name. A user called `Tom & Jerry` produces
/// `C:\Users\Tom & Jerry\...`; interpolated raw, that is malformed XML,
/// `schtasks /Create /XML` fails, and the supervisor then generates the same
/// malformed XML at every launch until the cap latches `GIVING UP` against a
/// machine where nothing is actually wrong.
///
/// `&` must be replaced first or the replacement's own ampersands get escaped
/// a second time.
pub fn escape_xml_text(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Inverse of [`escape_xml_text`], for text read back out of Task Scheduler's
/// XML.
///
/// Without this, escaping the write side alone turns a hard failure into a
/// worse one: Task Scheduler stores `C:\Users\Tom & Jerry\...` and re-emits it
/// as `&amp;`, the raw read-back never equals the expected path,
/// `command_line_matches` reports a mismatch, and the supervisor re-registers
/// a perfectly good task on every launch until it gives up.
///
/// `&amp;` must be replaced last, for the same reason it is replaced first
/// above.
pub fn unescape_xml_text(value: &str) -> String {
    value
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// Read one element's text out of a task XML document, unescaped.
///
/// Deliberately not an XML parser: this reads a single well-known element out
/// of `schtasks /Query /XML` output. It is pure and tested so the read-back
/// half of the escaping round trip is covered.
pub fn extract_xml_element(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(unescape_xml_text(xml[start..end].trim()))
}

/// The Task Scheduler definition for the logon task.
///
/// * `<Command>` is **quoted** (acceptance item 17) — this path contains
///   `Program Files`, and an unquoted one is the classic unquoted-path
///   privilege problem. The quotes are then XML-escaped like everything else;
///   `&quot;` and `"` are the same character to any XML parser.
/// * `<RunLevel>LeastPrivilege</RunLevel>` on an interactive token is the
///   **user scope** of acceptance item 17: no elevation, ever.
/// * `<LogonTrigger>` carries no `<UserId>`, so the task binds to the account
///   that registers it — the installing user.
///
/// See S1 and S2 in the parent module header for what about this definition
/// is not verified.
pub fn task_definition_xml(spec: &TaskSpec) -> Result<String, String> {
    let command = escape_xml_text(&quoted_task_command(&spec.sidecar_exe)?);
    let working_directory = escape_xml_text(
        &spec
            .sidecar_exe
            .parent()
            .map(|parent| parent.display().to_string())
            .unwrap_or_default(),
    );
    let version = escape_xml_text(&spec.app_version);
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Starts the Buzz edge sidecar at logon (app version {version}).</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>false</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <RestartOnFailure>
      <Interval>{interval}</Interval>
      <Count>{count}</Count>
    </RestartOnFailure>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{command}</Command>
      <WorkingDirectory>{working_directory}</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
"#,
        interval = RESTART_INTERVAL,
        count = RESTART_COUNT,
    ))
}

/// The `schtasks` arguments that register the task.
///
/// A value rather than a string literal buried in `register_task`, so a test
/// can assert the actual argument vector instead of grepping this file — a
/// source-text check passes even when the vector is built and thrown away.
///
/// `/XML` carries the principal, so the scope lives in the definition rather
/// than in flags; `/F` overwrites, which is the upgrade path. `/RU`, `/RP` and
/// `/RL` are never passed — any of them would move the task off the invoking
/// user or elevate it, silently defeating the XML principal.
pub fn register_task_args(task_name: &str, xml_path: &str) -> Vec<String> {
    vec![
        "/Create".to_string(),
        "/TN".to_string(),
        task_name.to_string(),
        "/XML".to_string(),
        xml_path.to_string(),
        "/F".to_string(),
    ]
}

/// The locale-independent existence query: list every task as CSV with no
/// header. See S3 in the parent module header for what substring matching on
/// this listing cannot distinguish.
pub fn query_listing_args() -> Vec<String> {
    vec![
        "/Query".to_string(),
        "/FO".to_string(),
        "CSV".to_string(),
        "/NH".to_string(),
    ]
}

/// The follow-up query that reads one task's definition back.
pub fn query_definition_args(task_name: &str) -> Vec<String> {
    vec![
        "/Query".to_string(),
        "/TN".to_string(),
        task_name.to_string(),
        "/XML".to_string(),
        "ONE".to_string(),
    ]
}

/// Turn the configured edge URL into the health-probe URL.
///
/// Loopback-only, in the same spirit as `relay::edge::resolve_edge_endpoint`:
/// a supervisor that probes a remote host is a supervisor pointed at the wrong
/// thing, and it would treat that host's answers as this machine's health.
pub fn health_probe_url(edge_relay_url: &str) -> Option<String> {
    let mut parsed = url::Url::parse(edge_relay_url).ok()?;
    let loopback = match parsed.host() {
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        Some(url::Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        None => false,
    };
    if !matches!(parsed.scheme(), "ws" | "http") || !loopback {
        return None;
    }
    parsed.set_scheme("http").ok()?;
    parsed.set_path("/health");
    parsed.set_query(None);
    parsed.set_fragment(None);
    Some(parsed.to_string())
}

/// Where the persisted failure ledger lives. `None` when there is no app data
/// directory, in which case supervision is skipped rather than run with a cap
/// that cannot survive a restart.
pub fn ledger_path(app_data_dir: Option<PathBuf>) -> Option<PathBuf> {
    Some(app_data_dir?.join(LEDGER_FILE))
}

/// The single call site in `lib.rs`.
///
/// Returns immediately on every platform and in every configuration. With
/// `BUZZ_EDGE_RELAY_URL` unset it does nothing at all; with it set, all
/// supervision happens on a detached worker so the readiness probe can never
/// delay app startup.
pub fn start_for_app(app: &tauri::AppHandle) {
    use tauri::Manager as _;

    let app_data_dir = app.path().app_data_dir().ok();
    let version = app.package_info().version.to_string();
    start(app_data_dir, &version);
}

/// Non-Windows builds have no Task Scheduler, so supervision is a no-op there.
/// Kept as a real symbol so the `lib.rs` wiring is platform-independent.
#[cfg(not(windows))]
pub fn start(_app_data_dir: Option<PathBuf>, _app_version: &str) {}

#[cfg(windows)]
pub use windows_host::start;

#[cfg(windows)]
mod windows_host {
    use std::os::windows::process::CommandExt as _;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::Duration;

    use crate::edge_supervisor::host::{
        extract_xml_element, health_probe_url, ledger_path, query_definition_args,
        query_listing_args, register_task_args, task_definition_xml,
    };
    use crate::edge_supervisor::{
        classify_probe_failure, classify_probe_response, launch_env, probe_with_deadline,
        supervise_launch, LedgerLoad, SidecarHealth, SupervisorHost, SupervisorLedger,
        TaskRegistration, TaskSpec, READINESS_DEADLINE,
    };

    /// Keeps `schtasks.exe` and the sidecar from flashing a console window
    /// over whatever the operator is doing.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    pub struct WindowsHost {
        ledger_path: PathBuf,
        probe_url: Option<String>,
    }

    impl WindowsHost {
        pub fn new(ledger_path: PathBuf, edge_relay_url: &str) -> Self {
            Self {
                ledger_path,
                probe_url: health_probe_url(edge_relay_url),
            }
        }

        fn schtasks(args: &[String]) -> Result<(bool, String), String> {
            let output = Command::new("schtasks.exe")
                .args(args)
                .creation_flags(CREATE_NO_WINDOW)
                .output()
                .map_err(|error| format!("run schtasks.exe: {error}"))?;
            let mut text = decode_console_bytes(&output.stdout);
            if !output.status.success() {
                text.push_str(&decode_console_bytes(&output.stderr));
            }
            Ok((output.status.success(), text))
        }
    }

    /// `schtasks /XML` emits UTF-16LE with a BOM; everything else is 8-bit.
    /// Decode both rather than guess: a mis-decoded query looks like a missing
    /// task, and a missing task triggers a repair.
    fn decode_console_bytes(bytes: &[u8]) -> String {
        if bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] == 0xFE {
            let units: Vec<u16> = bytes[2..]
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .collect();
            return String::from_utf16_lossy(&units);
        }
        String::from_utf8_lossy(bytes).into_owned()
    }

    impl SupervisorHost for WindowsHost {
        /// Two read-only queries, deliberately.
        ///
        /// The listing decides existence in a locale-independent way. Parsing
        /// `ERROR: The system cannot find...` would read as `Missing` on an
        /// English machine and as something else everywhere else; only a
        /// listing that *succeeded* and does not contain the name proves
        /// `Missing`. Every other failure is `Unknown`.
        fn query_task(&self, task_name: &str) -> TaskRegistration {
            let listing = match Self::schtasks(&query_listing_args()) {
                Ok((true, text)) => text,
                Ok((false, text)) => {
                    return TaskRegistration::Unknown {
                        reason: format!("schtasks listing failed: {}", text.trim()),
                    }
                }
                Err(error) => return TaskRegistration::Unknown { reason: error },
            };
            if !listing.contains(task_name) {
                return TaskRegistration::Missing;
            }

            match Self::schtasks(&query_definition_args(task_name)) {
                Ok((true, xml)) => match extract_xml_element(&xml, "Command") {
                    Some(command_line) => TaskRegistration::Registered { command_line },
                    None => TaskRegistration::Unknown {
                        reason: "task XML has no <Command> element".to_string(),
                    },
                },
                Ok((false, text)) => TaskRegistration::Unknown {
                    reason: format!("schtasks XML query failed: {}", text.trim()),
                },
                Err(error) => TaskRegistration::Unknown { reason: error },
            }
        }

        /// Register from XML so a repair carries the same restart-on-failure
        /// policy the installer set. The argument vector comes from
        /// [`register_task_args`], which is where the flag policy is asserted.
        fn register_task(&self, spec: &TaskSpec) -> Result<(), String> {
            let xml = task_definition_xml(spec)?;
            let directory = tempfile::tempdir()
                .map_err(|error| format!("create temp dir for task XML: {error}"))?;
            let path = directory.path().join("buzz-edge-task.xml");
            let mut encoded: Vec<u8> = vec![0xFF, 0xFE];
            for unit in xml.encode_utf16() {
                encoded.extend_from_slice(&unit.to_le_bytes());
            }
            std::fs::write(&path, &encoded)
                .map_err(|error| format!("write task XML {}: {error}", path.display()))?;
            let (ok, text) = Self::schtasks(&register_task_args(
                &spec.task_name,
                &path.display().to_string(),
            ))?;
            if ok {
                Ok(())
            } else {
                Err(format!("schtasks /Create failed: {}", text.trim()))
            }
        }

        fn probe_health(&self) -> SidecarHealth {
            let Some(url) = self.probe_url.clone() else {
                return SidecarHealth::Indeterminate {
                    reason: "BUZZ_EDGE_RELAY_URL is not a loopback origin".to_string(),
                };
            };
            probe_with_deadline(move || blocking_probe(&url), READINESS_DEADLINE)
        }

        fn spawn_sidecar(&self, exe: &Path) -> Result<(), String> {
            if !exe.exists() {
                return Err(format!("sidecar binary is missing at {}", exe.display()));
            }
            Command::new(exe)
                .creation_flags(CREATE_NO_WINDOW)
                .spawn()
                .map(|_| ())
                .map_err(|error| format!("spawn {}: {error}", exe.display()))
        }

        fn sidecar_exists(&self, exe: &Path) -> bool {
            exe.exists()
        }

        /// Real time, on the detached supervision worker only. Nothing on the
        /// startup path ever waits here.
        fn wait_before_reprobe(&self, delay: Duration) {
            std::thread::sleep(delay);
        }

        fn load_ledger(&self) -> LedgerLoad {
            SupervisorLedger::load_from(&self.ledger_path)
        }

        fn store_ledger(&self, ledger: &SupervisorLedger) -> Result<(), String> {
            ledger.store_to(&self.ledger_path)
        }
    }

    /// One health request. Only a *refused* connection reports `NotRunning`;
    /// every other transport failure is classified as unknown by
    /// [`classify_probe_failure`].
    fn blocking_probe(url: &str) -> SidecarHealth {
        let client = match reqwest::blocking::Client::builder()
            .timeout(READINESS_DEADLINE)
            .build()
        {
            Ok(client) => client,
            Err(error) => {
                return SidecarHealth::Indeterminate {
                    reason: format!("could not build health probe client: {error}"),
                }
            }
        };
        let response = match client.get(url).send() {
            Ok(response) => response,
            Err(error) => return classify_probe_failure(error.is_connect(), error.is_timeout()),
        };
        let status = response.status().as_u16();
        match response.text() {
            Ok(body) => classify_probe_response(status, &body),
            Err(error) => SidecarHealth::Indeterminate {
                reason: format!("health response body unreadable: {error}"),
            },
        }
    }

    /// Launch hook. Returns immediately in every case.
    ///
    /// With `BUZZ_EDGE_RELAY_URL` unset this does nothing at all — no thread,
    /// no query, no file, no log line. With it set, supervision runs on a
    /// detached worker, so the readiness probe can never delay startup; an
    /// edge that never becomes ready degrades to canonical-only instead.
    ///
    /// The `launch_env` guard must stay the first statement: everything below
    /// it either logs or touches the machine.
    pub fn start(app_data_dir: Option<PathBuf>, app_version: &str) {
        let Some(env) = launch_env(app_version) else {
            return;
        };
        let Some(ledger) = ledger_path(app_data_dir) else {
            eprintln!(
                "buzz-desktop: [GUARDRAIL] edge-supervisor: no app data directory; skipping \
                 supervision rather than running without a durable repair cap"
            );
            return;
        };
        std::thread::spawn(move || {
            let url = env.edge_relay_url.clone().unwrap_or_default();
            supervise_launch(&WindowsHost::new(ledger, &url), &env).emit();
        });
    }
}
