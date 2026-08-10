//! Host-capability detection for tests that drive a **real** POSIX subprocess.
//!
//! # Why this exists (BUG-049)
//!
//! A large block of `acp::tests::` builds fake ACP agents out of shell scripts
//! (`bash -c '...'`) and an inert `cat` pipe. Those tests are meaningful only on
//! a host where a POSIX toolchain can actually execute. On a host where it
//! cannot — a Windows box whose WSL install is broken, for example, where
//! `bash` resolves to a launcher that spawns fine and then dies with
//! `execvpe(/bin/bash) failed: No such file or directory` — every one of them
//! failed with a generic panic. A developer reading `33 failed` on every single
//! run learns to ignore the number, and the day one of those tests catches a
//! real regression it looks exactly the same.
//!
//! So the tests now *skip explicitly, with the reason printed*, instead of
//! failing anonymously.
//!
//! # Design constraints
//!
//! - **Runtime probe, never a platform `cfg`.** `cfg!(windows)` would darken
//!   these tests on a Windows host with a *working* WSL/POSIX shell, which is
//!   precisely the host they are supposed to run on. The probe therefore
//!   *executes* the shell and checks that it produced the expected bytes and
//!   exited 0 — a launcher that spawns successfully and then fails internally
//!   is correctly reported as unavailable.
//! - **Probe once per process.** Cached in a [`OnceLock`]; the ~33 guarded
//!   tests pay for one `bash` spawn between them, not 33.
//! - **Never swallow the failure.** The `io::Error` / exit status / stderr that
//!   caused the probe to fail is captured into the reason string and printed on
//!   every skip line. A skip with no reason is indistinguishable from coverage,
//!   which is the bug this module exists to fix.
//!
//! # Output visibility
//!
//! `cargo test` captures test output and shows it only for failing tests unless
//! `--nocapture` is passed. That capture is installed on the `print!` /
//! `eprintln!` macro path (`std::io::_print` consults the thread's output
//! capture); writes made through a `std::io::Stdout` handle directly do **not**
//! go through it. [`emit_skip`] therefore writes to the process's real stdout
//! handle, so the `[SKIP …]` lines appear in a plain `cargo test` run with no
//! extra flags.

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

/// The shell `acp::tests::spawn_script` drives its fake agents through.
pub(crate) const PROBE_SHELL: &str = "bash";

/// The coreutil `acp::tests::spawn_inert_client` execs directly (not via the
/// shell), so shell availability alone does not imply it is reachable.
pub(crate) const PROBE_CAT: &str = "cat";

/// Sentinel the probe script echoes back. Proves the shell *executed* the
/// script rather than merely starting and exiting 0 for some other reason.
const PROBE_MARKER: &str = "buzz-posix-probe-ok";

/// Prefix every skip line carries, so the lines are greppable and unmissable.
const SKIP_PREFIX: &str = "[SKIP";

/// Cached probe result. `Ok(())` = the POSIX test toolchain works here;
/// `Err(reason)` = it does not, and `reason` says exactly why.
static POSIX_TOOLCHAIN: OnceLock<Result<(), String>> = OnceLock::new();

/// Running count of skips emitted this process, so the last line doubles as the
/// total. See the "Limitations" note on [`emit_skip`].
static SKIP_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Serializes writes to the raw stdout handle so concurrently-skipping tests
/// cannot interleave halves of a line.
static EMIT_LOCK: Mutex<()> = Mutex::new(());

/// Whether a POSIX shell + `cat` can actually run on this host.
///
/// Probed once per process and cached. `Err` carries a human-readable reason
/// naming the program and the underlying failure.
pub(crate) fn posix_toolchain_status() -> &'static Result<(), String> {
    POSIX_TOOLCHAIN.get_or_init(probe_posix_toolchain)
}

/// Run the probes. First failure wins and is reported verbatim.
fn probe_posix_toolchain() -> Result<(), String> {
    probe_shell()?;
    probe_cat()
}

/// Execute `bash -c "printf '%s' <marker>"` and require: it spawns, it exits 0,
/// and it wrote the marker. Anything else is a reason string.
fn probe_shell() -> Result<(), String> {
    let script = format!("printf '%s' {PROBE_MARKER}");

    let output = Command::new(PROBE_SHELL)
        .arg("-c")
        .arg(&script)
        .stdin(Stdio::null())
        .output()
        .map_err(|err| {
            format!(
                "no POSIX shell available: spawning `{PROBE_SHELL} -c \"{script}\"` \
                 failed: {err} (io::ErrorKind::{kind:?})",
                kind = err.kind(),
            )
        })?;

    if !output.status.success() {
        return Err(format!(
            "no POSIX shell available: `{PROBE_SHELL}` started but exited {status} \
             (stderr: {stderr})",
            status = output.status,
            stderr = describe_stream(&output.stderr),
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.trim() != PROBE_MARKER {
        return Err(format!(
            "no POSIX shell available: `{PROBE_SHELL}` exited 0 but did not run the \
             script — expected {PROBE_MARKER:?} on stdout, got {got:?} \
             (stderr: {stderr})",
            got = stdout.trim(),
            stderr = describe_stream(&output.stderr),
        ));
    }

    Ok(())
}

/// Execute `cat` against an empty stdin and require it spawns and exits 0.
fn probe_cat() -> Result<(), String> {
    let output = Command::new(PROBE_CAT)
        .stdin(Stdio::null())
        .output()
        .map_err(|err| {
            format!(
                "no POSIX `{PROBE_CAT}` available: spawning it failed: {err} \
                 (io::ErrorKind::{kind:?})",
                kind = err.kind(),
            )
        })?;

    if !output.status.success() {
        return Err(format!(
            "no POSIX `{PROBE_CAT}` available: it started but exited {status} \
             (stderr: {stderr})",
            status = output.status,
            stderr = describe_stream(&output.stderr),
        ));
    }

    Ok(())
}

/// Render a captured stream for a reason string: trimmed, single-line, bounded.
fn describe_stream(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let flattened = text.trim().replace(['\r', '\n'], " ");
    if flattened.is_empty() {
        return "<empty>".to_string();
    }
    const MAX: usize = 300;
    match flattened.char_indices().nth(MAX) {
        Some((cut, _)) => format!("{}…", &flattened[..cut]),
        None => flattened,
    }
}

/// Turn `module_path!()` + a test fn name into the name libtest prints, e.g.
/// `("buzz_acp::acp::tests", "idle_resets_on_stdout_activity")` becomes
/// `acp::tests::idle_resets_on_stdout_activity`.
pub(crate) fn qualified_test_name(module_path: &str, test_fn: &str) -> String {
    let module = module_path
        .strip_prefix(concat!(env!("CARGO_CRATE_NAME"), "::"))
        .unwrap_or(module_path);
    format!("{module}::{test_fn}")
}

/// The exact bytes one skip emits. Kept pure so it can be asserted on directly.
///
/// Leading newline on purpose: libtest writes its own `test <name> ... ` prefix
/// and the trailing `ok` as two separate writes, so an unprefixed skip line
/// lands mid-prefix and stops being greppable as `^\[SKIP`.
pub(crate) fn format_skip_line(index: usize, test: &str, reason: &str) -> String {
    format!("\n{SKIP_PREFIX} #{index}] {test} — {reason}\n")
}

/// Write one skip line to `sink`.
///
/// Split out from [`emit_skip`] so tests can capture the bytes and prove the
/// skip is neither silent nor missing its reason.
pub(crate) fn emit_skip_to<W: Write>(sink: &mut W, index: usize, test: &str, reason: &str) {
    let line = format_skip_line(index, test, reason);
    // A failed write here would make a skip silent — the exact failure mode
    // this module exists to prevent — so it is reported, never swallowed.
    if let Err(err) = sink.write_all(line.as_bytes()) {
        eprintln!("{SKIP_PREFIX}] failed to report skip for {test}: {err}");
    }
}

/// Report that `test` did not execute, and why.
///
/// # Limitations
///
/// libtest has no stable hook for amending its own `test result:` line, and a
/// `#[test]` that returns early is counted as **passed**, not `ignored`
/// (`#[ignore]` is deliberately not used: it is a compile-time decision and
/// prints no reason). The per-skip lines below are therefore the honest record
/// of what ran. The `#N` counter makes the final line the running total for the
/// process, which is the closest thing to a summary available without replacing
/// the harness (`harness = false`).
pub(crate) fn emit_skip(test: &str, reason: &str) {
    let index = SKIP_COUNT.fetch_add(1, Ordering::SeqCst) + 1;

    // Bypasses libtest's output capture (which hooks the `print!`/`eprint!`
    // macro path, not `Stdout` handle writes), so the reason is visible in a
    // plain `cargo test` run without `--nocapture`.
    let _guard = EMIT_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut out = std::io::stdout();
    emit_skip_to(&mut out, index, test, reason);
    let _ = out.flush();
}

/// Skip the calling test, loudly and by name, when this host has no working
/// POSIX shell.
///
/// Evaluates to `true` when the test must not run (a `[SKIP …]` line naming the
/// test and the probe failure has been emitted) and `false` when it may.
///
/// ```ignore
/// #[tokio::test]
/// async fn my_test() {
///     if skip_without_posix_shell!("my_test") {
///         return;
///     }
///     // …drives `bash` from here on…
/// }
/// ```
#[macro_export]
macro_rules! skip_without_posix_shell {
    ($test_fn:expr) => {
        match $crate::test_support::posix_toolchain_status() {
            ::std::result::Result::Ok(()) => false,
            ::std::result::Result::Err(reason) => {
                $crate::test_support::emit_skip(
                    &$crate::test_support::qualified_test_name(module_path!(), $test_fn),
                    reason,
                );
                true
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The guardrail on the guardrail: a skip must carry the test name AND the
    /// verbatim probe reason, and must never write nothing at all. Drop the
    /// reason from `format_skip_line`, or make `emit_skip_to` a no-op, and this
    /// test goes red.
    #[test]
    fn skip_line_names_the_test_and_the_reason() {
        let reason = "no POSIX shell available: spawning `bash -c \"printf\"` failed: \
                      program not found (io::ErrorKind::NotFound)";
        let mut buf: Vec<u8> = Vec::new();

        emit_skip_to(&mut buf, 7, "acp::tests::example_test", reason);

        let line = String::from_utf8(buf).expect("skip line must be valid UTF-8");
        assert!(
            !line.trim().is_empty(),
            "a skip must never be silent — it emitted nothing"
        );
        assert!(
            line.contains(SKIP_PREFIX),
            "skip line must be greppable by {SKIP_PREFIX:?}: {line:?}"
        );
        assert!(
            line.contains("acp::tests::example_test"),
            "skip line must name the skipped test: {line:?}"
        );
        assert!(
            line.contains(reason),
            "skip line must carry the probe failure reason verbatim: {line:?}"
        );
        assert!(
            line.ends_with('\n'),
            "skip line must be newline-terminated so it cannot merge into harness output: {line:?}"
        );
        assert!(
            line.starts_with('\n'),
            "skip line must start on a fresh line so `^\\[SKIP` stays greppable \
             against libtest's interleaved writes: {line:?}"
        );
    }

    /// A skip index must appear, so the final line doubles as the total.
    #[test]
    fn skip_line_carries_its_running_index() {
        let line = format_skip_line(12, "acp::tests::example_test", "reason");
        assert!(
            line.contains("#12"),
            "skip line must carry its running index: {line:?}"
        );
    }

    /// The printed name must match what libtest prints, or a developer cannot
    /// map a skip line back to a test.
    #[test]
    fn qualified_test_name_matches_libtest_naming() {
        assert_eq!(
            qualified_test_name("buzz_acp::acp::tests", "idle_resets_on_stdout_activity"),
            "acp::tests::idle_resets_on_stdout_activity"
        );
        // Unrecognised prefixes are passed through rather than mangled.
        assert_eq!(
            qualified_test_name("other::module", "thing"),
            "other::module::thing"
        );
    }

    /// Whatever this host is, the probe must never report failure without
    /// saying why, and the reason must name the program that failed.
    #[test]
    fn probe_failure_always_carries_a_named_reason() {
        match posix_toolchain_status() {
            Ok(()) => { /* host has a POSIX toolchain — nothing to assert */ }
            Err(reason) => {
                assert!(
                    !reason.trim().is_empty(),
                    "probe failure reason must never be empty"
                );
                assert!(
                    reason.contains(PROBE_SHELL) || reason.contains(PROBE_CAT),
                    "probe failure reason must name the program that failed: {reason:?}"
                );
            }
        }
    }

    /// The probe is cached: repeated calls return the same decision, so ~33
    /// guarded tests cost one spawn, not 33.
    #[test]
    fn probe_result_is_cached_and_stable() {
        let first = posix_toolchain_status().is_ok();
        let second = posix_toolchain_status().is_ok();
        assert_eq!(first, second, "probe must be cached, not re-run per call");
    }

    /// Long stderr is bounded and single-lined, and empty stderr is labelled
    /// rather than silently blank.
    #[test]
    fn describe_stream_is_bounded_and_single_line() {
        assert_eq!(describe_stream(b""), "<empty>");
        assert_eq!(describe_stream(b"  a\nb\r\nc  "), "a b  c");
        let long = vec![b'x'; 1000];
        let described = describe_stream(&long);
        assert!(described.len() < 1000, "stderr must be truncated");
        assert!(described.ends_with('…'), "truncation must be marked");
    }
}
