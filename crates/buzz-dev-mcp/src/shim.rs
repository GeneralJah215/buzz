use nostr::ToBech32;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tempfile::TempDir;
use zeroize::Zeroize;

/// Session-scoped shim directory providing tools and git config to shell children.
///
/// On install:
/// 1. Sweeps stale shim/session directories left by dead processes (see
///    [`sweep_stale_dirs`]) — best effort, never fatal
/// 2. Creates a 0700 tempdir with symlinks back to our binary (multicall)
/// 3. If `NOSTR_PRIVATE_KEY` is set: writes a 0600 keyfile, derives the pubkey,
///    builds ephemeral `GIT_CONFIG_*` env vars, then removes the env var
/// 4. Prepends the shim dir to PATH
///
/// Shell children receive `path_env`, `git_env`, and `BUZZ_PRIVATE_KEY` (for
/// the buzz CLI). `NOSTR_PRIVATE_KEY` is removed from the process env after
/// the keyfile is written — git helpers read from the keyfile only.
///
/// `TempDir`'s `Drop` removes the directory on a graceful exit, but the shipped
/// reaping strategy for agent trees is a job object with
/// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` — a `TerminateProcess`, which runs no
/// destructor. `Drop` is therefore the happy path only; the startup sweep is
/// what actually bounds disk use. See BUG-036.
pub struct Shim {
    _dir: TempDir,
    pub path_env: String,
    pub git_env: Vec<(String, String)>,
}

impl Shim {
    pub fn install() -> std::io::Result<Self> {
        // Housekeeping runs BEFORE we allocate anything, and can never fail the
        // install: a sweep problem is a disk-usage problem, not a startup
        // problem. `sweep_stale_dirs` swallows nothing — every skipped or failed
        // path is logged — but it returns a report instead of an error.
        let _ = sweep_stale_dirs(&std::env::temp_dir());

        let dir = tempfile::Builder::new()
            .prefix(&shim_dir_prefix())
            .tempdir()?;
        set_owner_only(dir.path())?;

        let self_exe = std::env::current_exe()?;
        install_tools(&self_exe, dir.path())?;

        let original = std::env::var_os("PATH").unwrap_or_default();
        let mut entries = vec![PathBuf::from(dir.path())];
        entries.extend(std::env::split_paths(&original));
        // join_paths uses the platform separator (':' on Unix, ';' on Windows).
        let path_env = std::env::join_paths(entries)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?
            .to_string_lossy()
            .into_owned();

        // Read and unconditionally remove NOSTR_PRIVATE_KEY from this process's
        // env. The key must never leak to child processes regardless of whether
        // keyfile creation succeeds.
        let mut nostr_key = std::env::var("NOSTR_PRIVATE_KEY").ok();
        std::env::remove_var("NOSTR_PRIVATE_KEY");

        // Ephemeral git config: write key to 0600 keyfile, derive pubkey, build
        // GIT_CONFIG_* env vars for nostr auth + signing.
        let git_env = match nostr_key
            .as_deref()
            .and_then(|k| write_keyfile(dir.path(), k))
        {
            Some(info) => build_git_env(&info),
            None => Vec::new(),
        };
        if let Some(ref mut k) = nostr_key {
            k.zeroize();
        }

        Ok(Self {
            _dir: dir,
            path_env,
            git_env,
        })
    }
}

struct KeyInfo {
    keyfile_path: String,
    pubkey_hex: String,
    npub: String,
}

/// Write the nostr private key to an owner-only file in the shim dir.
/// Returns key metadata or None if key is empty/invalid.
/// Warns to stderr if the key is invalid (operator mistake).
fn write_keyfile(shim_dir: &Path, raw: &str) -> Option<KeyInfo> {
    if raw.is_empty() {
        return None;
    }
    let keys = match nostr::Keys::parse(raw) {
        Ok(k) => k,
        Err(e) => {
            eprintln!(
                "buzz-dev-mcp: warning: NOSTR_PRIVATE_KEY is set but invalid ({e}); \
                 git auth/signing will be disabled"
            );
            return None;
        }
    };
    let pubkey_hex = keys.public_key().to_hex();
    let npub = keys
        .public_key()
        .to_bech32()
        .unwrap_or_else(|_| pubkey_hex.clone());

    let keyfile = shim_dir.join(".nostr-key");
    if write_keyfile_atomic(&keyfile, raw.as_bytes()).is_err() {
        eprintln!(
            "buzz-dev-mcp: warning: failed to write nostr keyfile; git auth/signing disabled"
        );
        return None;
    }
    let keyfile_path = match keyfile.to_str() {
        Some(s) => s.to_owned(),
        None => {
            eprintln!(
                "buzz-dev-mcp: warning: tempdir path is not valid UTF-8; git auth/signing disabled"
            );
            return None;
        }
    };

    Some(KeyInfo {
        keyfile_path,
        pubkey_hex,
        npub,
    })
}

/// Write `data` to `path` with 0600 permissions set at creation time via
/// `OpenOptions::mode()` (no window where the file is world-readable).
/// Non-Unix: plain write — acceptable inside our 0700 tempdir.
#[cfg(unix)]
fn write_keyfile_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(data)
}

#[cfg(not(unix))]
fn write_keyfile_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, data)
}

/// Derive a NIP-05-style email from the pubkey and relay URL.
/// Format: `<hex_pubkey>@<relay_host>` (e.g., `ab12...cd@relay.buzz.dev`).
/// Falls back to `<hex_pubkey>@buzz` if no relay URL is configured.
fn derive_git_email(pubkey_hex: &str) -> String {
    let host = std::env::var("BUZZ_RELAY_URL")
        .ok()
        .and_then(|url| {
            // Strip scheme, port, and trailing paths
            let stripped = url
                .strip_prefix("https://")
                .or_else(|| url.strip_prefix("http://"))
                .or_else(|| url.strip_prefix("wss://"))
                .or_else(|| url.strip_prefix("ws://"))
                .unwrap_or(&url);
            let host_port = stripped.split('/').next()?;
            // Strip port number (e.g., "localhost:3000" → "localhost")
            Some(host_port.split(':').next().unwrap_or(host_port).to_owned())
        })
        .filter(|h| !h.is_empty() && !h.starts_with("localhost") && !h.starts_with("127."))
        .unwrap_or_else(|| "buzz".to_owned());
    format!("{pubkey_hex}@{host}")
}

/// Stable identity contract for git attribution: the bare agent display name,
/// never channel-qualified, safe to embed in commit history.
///
/// Deliberately distinct from `BUZZ_ACP_SESSION_TITLE`, which is per-session UI
/// chrome and may be composed (`Agent · #channel`) by consumers. Commits
/// outlive sessions, so git attribution must not follow a mutable title.
///
/// Nothing writes this yet — when unset, [`build_git_env`] falls back to the
/// npub, which is byte-for-byte today's behavior.
const DISPLAY_NAME_ENV_VAR: &str = "BUZZ_ACP_DISPLAY_NAME";

/// Max characters in a git author name. Nostr display names are unbounded.
const MAX_GIT_USER_NAME_CHARS: usize = 80;

/// Characters git's `ident.c` treats as "crud": stripped from both ends of a
/// name, and — when a name is *nothing but* these — rejected outright with
/// `fatal: name consists only of disallowed characters`.
///
/// Verified empirically against git 2.54.0 by committing with each ASCII byte
/// 32..=126 as the entire `user.name`: exactly space, `"`, `'`, `,`, `:`, `;`,
/// `<`, `>`, and `\` abort. Control characters abort too (the predicate is
/// `c <= 32`). Note `.` is *not* crud in this version despite older lore.
fn is_git_crud(c: char) -> bool {
    c <= ' ' || matches!(c, '"' | '\'' | ',' | ':' | ';' | '<' | '>' | '\\')
}

/// Characters in Unicode general category `Cf` (format): zero-width space and
/// joiners, bidi embedding/override marks, invisible math operators, interlinear
/// annotations, and tag characters.
///
/// `char::is_control` covers only `Cc`, so every one of these survives it — and
/// none is whitespace or [`is_git_crud`]. A display name of nothing but U+200B
/// ZERO WIDTH SPACE would therefore satisfy the "at least one non-crud
/// character" gate and hand git a visually blank author instead of falling back
/// to the npub. An embedded U+202E RIGHT-TO-LEFT OVERRIDE is worse: it makes a
/// commit's persisted author line render as something other than what it says,
/// the same confusion the angle-bracket filter exists to prevent.
///
/// The whole category is rejected rather than the two known-bad marks, because
/// the boundary that matters is "invisible or reorders text", not "the codepoint
/// someone thought of". Ranges transcribed from the UCD's
/// `DerivedGeneralCategory.txt` (17.0.0) and independently cross-checked against
/// Python's `unicodedata` (16.0.0); both yield exactly these 21 ranges. Inlined
/// rather than taking a Unicode-tables dependency for one predicate.
fn is_unicode_format(c: char) -> bool {
    matches!(c,
        '\u{00AD}'
        | '\u{0600}'..='\u{0605}'
        | '\u{061C}'
        | '\u{06DD}'
        | '\u{070F}'
        | '\u{0890}'..='\u{0891}'
        | '\u{08E2}'
        | '\u{180E}'
        | '\u{200B}'..='\u{200F}'
        | '\u{202A}'..='\u{202E}'
        | '\u{2060}'..='\u{2064}'
        | '\u{2066}'..='\u{206F}'
        | '\u{FEFF}'
        | '\u{FFF9}'..='\u{FFFB}'
        | '\u{110BD}'
        | '\u{110CD}'
        | '\u{13430}'..='\u{1343F}'
        | '\u{1BCA0}'..='\u{1BCA3}'
        | '\u{1D173}'..='\u{1D17A}'
        | '\u{E0001}'
        | '\u{E0020}'..='\u{E007F}'
    )
}

/// Normalize a Buzz display name into a git author name, or `None` to fall
/// back to the npub.
///
/// Strips control and Unicode format characters plus angle brackets, collapses
/// whitespace runs, trims, and caps at [`MAX_GIT_USER_NAME_CHARS`] by `chars()`
/// so a multi-byte name cannot be split mid-UTF-8. Angle brackets go because git
/// silently drops them rather than erroring — `Duncan <evil@x.com>` would
/// render as `Duncan evil@x.com <hex@relay>`, which forges nothing but reads as
/// though it might.
///
/// Returns `None` unless at least one non-crud character survives. A bare
/// emptiness check is not sufficient: git rejects a name built only of crud,
/// so a display name of `;;` or `""` would abort **every commit** the agent
/// makes. Falling back to the npub keeps the agent able to commit.
fn sanitize_git_user_name(raw: &str) -> Option<String> {
    let collapsed = raw
        .split_whitespace()
        .map(|word| {
            word.chars()
                .filter(|c| !c.is_control() && !is_unicode_format(*c) && *c != '<' && *c != '>')
                .collect::<String>()
        })
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let name: String = collapsed
        .chars()
        .take(MAX_GIT_USER_NAME_CHARS)
        .collect::<String>()
        .trim_end()
        .to_string();
    name.chars().any(|c| !is_git_crud(c)).then_some(name)
}

/// Build GIT_CONFIG_COUNT/KEY/VALUE env vars for ephemeral nostr git config.
/// Composes with any existing GIT_CONFIG_COUNT in the environment. When launched
/// via buzz-agent (which clears env), the base is always 0 — composition only
/// matters when dev-mcp is run directly with pre-existing GIT_CONFIG vars.
fn build_git_env(info: &KeyInfo) -> Vec<(String, String)> {
    let email = derive_git_email(&info.pubkey_hex);
    // Display name for humans reading `git log`; the pubkey stays in the email,
    // which is what NIP-98 auth, NIP-GS signing, and contributor matching key on.
    let user_name = std::env::var(DISPLAY_NAME_ENV_VAR)
        .ok()
        .as_deref()
        .and_then(sanitize_git_user_name)
        .unwrap_or_else(|| info.npub.clone());
    let entries: Vec<(&str, String)> = vec![
        // Identity — Buzz display name (npub fallback), NIP-05-style email
        ("user.name", user_name),
        ("user.email", email),
        // Nostr credential helper is additive — it silently declines non-Buzz
        // remotes (exits 0, no credential), so git falls through to system
        // helpers (osxkeychain, store, etc.) for GitHub/GitLab/etc.
        ("credential.helper", "nostr".into()),
        // Required: Buzz relay verifies NIP-98 against the full repo-root URL.
        // Without useHttpPath, git only passes the host and auth is rejected.
        ("credential.useHttpPath", "true".into()),
        ("nostr.keyfile", info.keyfile_path.clone()),
        ("gpg.format", "x509".into()),
        ("gpg.x509.program", "git-sign-nostr".into()),
        ("commit.gpgSign", "true".into()),
        ("tag.gpgSign", "true".into()),
        ("user.signingkey", info.pubkey_hex.clone()),
    ];

    // Compose with existing GIT_CONFIG_COUNT — don't clobber caller's config.
    let base: usize = std::env::var("GIT_CONFIG_COUNT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let mut env = Vec::with_capacity(entries.len() * 2 + 1);
    env.push((
        "GIT_CONFIG_COUNT".into(),
        (base + entries.len()).to_string(),
    ));
    for (i, (key, val)) in entries.iter().enumerate() {
        let idx = base + i;
        env.push((format!("GIT_CONFIG_KEY_{idx}"), key.to_string()));
        env.push((format!("GIT_CONFIG_VALUE_{idx}"), val.to_string()));
    }
    env
}

#[cfg(unix)]
fn set_owner_only(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o700);
    std::fs::set_permissions(path, perms)
}

#[cfg(not(unix))]
fn set_owner_only(_: &Path) -> std::io::Result<()> {
    Ok(())
}

// ---------------------------------------------------------------------------
// Multicall tool materialization
// ---------------------------------------------------------------------------

/// Names the multicall binary answers to. The first entry is the one that gets
/// the real bytes on Windows; the rest are hard links to it.
const TOOL_NAMES: [&str; 5] = [
    "buzz",
    "rg",
    "tree",
    "git-credential-nostr",
    "git-sign-nostr",
];

/// How a shim entry ended up on disk. Returned so the caller (and the tests)
/// can tell a 19 MB link from a 19 MB copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(unix, allow(dead_code))]
pub(crate) enum LinkKind {
    HardLink,
    Copy,
}

/// PATH lookup on Windows goes through PATHEXT, which only treats a `.exe` as
/// runnable. None of [`TOOL_NAMES`] contains a `.`, so this only ever appends.
#[cfg_attr(unix, allow(dead_code))]
fn exe_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(name).with_extension("exe")
}

#[cfg(unix)]
fn install_tools(self_exe: &Path, dir: &Path) -> std::io::Result<()> {
    // Symlinks cost one inode each here; there is nothing to optimise.
    for name in TOOL_NAMES {
        std::os::unix::fs::symlink(self_exe, dir.join(name))?;
    }
    Ok(())
}

/// Windows has no symlink without elevation, so every multicall name has to be
/// a real directory entry. This used to be five `std::fs::copy` calls, which
/// made each shim directory 5 x 19.3 MB = 96.5 MB (BUG-036: 25.6 GB leaked).
///
/// Copy the binary **once**, then hard-link the other four to that copy. The
/// link source is a file we just created in the *same directory*, so the
/// same-volume precondition holds by construction, and we are not linking a
/// running image (which can fail with a sharing violation). Result: 96.5 MB
/// becomes 19.3 MB, and `remove_dir_all` still reclaims all of it because the
/// last link in the directory is removed with the directory.
#[cfg(not(unix))]
fn install_tools(self_exe: &Path, dir: &Path) -> std::io::Result<()> {
    let (primary, rest) = TOOL_NAMES
        .split_first()
        .expect("TOOL_NAMES is a non-empty const array");
    let primary_path = exe_path(dir, primary);
    std::fs::copy(self_exe, &primary_path)?;
    for name in rest {
        link_or_copy(&primary_path, &exe_path(dir, name))?;
    }
    Ok(())
}

/// Hard-link `src` to `dst`, falling back to a full copy if the filesystem
/// cannot.
#[cfg_attr(unix, allow(dead_code))]
fn link_or_copy(src: &Path, dst: &Path) -> std::io::Result<LinkKind> {
    link_or_copy_with(src, dst, |s, d| std::fs::hard_link(s, d))
}

/// The body of [`link_or_copy`] with the linker injected, so the fallback can
/// be tested without needing a second volume or a FAT partition.
///
/// A hard link fails when `src` and `dst` are on different volumes, when the
/// filesystem has no hard-link support (FAT/exFAT, some network redirectors),
/// or when `src` has hit its per-file link limit (1023 on NTFS). None of those
/// is fatal — a copy is exactly what this code did before — but a silent
/// fallback would hide an 80% regression in disk use, so it is logged.
#[cfg_attr(unix, allow(dead_code))]
fn link_or_copy_with(
    src: &Path,
    dst: &Path,
    link: impl Fn(&Path, &Path) -> std::io::Result<()>,
) -> std::io::Result<LinkKind> {
    match link(src, dst) {
        Ok(()) => Ok(LinkKind::HardLink),
        Err(e) => {
            tracing::warn!(
                target: "shim",
                "hard link {} -> {} failed ({e}); falling back to a full copy \
                 (this shim directory will use ~5x the disk)",
                src.display(),
                dst.display(),
            );
            std::fs::copy(src, dst).map(|_| LinkKind::Copy)
        }
    }
}

// ---------------------------------------------------------------------------
// Temp directory naming — the attribution rule for the startup sweep
// ---------------------------------------------------------------------------

/// Prefix on every temp directory this crate creates.
const TEMP_PREFIX: &str = "buzz-dev-mcp-";
/// Extra segment on the shell's per-session workspace (see `shell::SharedState`).
const SESSION_INFIX: &str = "session-";
/// Introduces the owning PID. Directories created before BUG-036 have no such
/// segment, which is exactly how the sweep tells them apart.
const PID_MARKER: &str = "pid";

/// `buzz-dev-mcp-pid<PID>-` — `tempfile` appends its own random suffix.
pub(crate) fn shim_dir_prefix() -> String {
    format!("{TEMP_PREFIX}{PID_MARKER}{}-", std::process::id())
}

/// `buzz-dev-mcp-session-pid<PID>-` — same rule, for the shell workspace.
pub(crate) fn session_dir_prefix() -> String {
    format!(
        "{TEMP_PREFIX}{SESSION_INFIX}{PID_MARKER}{}-",
        std::process::id()
    )
}

/// The whole attribution rule, in one function: which process owns the
/// directory called `dir_name`, or `None` if we cannot say.
///
/// `None` is not a licence to delete. A directory we cannot attribute is left
/// alone — that covers every directory created before BUG-036 (`buzz-dev-mcp-`
/// plus six random alphanumerics and nothing else) as well as anything else
/// that happens to share our prefix.
///
/// The old and new schemes cannot be confused: `tempfile`'s random suffix is
/// alphanumeric with no `-`, so a pre-BUG-036 name has exactly one segment
/// after the prefix and can never satisfy the `pid<digits>-<rest>` shape here,
/// even in the pathological case where the random suffix starts with `pid`.
fn owning_pid(dir_name: &str) -> Option<u32> {
    let rest = dir_name.strip_prefix(TEMP_PREFIX)?;
    let rest = rest.strip_prefix(SESSION_INFIX).unwrap_or(rest);
    let digits = rest.strip_prefix(PID_MARKER)?;
    // The PID must be terminated by the separator before tempfile's random
    // suffix. A name that simply *ends* after the digits was not built here.
    let end = digits.find('-')?;
    let num = &digits[..end];
    if num.is_empty() || !num.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    num.parse().ok()
}

// ---------------------------------------------------------------------------
// Process liveness — "unknown is not dead"
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Liveness {
    Alive,
    Dead,
    /// The process may or may not exist and we are not permitted to find out.
    /// Treated exactly like `Alive` by the sweep.
    Unknown,
}

/// `kill(pid, 0)` is a pure permission probe — no signal is delivered.
/// `EPERM` means the process exists and belongs to somebody else, which is
/// alive, not dead.
#[cfg(unix)]
fn process_liveness(pid: u32) -> Liveness {
    use nix::errno::Errno;
    use nix::unistd::Pid;
    match nix::sys::signal::kill(Pid::from_raw(pid as i32), None) {
        Ok(()) => Liveness::Alive,
        Err(Errno::ESRCH) => Liveness::Dead,
        Err(Errno::EPERM) => Liveness::Alive,
        Err(_) => Liveness::Unknown,
    }
}

/// Windows equivalent, mirroring `managed_agents::runtime::process` but with a
/// third state: that probe folds "not permitted to ask" into `false`, which is
/// safe when the answer decides whether to send a kill and unsafe when it
/// decides whether to delete a directory.
///
/// `PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE` is the minimum that lets
/// us poll the process object — enough to ask, not enough to modify.
/// `ERROR_INVALID_PARAMETER` from `OpenProcess` is Windows for "no such PID";
/// every other failure (notably `ERROR_ACCESS_DENIED` on a protected process)
/// is `Unknown`.
#[cfg(windows)]
#[allow(unsafe_code)]
fn process_liveness(pid: u32) -> Liveness {
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_INVALID_PARAMETER, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // `windows-sys` only exposes SYNCHRONIZE as a FILE_ACCESS_RIGHTS constant,
    // which will not combine with PROCESS_ACCESS_RIGHTS. Same standard access
    // right for every object type.
    const SYNCHRONIZE: u32 = 0x0010_0000;

    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, pid);
        if handle.is_null() {
            return if GetLastError() == ERROR_INVALID_PARAMETER {
                Liveness::Dead
            } else {
                Liveness::Unknown
            };
        }
        // Zero timeout makes this a poll. Still signalled = still running.
        let alive = WaitForSingleObject(handle, 0) == WAIT_TIMEOUT;
        CloseHandle(handle);
        if alive {
            Liveness::Alive
        } else {
            Liveness::Dead
        }
    }
}

#[cfg(not(any(unix, windows)))]
fn process_liveness(_pid: u32) -> Liveness {
    // No probe available, so nothing is ever provably dead and nothing is ever
    // swept. Under-reclaiming is the safe failure.
    Liveness::Unknown
}

// ---------------------------------------------------------------------------
// Startup sweep
// ---------------------------------------------------------------------------

/// Wall-clock ceiling on the whole sweep. This is the bound that actually
/// matters: whatever the filesystem does, startup is delayed by at most this.
const SWEEP_TIME_BUDGET: Duration = Duration::from_secs(2);
/// Ceiling on directory entries *looked at* in the temp root. `%TEMP%` is
/// shared with every other program on the machine and can hold six figures of
/// unrelated entries.
const SWEEP_VISIT_LIMIT: usize = 10_000;
/// Ceiling on directories *removed* per startup. Each removal is a recursive
/// delete, so this is the expensive counter. A backlog is drained over
/// successive starts rather than in one stall.
const SWEEP_REMOVE_LIMIT: usize = 64;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SweepReport {
    /// Entries in the root that carried our prefix.
    pub matched: usize,
    /// Directories removed.
    pub removed: usize,
    /// Skipped because the owning PID is alive, or its liveness is unknown.
    pub kept_owned: usize,
    /// Skipped because no PID could be read out of the name.
    pub unattributable: usize,
    /// Read or remove errors. Logged, never propagated.
    pub failed: usize,
    /// One of the three bounds cut the sweep short.
    pub hit_limit: bool,
}

/// Remove shim and session directories whose owning process is gone.
///
/// Never returns an error: a housekeeping failure must not stop the app
/// starting. Nothing is swallowed — every skip and every failure is logged, and
/// the counts come back in the report.
pub(crate) fn sweep_stale_dirs(root: &Path) -> SweepReport {
    sweep_stale_dirs_with(root, process_liveness)
}

fn sweep_stale_dirs_with(root: &Path, liveness: impl Fn(u32) -> Liveness) -> SweepReport {
    let started = Instant::now();
    let mut report = SweepReport::default();

    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) => {
            tracing::warn!(
                target: "shim::sweep",
                "[GUARDRAIL] shim sweep could not read temp root {}: {e}; \
                 continuing without reclaiming disk",
                root.display(),
            );
            report.failed += 1;
            return report;
        }
    };

    let mut visited = 0usize;
    for entry in entries {
        visited += 1;
        if visited > SWEEP_VISIT_LIMIT
            || report.removed >= SWEEP_REMOVE_LIMIT
            || started.elapsed() >= SWEEP_TIME_BUDGET
        {
            report.hit_limit = true;
            break;
        }

        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                report.failed += 1;
                tracing::warn!(
                    target: "shim::sweep",
                    "[GUARDRAIL] shim sweep could not read an entry in {}: {e}",
                    root.display(),
                );
                continue;
            }
        };

        // Our names are ASCII by construction, so a non-UTF-8 name is not ours.
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with(TEMP_PREFIX) {
            continue;
        }
        report.matched += 1;

        // A plain file or a symlink wearing our prefix is not a shim directory.
        // `file_type` does not follow symlinks, which is what we want: we must
        // never recurse out of the temp root.
        match entry.file_type() {
            Ok(t) if t.is_dir() => {}
            Ok(_) => {
                report.unattributable += 1;
                tracing::debug!(
                    target: "shim::sweep",
                    "leaving non-directory {name} alone",
                );
                continue;
            }
            Err(e) => {
                report.failed += 1;
                tracing::warn!(
                    target: "shim::sweep",
                    "[GUARDRAIL] shim sweep could not stat {name}: {e}; leaving it alone",
                );
                continue;
            }
        }

        let Some(pid) = owning_pid(name) else {
            report.unattributable += 1;
            tracing::debug!(
                target: "shim::sweep",
                "leaving {name} alone: no owning PID in the name",
            );
            continue;
        };

        match liveness(pid) {
            Liveness::Dead => {}
            Liveness::Alive => {
                report.kept_owned += 1;
                continue;
            }
            Liveness::Unknown => {
                report.kept_owned += 1;
                tracing::debug!(
                    target: "shim::sweep",
                    "leaving {name} alone: liveness of pid {pid} is unknown",
                );
                continue;
            }
        }

        match std::fs::remove_dir_all(entry.path()) {
            Ok(()) => report.removed += 1,
            Err(e) => {
                report.failed += 1;
                tracing::warn!(
                    target: "shim::sweep",
                    "[GUARDRAIL] shim sweep failed to remove {} (owner pid {pid} is gone): {e}",
                    entry.path().display(),
                );
            }
        }
    }

    if report.failed > 0 || report.unattributable > 0 || report.hit_limit {
        tracing::warn!(
            target: "shim::sweep",
            "[GUARDRAIL] shim sweep incomplete: matched={} removed={} kept_owned={} \
             unattributable={} failed={} hit_limit={}",
            report.matched,
            report.removed,
            report.kept_owned,
            report.unattributable,
            report.failed,
            report.hit_limit,
        );
    } else if report.matched > 0 {
        tracing::info!(
            target: "shim::sweep",
            "shim sweep: matched={} removed={} kept_owned={}",
            report.matched,
            report.removed,
            report.kept_owned,
        );
    }

    report
}

pub fn artifact_dir(session_root: &Path) -> PathBuf {
    let p = session_root.join("artifacts");
    let _ = std::fs::create_dir_all(&p);
    p
}

/// Tests for the BUG-036 disk fixes: one file behind five names, and a startup
/// sweep that reclaims only what it can prove is garbage.
///
/// Nothing here calls [`Shim::install`], so nothing here touches the real
/// `%TEMP%`. Every test builds its own root.
#[cfg(test)]
mod disk_tests {
    use super::{
        link_or_copy, link_or_copy_with, owning_pid, process_liveness, session_dir_prefix,
        shim_dir_prefix, sweep_stale_dirs_with, LinkKind, Liveness, SweepReport,
        SWEEP_REMOVE_LIMIT,
    };
    use std::path::Path;

    /// Do these two paths name the same bytes on disk, or two copies of them?
    ///
    /// Asked behaviourally — write through `a`, look through `b`, put `a` back
    /// — rather than by comparing inode numbers, because Windows only exposes
    /// its file index behind the unstable `windows_by_handle` feature. This is
    /// also the more direct statement of the property under test: the point of
    /// the hard link is that there is only one copy of the bytes.
    fn shares_storage(a: &Path, b: &Path) -> bool {
        let original = std::fs::read(a).expect("read a");
        assert!(!original.is_empty(), "probe needs a non-empty file");
        let mut probe = original.clone();
        probe[0] ^= 0xFF;
        std::fs::write(a, &probe).expect("write a");
        let seen = std::fs::read(b).expect("read b");
        std::fs::write(a, &original).expect("restore a");
        seen == probe
    }

    fn always(state: Liveness) -> impl Fn(u32) -> Liveness {
        move |_| state
    }

    // -- naming / attribution -------------------------------------------------

    #[test]
    fn test_owning_pid_reads_the_pid_out_of_both_directory_shapes() {
        assert_eq!(owning_pid("buzz-dev-mcp-pid4242-AbC123"), Some(4242));
        assert_eq!(
            owning_pid("buzz-dev-mcp-session-pid4242-AbC123"),
            Some(4242)
        );
    }

    #[test]
    fn test_owning_pid_round_trips_the_prefixes_we_actually_create() {
        // The builders and the parser must agree, or the sweep silently never
        // reclaims anything.
        let me = std::process::id();
        for prefix in [shim_dir_prefix(), session_dir_prefix()] {
            let name = format!("{prefix}0BNI9I");
            assert_eq!(
                owning_pid(&name),
                Some(me),
                "{name} must attribute back to this process"
            );
        }
    }

    #[test]
    fn test_owning_pid_rejects_the_pre_bug_036_naming_scheme() {
        // These are the 281 directories already on the operator's disk. They
        // carry no owner, so they must never be attributed — and therefore
        // never swept.
        for name in [
            "buzz-dev-mcp-0BNI9I",
            "buzz-dev-mcp-session-0BNI9I",
            "buzz-dev-mcp-",
            "buzz-dev-mcp-session-",
        ] {
            assert_eq!(owning_pid(name), None, "{name} must be unattributable");
        }
    }

    #[test]
    fn test_owning_pid_rejects_names_that_only_look_like_ours() {
        for name in [
            // Not our prefix at all.
            "buzz-dev-mcpX-pid1-a",
            "some-other-tool-pid1-a",
            // `pid` present but no digits, or digits that are not digits.
            "buzz-dev-mcp-pid-a",
            "buzz-dev-mcp-pid12x4-a",
            "buzz-dev-mcp-pid 12-a",
            // Digits not terminated by the random-suffix separator. A six-char
            // tempfile suffix could theoretically be "pid123", and this is what
            // stops that being read as an owner.
            "buzz-dev-mcp-pid123",
        ] {
            assert_eq!(owning_pid(name), None, "{name} must be unattributable");
        }
    }

    // -- liveness -------------------------------------------------------------

    #[test]
    fn test_process_liveness_knows_itself_alive_and_an_impossible_pid_dead() {
        assert_eq!(process_liveness(std::process::id()), Liveness::Alive);
        // Above every platform's pid_max, and not a multiple of 4 so it can
        // never be a Windows PID either. Positive, so the Unix probe cannot be
        // mistaken for a process-group signal.
        assert_eq!(process_liveness(i32::MAX as u32), Liveness::Dead);
    }

    // -- hard linking ---------------------------------------------------------

    #[test]
    fn test_link_or_copy_hard_links_when_the_filesystem_allows_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = dir.path().join("src.bin");
        std::fs::write(&src, b"multicall").expect("write");
        let dst = dir.path().join("dst.bin");

        assert_eq!(link_or_copy(&src, &dst).expect("link"), LinkKind::HardLink);
        assert_eq!(std::fs::read(&dst).expect("read"), b"multicall");
        assert!(
            shares_storage(&src, &dst),
            "a hard link must be the same file on disk, not a second copy"
        );
    }

    #[test]
    fn test_link_or_copy_falls_back_to_a_full_copy_when_hard_link_fails() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = dir.path().join("src.bin");
        std::fs::write(&src, b"multicall").expect("write");
        let dst = dir.path().join("dst.bin");

        // Stands in for a different volume, a FAT partition, or NTFS's
        // 1023-link ceiling. The install must still produce a working tool.
        let refuse = |_: &Path, _: &Path| {
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "no hard links here",
            ))
        };
        assert_eq!(
            link_or_copy_with(&src, &dst, refuse).expect("fallback"),
            LinkKind::Copy
        );
        assert_eq!(std::fs::read(&dst).expect("read"), b"multicall");
        assert!(
            !shares_storage(&src, &dst),
            "the fallback must produce a real, independent copy"
        );
    }

    #[cfg(not(unix))]
    #[test]
    fn test_shim_tools_are_five_names_over_one_file_on_disk() {
        // The BUG-036 amplifier: this directory used to be 5 x 19.3 MB.
        let dir = tempfile::tempdir().expect("tempdir");
        let src = dir.path().join("multicall.exe");
        std::fs::write(&src, vec![0xAAu8; 4096]).expect("write");

        let out = tempfile::tempdir().expect("tempdir");
        super::install_tools(&src, out.path()).expect("install tools");

        let paths: Vec<_> = super::TOOL_NAMES
            .iter()
            .map(|name| super::exe_path(out.path(), name))
            .collect();
        for p in &paths {
            assert!(p.is_file(), "{} must exist", p.display());
            assert_eq!(std::fs::metadata(p).expect("stat").len(), 4096);
        }
        // Every name must resolve to the same bytes on disk as the first, so
        // the directory costs one binary rather than five.
        for p in &paths[1..] {
            assert!(
                shares_storage(&paths[0], p),
                "{} must share storage with {}, not be a second copy",
                p.display(),
                paths[0].display()
            );
        }
    }

    #[test]
    fn test_a_hard_linked_executable_actually_runs() {
        // Tested, not assumed. Windows opens a running image with
        // FILE_SHARE_READ | FILE_SHARE_DELETE and it is not obvious from the
        // docs that a hard link to a PE image is loadable. The source is a copy
        // of this test binary — a real executable on both platforms.
        let self_exe = std::env::current_exe().expect("current exe");
        let dir = tempfile::tempdir().expect("tempdir");
        let src = dir.path().join("multicall.exe");
        std::fs::copy(&self_exe, &src).expect("copy self");
        let linked = dir.path().join("linked.exe");

        // A `Copy` here would make the execution below prove nothing, so the
        // kind is asserted before the process is spawned.
        assert_eq!(
            link_or_copy(&src, &linked).expect("link"),
            LinkKind::HardLink,
            "this test only means something if the entry is a hard link"
        );

        // `--list` makes libtest enumerate and exit 0 without running anything.
        let run = std::process::Command::new(&linked)
            .arg("--list")
            .output()
            .unwrap_or_else(|e| panic!("spawning hard-linked {} failed: {e}", linked.display()));
        assert!(
            run.status.success(),
            "hard-linked executable exited with {:?}\nstderr: {}",
            run.status,
            String::from_utf8_lossy(&run.stderr)
        );
    }

    // -- sweep ----------------------------------------------------------------

    /// Create `name/` under `root` with one file in it, so a removal has to do
    /// real recursive work.
    fn seed(root: &Path, name: &str) -> std::path::PathBuf {
        let d = root.join(name);
        std::fs::create_dir_all(&d).expect("create dir");
        std::fs::write(d.join("buzz.exe"), b"x").expect("write");
        d
    }

    #[test]
    fn test_sweep_removes_a_directory_whose_owner_is_dead() {
        let root = tempfile::tempdir().expect("tempdir");
        let shim = seed(root.path(), "buzz-dev-mcp-pid700-AAAAAA");
        let session = seed(root.path(), "buzz-dev-mcp-session-pid700-BBBBBB");

        let report = sweep_stale_dirs_with(root.path(), always(Liveness::Dead));

        assert!(!shim.exists(), "dead owner's shim dir must be reclaimed");
        assert!(!session.exists(), "dead owner's session dir must be reclaimed");
        assert_eq!(
            report,
            SweepReport {
                matched: 2,
                removed: 2,
                ..SweepReport::default()
            }
        );
    }

    #[test]
    fn test_sweep_never_removes_a_live_owners_directory() {
        let root = tempfile::tempdir().expect("tempdir");
        let live = seed(root.path(), "buzz-dev-mcp-pid700-AAAAAA");

        let report = sweep_stale_dirs_with(root.path(), always(Liveness::Alive));

        assert!(live.exists(), "a live owner's directory must survive");
        assert_eq!(report.removed, 0);
        assert_eq!(report.kept_owned, 1);
    }

    #[test]
    fn test_sweep_treats_unknown_liveness_as_not_dead() {
        // The whole project's discipline: we delete on proof of death, never on
        // absence of proof of life.
        let root = tempfile::tempdir().expect("tempdir");
        let d = seed(root.path(), "buzz-dev-mcp-pid700-AAAAAA");

        let report = sweep_stale_dirs_with(root.path(), always(Liveness::Unknown));

        assert!(d.exists(), "unknown liveness must not authorise a delete");
        assert_eq!(report.removed, 0);
        assert_eq!(report.kept_owned, 1);
    }

    #[test]
    fn test_sweep_leaves_unattributable_directories_alone() {
        // Every one of the 281 directories already on the operator's disk looks
        // like this. The liveness probe says "dead" for everything here, so the
        // ONLY thing standing between these and deletion is the attribution
        // rule.
        let root = tempfile::tempdir().expect("tempdir");
        let old_shim = seed(root.path(), "buzz-dev-mcp-0BNI9I");
        let old_session = seed(root.path(), "buzz-dev-mcp-session-0BNI9I");

        let report = sweep_stale_dirs_with(root.path(), always(Liveness::Dead));

        assert!(old_shim.exists(), "pre-BUG-036 shim dir must be left alone");
        assert!(
            old_session.exists(),
            "pre-BUG-036 session dir must be left alone"
        );
        assert_eq!(report.removed, 0);
        assert_eq!(report.matched, 2);
        assert_eq!(report.unattributable, 2);
    }

    #[test]
    fn test_sweep_ignores_anything_not_carrying_our_prefix() {
        let root = tempfile::tempdir().expect("tempdir");
        let foreign = seed(root.path(), "some-other-tool-pid700-AAAAAA");
        let nearly = seed(root.path(), "buzz-dev-mcpX-pid700-AAAAAA");

        let report = sweep_stale_dirs_with(root.path(), always(Liveness::Dead));

        assert!(foreign.exists());
        assert!(nearly.exists());
        assert_eq!(report.matched, 0);
        assert_eq!(report.removed, 0);
    }

    #[test]
    fn test_sweep_stops_at_the_removal_bound_and_says_so() {
        // A backlog must not stall the launch: it is drained across starts.
        let root = tempfile::tempdir().expect("tempdir");
        let seeded = SWEEP_REMOVE_LIMIT + 20;
        for i in 0..seeded {
            seed(root.path(), &format!("buzz-dev-mcp-pid{}-AAAAAA", 1000 + i));
        }

        let report = sweep_stale_dirs_with(root.path(), always(Liveness::Dead));

        assert_eq!(
            report.removed, SWEEP_REMOVE_LIMIT,
            "the sweep must remove no more than its bound in one startup"
        );
        assert!(report.hit_limit, "hitting the bound must be reported");
        let left = std::fs::read_dir(root.path()).expect("read").count();
        assert_eq!(left, seeded - SWEEP_REMOVE_LIMIT, "the rest waits for the next start");
    }

    #[test]
    fn test_sweep_reports_a_failure_instead_of_propagating_it() {
        // An unreadable temp root must not be able to stop the app starting.
        let root = tempfile::tempdir().expect("tempdir");
        let missing = root.path().join("definitely-not-here");

        let report = sweep_stale_dirs_with(&missing, always(Liveness::Dead));

        assert_eq!(report.failed, 1);
        assert_eq!(report.removed, 0);
    }
}

#[cfg(test)]
mod git_user_name_tests {
    use super::{
        build_git_env, is_git_crud, is_unicode_format, sanitize_git_user_name, KeyInfo,
        MAX_GIT_USER_NAME_CHARS,
    };
    use std::sync::Mutex;

    /// Env-var-touching tests must run serially — env vars are process-global.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    const PUBKEY_HEX: &str = "dcfd242e557282d7a1e2cf2e6877522682f1e5c6156dc92ca7d90eaedd3b0f95";
    const NPUB: &str = "npub1mn7jgtj4w2pd0g0zeuhxsa6jy6p0rewxz4kujt98my82ahfmp72sxjexk7";

    fn key_info() -> KeyInfo {
        KeyInfo {
            keyfile_path: "/tmp/.nostr-key".into(),
            pubkey_hex: PUBKEY_HEX.into(),
            npub: NPUB.into(),
        }
    }

    /// Read a git config value back out of the flat GIT_CONFIG_KEY_n/VALUE_n pairs.
    fn git_config(env: &[(String, String)], key: &str) -> Option<String> {
        let idx = env
            .iter()
            .find(|(k, v)| k.starts_with("GIT_CONFIG_KEY_") && v == key)?
            .0
            .strip_prefix("GIT_CONFIG_KEY_")?
            .to_owned();
        env.iter()
            .find(|(k, _)| *k == format!("GIT_CONFIG_VALUE_{idx}"))
            .map(|(_, v)| v.clone())
    }

    #[test]
    fn test_ordinary_name_passes_through_unchanged() {
        assert_eq!(sanitize_git_user_name("Duncan"), Some("Duncan".into()));
    }

    #[test]
    fn test_angle_brackets_are_stripped_so_no_second_email_is_rendered() {
        // git drops the brackets itself and renders `Duncan evil@x.com
        // <hex@relay>` — no forgery, but a confusing author line.
        assert_eq!(
            sanitize_git_user_name("Duncan <evil@x.com>"),
            Some("Duncan evil@x.com".into())
        );
    }

    #[test]
    fn test_whitespace_control_characters_become_a_single_separator() {
        // Newline, tab and carriage return are whitespace: they collapse to one
        // space like any other run, so a multi-line name stays readable.
        assert_eq!(
            sanitize_git_user_name("Dun\ncan\tThe\r\nIdaho"),
            Some("Dun can The Idaho".into())
        );
    }

    #[test]
    fn test_non_whitespace_control_characters_are_dropped_outright() {
        // NUL is the important one: an interior NUL makes `Command::env` fail
        // the entire spawn upstream, so it must never survive to git config.
        let got = sanitize_git_user_name("Idaho\0Blade\u{7}").expect("non-empty");
        assert_eq!(got, "IdahoBlade");
        assert!(!got.chars().any(char::is_control));
    }

    #[test]
    fn test_internal_whitespace_runs_collapse_to_one_space() {
        assert_eq!(
            sanitize_git_user_name("  Duncan   Idaho  "),
            Some("Duncan Idaho".into())
        );
    }

    #[test]
    fn test_whitespace_only_name_falls_back_to_npub() {
        assert_eq!(sanitize_git_user_name("   \t\n  "), None);
    }

    #[test]
    fn test_empty_name_falls_back_to_npub() {
        assert_eq!(sanitize_git_user_name(""), None);
    }

    #[test]
    fn test_crud_only_name_falls_back_rather_than_aborting_every_commit() {
        // git rejects a name built only of crud with `fatal: name consists
        // only of disallowed characters`, which would break EVERY commit the
        // agent makes. Verified against git 2.54.0.
        for raw in ["<>", ";;", "\"\"", "''", ",", ":", "\\", ",;:"] {
            assert_eq!(
                sanitize_git_user_name(raw),
                None,
                "crud-only name {raw:?} must fall back to the npub"
            );
        }
    }

    #[test]
    fn test_crud_mixed_with_real_characters_is_kept() {
        // Legitimate names contain crud; only an all-crud result is fatal.
        assert_eq!(sanitize_git_user_name("O'Brien"), Some("O'Brien".into()));
        assert_eq!(
            sanitize_git_user_name("Smith, Jr."),
            Some("Smith, Jr.".into())
        );
    }

    #[test]
    fn test_over_length_name_is_truncated_to_the_cap() {
        let long = "a".repeat(200);
        let got = sanitize_git_user_name(&long).expect("non-empty");
        assert_eq!(got.chars().count(), MAX_GIT_USER_NAME_CHARS);
    }

    #[test]
    fn test_truncation_never_splits_a_multibyte_character() {
        let long = "🐝".repeat(200);
        let got = sanitize_git_user_name(&long).expect("non-empty");
        assert_eq!(got.chars().count(), MAX_GIT_USER_NAME_CHARS);
        assert!(got.chars().all(|c| c == '🐝'), "no replacement chars");
    }

    #[test]
    fn test_truncation_does_not_leave_a_trailing_space() {
        // Cutting mid-word would otherwise strand the separator at the end.
        let raw = format!("{} tail", "a".repeat(MAX_GIT_USER_NAME_CHARS - 1));
        let got = sanitize_git_user_name(&raw).expect("non-empty");
        assert!(!got.ends_with(' '), "got {got:?}");
    }

    #[test]
    fn test_non_ascii_names_survive() {
        assert_eq!(
            sanitize_git_user_name("Élodie 🐝"),
            Some("Élodie 🐝".into())
        );
    }

    #[test]
    fn test_format_only_name_falls_back_to_npub() {
        // U+200B is neither control, nor whitespace, nor crud, so before Cf
        // filtering this passed the non-crud gate and handed git a visually
        // blank author instead of falling back.
        assert_eq!(sanitize_git_user_name("\u{200B}\u{200B}"), None);
        // Same class, different marks: joiner, word joiner, BOM, bidi override.
        for raw in ["\u{200D}", "\u{2060}", "\u{FEFF}", "\u{202E}", "\u{00AD}"] {
            assert_eq!(
                sanitize_git_user_name(raw),
                None,
                "format-only name {raw:?} must fall back to the npub"
            );
        }
    }

    #[test]
    fn test_bidi_override_is_stripped_and_the_name_is_kept() {
        // A trailing RLO would reorder everything after it in `git log`, so the
        // mark goes and the readable name stays.
        assert_eq!(
            sanitize_git_user_name("Duncan\u{202E}"),
            Some("Duncan".into())
        );
        assert_eq!(
            sanitize_git_user_name("Dun\u{202E}can Idaho"),
            Some("Duncan Idaho".into())
        );
    }

    #[test]
    fn test_zero_width_space_inside_a_word_is_removed_without_splitting_it() {
        // U+200B is not whitespace, so it must not become a separator: the word
        // rejoins rather than turning into "Dun can".
        assert_eq!(
            sanitize_git_user_name("Dun\u{200B}can"),
            Some("Duncan".into())
        );
    }

    #[test]
    fn test_format_characters_do_not_consume_the_length_budget() {
        // Filtering happens before truncation, so invisible padding cannot
        // shorten the visible name.
        let raw = format!("{}{}", "\u{200B}".repeat(200), "a".repeat(90));
        let got = sanitize_git_user_name(&raw).expect("non-empty");
        assert_eq!(got.chars().count(), MAX_GIT_USER_NAME_CHARS);
        assert!(got.chars().all(|c| c == 'a'), "got {got:?}");
    }

    #[test]
    fn test_unicode_format_covers_every_cf_range_and_nothing_adjacent() {
        // Both endpoints of each of the 21 `Cf` ranges in UCD 17.0.0. Endpoints
        // are what a transcription error moves, so they are what gets asserted.
        for c in [
            '\u{00AD}',
            '\u{0600}',
            '\u{0605}',
            '\u{061C}',
            '\u{06DD}',
            '\u{070F}',
            '\u{0890}',
            '\u{0891}',
            '\u{08E2}',
            '\u{180E}',
            '\u{200B}',
            '\u{200F}',
            '\u{202A}',
            '\u{202E}',
            '\u{2060}',
            '\u{2064}',
            '\u{2066}',
            '\u{206F}',
            '\u{FEFF}',
            '\u{FFF9}',
            '\u{FFFB}',
            '\u{110BD}',
            '\u{110CD}',
            '\u{13430}',
            '\u{1343F}',
            '\u{1BCA0}',
            '\u{1BCA3}',
            '\u{1D173}',
            '\u{1D17A}',
            '\u{E0001}',
            '\u{E0020}',
            '\u{E007F}',
        ] {
            assert!(is_unicode_format(c), "U+{:04X} is Cf", c as u32);
        }
        // Codepoints immediately outside those ranges, plus ordinary characters.
        // U+2065 is the notable one: it sits *inside* the 2060..206F block but
        // is unassigned, not `Cf`.
        for c in [
            '\u{00AC}',
            '\u{00AE}',
            '\u{05FF}',
            '\u{0606}',
            '\u{061B}',
            '\u{061D}',
            '\u{200A}',
            '\u{2010}',
            '\u{2029}',
            '\u{202F}',
            '\u{2065}',
            '\u{205F}',
            '\u{2070}',
            '\u{FEFE}',
            '\u{FFF8}',
            '\u{FFFC}',
            '\u{110BC}',
            '\u{1342F}',
            '\u{E0000}',
            '\u{E0080}',
            'a',
            ' ',
            '🐝',
            'É',
        ] {
            assert!(!is_unicode_format(c), "U+{:04X} is not Cf", c as u32);
        }
    }

    #[test]
    fn test_build_git_env_uses_display_name_and_leaves_email_on_the_pubkey() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("BUZZ_ACP_DISPLAY_NAME", "Duncan");
        std::env::remove_var("BUZZ_RELAY_URL");
        std::env::remove_var("GIT_CONFIG_COUNT");
        let env = build_git_env(&key_info());
        std::env::remove_var("BUZZ_ACP_DISPLAY_NAME");

        assert_eq!(git_config(&env, "user.name").as_deref(), Some("Duncan"));
        // The pubkey — the thing NIP-98 auth, NIP-GS signing, and contributor
        // matching key on — must stay in the email untouched.
        assert_eq!(
            git_config(&env, "user.email").as_deref(),
            Some(format!("{PUBKEY_HEX}@buzz").as_str())
        );
        assert_eq!(
            git_config(&env, "user.signingkey").as_deref(),
            Some(PUBKEY_HEX)
        );
    }

    #[test]
    fn test_build_git_env_falls_back_to_npub_when_display_name_unset() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("BUZZ_ACP_DISPLAY_NAME");
        std::env::remove_var("BUZZ_RELAY_URL");
        std::env::remove_var("GIT_CONFIG_COUNT");
        let env = build_git_env(&key_info());

        // Today's behavior, and what every agent gets until a writer for
        // BUZZ_ACP_DISPLAY_NAME lands on the Desktop side.
        assert_eq!(git_config(&env, "user.name").as_deref(), Some(NPUB));
        assert_eq!(
            git_config(&env, "user.email").as_deref(),
            Some(format!("{PUBKEY_HEX}@buzz").as_str())
        );
    }

    #[test]
    fn test_build_git_env_falls_back_to_npub_when_display_name_is_unusable() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("BUZZ_RELAY_URL");
        std::env::remove_var("GIT_CONFIG_COUNT");

        // Crud-only and format-only names both reach git as the npub — one
        // would abort every commit, the other would render as blank.
        for raw in ["<>", "\u{200B}"] {
            std::env::set_var("BUZZ_ACP_DISPLAY_NAME", raw);
            let env = build_git_env(&key_info());
            assert_eq!(
                git_config(&env, "user.name").as_deref(),
                Some(NPUB),
                "unusable display name {raw:?} must reach git as the npub"
            );
        }
        std::env::remove_var("BUZZ_ACP_DISPLAY_NAME");
    }

    #[test]
    fn test_git_crud_set_matches_observed_git_behavior() {
        // Empirically derived from git 2.54.0: these bytes, alone, abort a commit.
        for c in [' ', '"', '\'', ',', ':', ';', '<', '>', '\\', '\t', '\n'] {
            assert!(is_git_crud(c), "{c:?} should be crud");
        }
        for c in ['.', '-', '_', '@', '(', 'a', '🐝'] {
            assert!(!is_git_crud(c), "{c:?} should not be crud");
        }
    }
}
