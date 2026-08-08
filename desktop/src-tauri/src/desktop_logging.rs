use serde::Deserialize;
use std::backtrace::Backtrace;
use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, TryLockError};
use std::time::{Duration, Instant};
use tracing_subscriber::fmt::MakeWriter;

const ACTIVE_LOG_NAME: &str = "buzz-desktop.log";
const MAX_LOG_FILE_BYTES: u64 = 2 * 1024 * 1024;
const LOG_BACKUP_COUNT: usize = 4;
const FRONTEND_REPORT_BUDGET: usize = 20;
const FRONTEND_REPORT_WINDOW: Duration = Duration::from_secs(60);
const MAX_FRONTEND_KIND_BYTES: usize = 64;
const MAX_FRONTEND_MESSAGE_BYTES: usize = 4 * 1024;
const MAX_FRONTEND_STACK_BYTES: usize = 16 * 1024;
const MAX_FRONTEND_SOURCE_BYTES: usize = 2 * 1024;

static DESKTOP_LOG: OnceLock<DesktopLog> = OnceLock::new();

struct DesktopLog {
    writer: RotatingWriterFactory,
    frontend_throttle: Mutex<FrontendErrorThrottle>,
}

#[derive(Clone)]
struct RotatingWriterFactory {
    inner: Arc<Mutex<RotatingFile>>,
    active_path: Arc<PathBuf>,
}

impl RotatingWriterFactory {
    fn open(directory: &Path, policy: RotationPolicy) -> io::Result<Self> {
        let rotating_file = RotatingFile::open(directory, ACTIVE_LOG_NAME, policy)?;
        let active_path = Arc::new(rotating_file.active_path.clone());
        Ok(Self {
            inner: Arc::new(Mutex::new(rotating_file)),
            active_path,
        })
    }

    fn lock(&self) -> MutexGuard<'_, RotatingFile> {
        match self.inner.lock() {
            Ok(writer) => writer,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn write_panic(&self, bytes: &[u8]) {
        match self.inner.try_lock() {
            Ok(mut writer) => {
                let _ = writer.write_bytes(bytes);
                let _ = writer.flush();
            }
            Err(TryLockError::Poisoned(poisoned)) => {
                let mut writer = poisoned.into_inner();
                let _ = writer.write_bytes(bytes);
                let _ = writer.flush();
            }
            Err(TryLockError::WouldBlock) => {
                if let Ok(mut file) = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(self.active_path.as_ref())
                {
                    let _ = file.write_all(bytes);
                    let _ = file.flush();
                }
            }
        }
    }

    fn flush(&self) {
        let _ = self.lock().flush();
    }
}

struct SharedWriter {
    factory: RotatingWriterFactory,
}

impl Write for SharedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.factory.lock().write_bytes(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.factory.lock().flush()
    }
}

impl<'a> MakeWriter<'a> for RotatingWriterFactory {
    type Writer = SharedWriter;

    fn make_writer(&'a self) -> Self::Writer {
        SharedWriter {
            factory: self.clone(),
        }
    }
}

#[derive(Clone, Copy)]
struct RotationPolicy {
    max_file_bytes: u64,
    backup_count: usize,
}

impl RotationPolicy {
    const PRODUCTION: Self = Self {
        max_file_bytes: MAX_LOG_FILE_BYTES,
        backup_count: LOG_BACKUP_COUNT,
    };
}

struct RotatingFile {
    directory: PathBuf,
    active_name: String,
    active_path: PathBuf,
    file: Option<File>,
    size: u64,
    policy: RotationPolicy,
}

impl RotatingFile {
    fn open(directory: &Path, active_name: &str, policy: RotationPolicy) -> io::Result<Self> {
        fs::create_dir_all(directory)?;
        let active_path = directory.join(active_name);
        let size = fs::metadata(&active_path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&active_path)?;
        let mut rotating = Self {
            directory: directory.to_path_buf(),
            active_name: active_name.to_owned(),
            active_path,
            file: Some(file),
            size,
            policy,
        };
        if rotating.size >= rotating.policy.max_file_bytes {
            rotating.rotate()?;
        }
        Ok(rotating)
    }

    fn backup_path(&self, index: usize) -> PathBuf {
        self.directory
            .join(format!("{}.{}", self.active_name, index))
    }

    fn rotate(&mut self) -> io::Result<()> {
        if let Some(mut file) = self.file.take() {
            file.flush()?;
        }

        for index in (1..self.policy.backup_count).rev() {
            let source = self.backup_path(index);
            let destination = self.backup_path(index + 1);
            if destination.exists() {
                fs::remove_file(&destination)?;
            }
            if source.exists() {
                fs::rename(source, destination)?;
            }
        }

        if self.policy.backup_count > 0 && self.active_path.exists() {
            let first_backup = self.backup_path(1);
            if first_backup.exists() {
                fs::remove_file(&first_backup)?;
            }
            fs::rename(&self.active_path, first_backup)?;
        } else if self.active_path.exists() {
            fs::remove_file(&self.active_path)?;
        }

        self.file = Some(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.active_path)?,
        );
        self.size = 0;
        Ok(())
    }

    fn write_bytes(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let write_len = bytes.len().min(self.policy.max_file_bytes as usize);
        if self.size + write_len as u64 > self.policy.max_file_bytes {
            self.rotate()?;
        }
        let Some(file) = self.file.as_mut() else {
            return Err(io::Error::other("desktop log file is not open"));
        };
        file.write_all(&bytes[..write_len])?;
        self.size += write_len as u64;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.file.as_mut() {
            Some(file) => file.flush(),
            None => Ok(()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThrottleDecision {
    Accept,
    SuppressAndReport,
    Suppress,
}

struct FrontendErrorThrottle {
    accepted_at: VecDeque<Instant>,
    suppression_reported: bool,
}

impl FrontendErrorThrottle {
    fn new(_now: Instant) -> Self {
        Self {
            accepted_at: VecDeque::new(),
            suppression_reported: false,
        }
    }

    fn decide(&mut self, now: Instant, budget: usize, window: Duration) -> ThrottleDecision {
        while self.accepted_at.front().is_some_and(|accepted_at| {
            now.checked_duration_since(*accepted_at).unwrap_or_default() >= window
        }) {
            self.accepted_at.pop_front();
        }
        if self.accepted_at.is_empty() {
            self.suppression_reported = false;
        }

        if self.accepted_at.len() < budget {
            self.accepted_at.push_back(now);
            ThrottleDecision::Accept
        } else if !self.suppression_reported {
            self.suppression_reported = true;
            ThrottleDecision::SuppressAndReport
        } else {
            ThrottleDecision::Suppress
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FrontendErrorReport {
    kind: String,
    message: String,
    stack: Option<String>,
    source: Option<String>,
    line: Option<u32>,
    column: Option<u32>,
}

impl FrontendErrorReport {
    fn bounded(mut self) -> Self {
        self.kind = truncate_utf8(self.kind, MAX_FRONTEND_KIND_BYTES);
        self.message = truncate_utf8(self.message, MAX_FRONTEND_MESSAGE_BYTES);
        self.stack = self
            .stack
            .map(|stack| truncate_utf8(stack, MAX_FRONTEND_STACK_BYTES));
        self.source = self
            .source
            .map(|source| truncate_utf8(source, MAX_FRONTEND_SOURCE_BYTES));
        self
    }
}

fn truncate_utf8(mut value: String, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    value
}

pub(crate) fn install(app_identifier: &str) -> Result<PathBuf, String> {
    let logs_directory = dirs::data_dir()
        .ok_or_else(|| "operating system did not provide an application-data directory".to_owned())?
        .join(app_identifier)
        .join("logs");
    let writer = RotatingWriterFactory::open(&logs_directory, RotationPolicy::PRODUCTION)
        .map_err(|error| format!("failed to open {}: {error}", logs_directory.display()))?;
    let active_path = writer.active_path.as_ref().clone();
    DESKTOP_LOG
        .set(DesktopLog {
            writer: writer.clone(),
            frontend_throttle: Mutex::new(FrontendErrorThrottle::new(Instant::now())),
        })
        .map_err(|_| "desktop logging was initialized more than once".to_owned())?;

    install_panic_hook();
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .with_target(true)
        .with_thread_ids(true)
        .with_thread_names(true)
        .with_writer(writer)
        .try_init()
        .map_err(|error| format!("failed to install tracing subscriber: {error}"))?;
    Ok(active_path)
}

/// Lifecycle markers live here rather than at each call site so `lib.rs` and
/// `shutdown.rs` stay within the desktop file-size ratchet, and so the marker
/// vocabulary a crash reader depends on is defined in exactly one place.
pub(crate) fn install_and_announce_startup(app_identifier: &str) {
    match install(app_identifier) {
        Ok(log_path) => tracing::info!(
            event = "startup",
            version = env!("CARGO_PKG_VERSION"),
            app_identifier = %app_identifier,
            log_path = %log_path.display(),
            "Buzz desktop starting"
        ),
        // Fail open: diagnostics must never keep the desktop from launching.
        Err(error) => eprintln!("buzz-desktop: persistent logging unavailable: {error}"),
    }
}

/// A duplicate launch handed its arguments to the running instance. This is the
/// hop the restart deep link depends on, and nothing recorded it before.
pub(crate) fn log_second_instance(argv: &[String]) {
    tracing::info!(
        event = "second_instance",
        args = argv.len(),
        // Scheme only. The full URL carries the control token.
        buzz_links = argv.iter().filter(|a| a.starts_with("buzz://")).count(),
        "duplicate launch forwarded its arguments"
    );
}

/// Record the launch-time agent-restore decision.
///
/// "No agents started" has three indistinguishable causes from outside: repos
/// unresolved, recovery mode, or the frontend never applying a workspace. This
/// says which.
pub(crate) fn log_agent_restore_gate(
    restore_agents: bool,
    recovery_mode: bool,
    identity_lost: bool,
    keyring_locked: bool,
) {
    tracing::info!(
        event = "agent_restore_gate",
        restore_agents,
        recovery_mode,
        identity_lost,
        keyring_locked,
        "evaluated launch-time agent restore"
    );
}

pub(crate) fn log_window_created(window_label: &str) {
    tracing::info!(
        event = "window_created",
        window_label,
        "desktop window created"
    );
}

pub(crate) fn log_shutdown_requested(exit_code: Option<i32>, source: &str) {
    tracing::info!(
        event = "shutdown_requested",
        exit_code = ?exit_code,
        source,
        "desktop shutdown requested"
    );
}

pub(crate) fn log_cleanup_started() {
    tracing::info!(event = "cleanup_started", "desktop cleanup starting");
}

pub(crate) fn log_cleanup_completed() {
    tracing::info!(event = "cleanup_completed", "desktop cleanup completed");
    flush();
}

/// Final marker. A log whose tail has no `exit` record is an abnormal death.
pub(crate) fn log_exit(source: &str) {
    tracing::info!(event = "exit", source, "desktop exiting");
    flush();
}

fn install_panic_hook() {
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let payload = panic_info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| {
                panic_info
                    .payload()
                    .downcast_ref::<String>()
                    .map(String::as_str)
            })
            .unwrap_or("non-string panic payload");
        let location = panic_info
            .location()
            .map(|location| {
                format!(
                    "{}:{}:{}",
                    location.file(),
                    location.line(),
                    location.column()
                )
            })
            .unwrap_or_else(|| "unknown".to_owned());
        let record = format!(
            "{} ERROR buzz_desktop::panic panic at {location}: {payload}\nBacktrace:\n{}\n",
            chrono::Utc::now().to_rfc3339(),
            Backtrace::force_capture()
        );
        if let Some(log) = DESKTOP_LOG.get() {
            log.writer.write_panic(record.as_bytes());
        }
        previous_hook(panic_info);
    }));
}

#[tauri::command]
pub(crate) fn report_frontend_error(report: FrontendErrorReport) {
    let report = report.bounded();
    let decision = DESKTOP_LOG
        .get()
        .map(|log| {
            let mut throttle = match log.frontend_throttle.lock() {
                Ok(throttle) => throttle,
                Err(poisoned) => poisoned.into_inner(),
            };
            throttle.decide(
                Instant::now(),
                FRONTEND_REPORT_BUDGET,
                FRONTEND_REPORT_WINDOW,
            )
        })
        .unwrap_or(ThrottleDecision::Accept);

    match decision {
        ThrottleDecision::Accept => tracing::error!(
            target: "buzz_desktop::frontend",
            error_kind = %report.kind,
            message = %report.message,
            stack = report.stack.as_deref().unwrap_or(""),
            source = report.source.as_deref().unwrap_or(""),
            line = ?report.line,
            column = ?report.column,
            "frontend error"
        ),
        ThrottleDecision::SuppressAndReport => tracing::warn!(
            target: "buzz_desktop::frontend",
            budget = FRONTEND_REPORT_BUDGET,
            window_seconds = FRONTEND_REPORT_WINDOW.as_secs(),
            "suppressing repeated frontend errors"
        ),
        ThrottleDecision::Suppress => {}
    }
}

pub(crate) fn flush() {
    if let Some(log) = DESKTOP_LOG.get() {
        log.writer.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotation_keeps_newest_files_and_prunes_oldest() {
        let directory = tempfile::tempdir().unwrap();
        let policy = RotationPolicy {
            max_file_bytes: 8,
            backup_count: 2,
        };
        let mut writer = RotatingFile::open(directory.path(), "test.log", policy).unwrap();

        writer.write_bytes(b"one\n").unwrap();
        writer.write_bytes(b"two\n").unwrap();
        writer.write_bytes(b"three\n").unwrap();
        writer.write_bytes(b"four\n").unwrap();
        writer.write_bytes(b"five\n").unwrap();
        writer.flush().unwrap();

        assert_eq!(
            fs::read_to_string(directory.path().join("test.log")).unwrap(),
            "five\n"
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("test.log.1")).unwrap(),
            "four\n"
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("test.log.2")).unwrap(),
            "three\n"
        );
        let total: u64 = fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().metadata().unwrap().len())
            .sum();
        assert!(total <= policy.max_file_bytes * (policy.backup_count as u64 + 1));
    }

    #[test]
    fn frontend_throttle_reports_suppression_once_then_resets() {
        let start = Instant::now();
        let mut throttle = FrontendErrorThrottle::new(start);
        let window = Duration::from_secs(60);

        assert_eq!(throttle.decide(start, 2, window), ThrottleDecision::Accept);
        assert_eq!(throttle.decide(start, 2, window), ThrottleDecision::Accept);
        assert_eq!(
            throttle.decide(start, 2, window),
            ThrottleDecision::SuppressAndReport
        );
        assert_eq!(
            throttle.decide(start, 2, window),
            ThrottleDecision::Suppress
        );
        assert_eq!(
            throttle.decide(start + window, 2, window),
            ThrottleDecision::Accept
        );
    }

    #[test]
    fn frontend_fields_are_truncated_on_utf8_boundaries() {
        let report = FrontendErrorReport {
            kind: "k".repeat(MAX_FRONTEND_KIND_BYTES + 1),
            message: "é".repeat(MAX_FRONTEND_MESSAGE_BYTES),
            stack: Some("s".repeat(MAX_FRONTEND_STACK_BYTES + 1)),
            source: Some("ø".repeat(MAX_FRONTEND_SOURCE_BYTES)),
            line: None,
            column: None,
        }
        .bounded();

        assert_eq!(report.kind.len(), MAX_FRONTEND_KIND_BYTES);
        assert!(report.message.len() <= MAX_FRONTEND_MESSAGE_BYTES);
        assert!(report.message.is_char_boundary(report.message.len()));
        assert_eq!(report.stack.unwrap().len(), MAX_FRONTEND_STACK_BYTES);
        assert!(report.source.unwrap().len() <= MAX_FRONTEND_SOURCE_BYTES);
    }
}
