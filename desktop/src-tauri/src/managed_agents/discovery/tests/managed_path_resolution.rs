use crate::managed_agents::discovery::{clear_resolve_cache, resolve_command};

/// Regression guard for BUG-070.
///
/// `resolve_command` memoises its answers process-globally, but the answer is a
/// function of `PATH`. Before the fix the memo survived a `PATH` change, so the
/// first caller to resolve a name pinned that answer for the life of the
/// process — in production across the app's own managed-Node/npm `PATH`
/// mutations, and in the test suite as an order-dependent failure where any
/// earlier `resolve_command("claude")` (e.g. via `discover_acp_runtimes_from`)
/// made `claude_spawn_uses_the_probed_cli_executable` fail.
///
/// This test never calls `clear_resolve_cache`: invalidation must be automatic.
#[test]
fn resolve_command_cache_is_invalidated_by_a_path_change() {
    let _guard = crate::managed_agents::lock_path_mutex();

    let name = format!("buzz-cache-gen-probe{}", std::env::consts::EXE_SUFFIX);
    let first = tempfile::tempdir().expect("first tempdir");
    let second = tempfile::tempdir().expect("second tempdir");
    let first_bin = first.path().join(&name);
    let second_bin = second.path().join(&name);

    for bin in [&first_bin, &second_bin] {
        std::fs::write(bin, "").expect("write probe binary");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(bin, std::fs::Permissions::from_mode(0o755))
                .expect("chmod probe binary");
        }
    }

    let original_path = std::env::var_os("PATH");

    std::env::set_var("PATH", first.path());
    let resolved_first = resolve_command("buzz-cache-gen-probe");

    // No cache clear here — the PATH change alone must invalidate the memo.
    std::env::set_var("PATH", second.path());
    let resolved_second = resolve_command("buzz-cache-gen-probe");

    match original_path {
        Some(path) => std::env::set_var("PATH", path),
        None => std::env::remove_var("PATH"),
    }
    clear_resolve_cache();

    assert_eq!(
        resolved_first.as_deref(),
        Some(first_bin.as_path()),
        "first resolution must come from the first PATH entry"
    );
    assert_eq!(
        resolved_second.as_deref(),
        Some(second_bin.as_path()),
        "a PATH change must invalidate the resolve cache without an explicit clear"
    );
}

/// The legacy Goose Windows installer wrote `%USERPROFILE%\goose\goose.exe`,
/// a directory on no standard PATH. `resolve_command_uncached` finds binaries
/// outside PATH only by scanning `common_binary_paths()`, so that directory
/// must appear there or those installs stay undiscovered (#2239 residual).
///
/// Asserts the probe list rather than a planted binary: `common_binary_paths`
/// is a process-lifetime `OnceLock`, so a test cannot re-seed `USERPROFILE`
/// deterministically, and planting an executable under the real user profile
/// is not an acceptable test side effect.
#[cfg(windows)]
#[test]
fn common_binary_paths_probes_legacy_goose_install_dir() {
    use std::path::PathBuf;

    let profile = std::env::var_os("USERPROFILE").expect("USERPROFILE is always set on Windows");
    let legacy_dir = PathBuf::from(profile).join("goose");

    let probed = super::super::common_binary_paths();

    assert!(
        probed.contains(&legacy_dir),
        "legacy Goose install dir {} must be probed, got: {probed:?}",
        legacy_dir.display()
    );
}

#[cfg(unix)]
#[test]
fn resolve_command_prefers_buzz_managed_npm_shim_over_path() {
    use std::os::unix::fs::PermissionsExt;

    let _guard = crate::managed_agents::lock_path_mutex();
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let xdg_data = temp.path().join("xdg-data");
    let global_bin = temp.path().join("global-bin");
    std::fs::create_dir_all(&home).expect("create home");
    std::fs::create_dir_all(&xdg_data).expect("create xdg data");
    std::fs::create_dir_all(&global_bin).expect("create global bin");

    let old_home = std::env::var_os("HOME");
    let old_xdg_data = std::env::var_os("XDG_DATA_HOME");
    let old_path = std::env::var_os("PATH").unwrap_or_default();

    std::env::set_var("HOME", &home);
    std::env::set_var("XDG_DATA_HOME", &xdg_data);
    let managed_bin = dirs::data_dir()
        .expect("data dir")
        .join("Buzz")
        .join("node-tools")
        .join("bin");
    std::fs::create_dir_all(&managed_bin).expect("create managed bin");

    let managed_shim = managed_bin.join("codex-acp");
    let global_shim = global_bin.join("codex-acp");
    std::fs::write(&managed_shim, "#!/bin/sh\necho managed\n").expect("write managed shim");
    std::fs::write(&global_shim, "#!/bin/sh\necho global\n").expect("write global shim");
    std::fs::set_permissions(&managed_shim, std::fs::Permissions::from_mode(0o755))
        .expect("chmod managed shim");
    std::fs::set_permissions(&global_shim, std::fs::Permissions::from_mode(0o755))
        .expect("chmod global shim");

    let new_path = std::env::join_paths(
        std::iter::once(global_bin.clone()).chain(std::env::split_paths(&old_path)),
    )
    .expect("join PATH");
    std::env::set_var("PATH", new_path);
    clear_resolve_cache();

    let resolved = resolve_command("codex-acp");

    std::env::set_var("PATH", &old_path);
    match old_home {
        Some(value) => std::env::set_var("HOME", value),
        None => std::env::remove_var("HOME"),
    }
    match old_xdg_data {
        Some(value) => std::env::set_var("XDG_DATA_HOME", value),
        None => std::env::remove_var("XDG_DATA_HOME"),
    }
    clear_resolve_cache();

    assert_eq!(
        resolved.as_deref(),
        Some(managed_shim.as_path()),
        "Buzz-managed npm shim must win over PATH/global shims"
    );
}
