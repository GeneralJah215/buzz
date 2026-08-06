# Intent

Add a bounded desktop "black box" so a future silent freeze or process death leaves durable evidence under the Tauri application-data directory. The Rust process will persist INFO-and-higher diagnostics, panic details with a forced backtrace, and lifecycle markers. The frontend will forward uncaught window errors and unhandled promise rejections without allowing an error loop to grow the log without bound.

# Files to touch

- `desktop/src-tauri/Cargo.toml` and `desktop/src-tauri/Cargo.lock`: use the already-locked `tracing-subscriber` crate directly.
- `desktop/src-tauri/src/desktop_logging.rs`: rotating file writer, panic hook, frontend-report command, throttling, and Rust tests.
- `desktop/src-tauri/src/lib.rs`: install logging before Tauri startup, register the command, and emit lifecycle markers.
- `desktop/src-tauri/src/shutdown.rs`: record shutdown initiation for signal and normal cleanup paths.
- `desktop/src/shared/lib/desktopErrorLogging.ts`: install frontend global error/rejection listeners and normalize bounded reports.
- `desktop/src/shared/lib/desktopErrorLogging.test.mjs`: unit coverage for normalization and truncation.
- `desktop/src/main.tsx`: install listeners before application bootstrap.
- `RESUME.md`: crash-safe lane state; not part of the product commit.

# Behavior contract

- On startup, create `<app_data_dir>/logs/buzz-desktop.log`; for the release identifier on Windows this is `%APPDATA%/xyz.block.buzz.app/logs/buzz-desktop.log`.
- Rust `tracing` events at INFO, WARN, and ERROR are written synchronously to the file with timestamps and no ANSI escapes.
- Rotate before the active file would exceed 2 MiB and retain at most four 2 MiB backups, bounding the complete desktop log set at approximately 10 MiB.
- Install a panic hook before Tauri builds. It appends the panic payload, source location, and a forced backtrace directly to the active log and flushes before delegating to the prior hook.
- Emit markers for process startup including package version and identifier, main-window creation, shutdown request, cleanup start, and final exit. An abnormal death is distinguishable by the absence of the shutdown/exit sequence.
- Register a `report_frontend_error` Tauri command. It accepts only bounded diagnostic fields, emits accepted reports at ERROR, and accepts at most 20 frontend reports per rolling 60-second window. The first rejected report emits one suppression warning; further reports in that window are silent.
- Install `window.error` and `unhandledrejection` listeners before frontend bootstrap. Forwarding is best-effort and must never surface a toast, create a new unhandled rejection, or block rendering.
- If the logging directory or subscriber cannot be initialized, print one diagnostic to stderr and continue launching so diagnostics cannot brick the desktop.
- Do not modify thread-directory or sidebar files owned by the parallel fallback lane.

# Test list

- Rust: rotation preserves newest data, prunes the oldest backup, and keeps the configured total bound.
- Rust: throttle allows the configured budget, emits one suppression decision, suppresses the remainder, and resets after the window.
- Rust: frontend fields are truncated at their byte budgets without breaking UTF-8.
- Frontend: Error objects and arbitrary rejection values normalize to bounded message/stack fields.
- Frontend: oversized fields are truncated.
- Run the complete desktop Rust workspace tests, complete desktop frontend unit suite, frontend build/typecheck/lint/file ratchets, and repository `just ci` gate.

# Safety implications

- Logs may contain frontend exception text and stacks, so report fields are length-bounded and no application state, credentials, message bodies, or environment variables are intentionally captured.
- Synchronous file writes occur only for INFO-and-higher diagnostics and use a mutex; the frontend ingress is rate-limited on the Rust side so a compromised or looping renderer cannot flood disk.
- Rotation uses fixed filenames inside the resolved application logs directory only; it never walks or deletes outside that directory.
- Logging failure is fail-open for availability, while panic reporting remains best-effort and calls the existing panic hook.

# Out of scope / do NOT touch

- `desktop/src/shared/api/threadDirectory.ts`, `desktop/src/features/sidebar/useThreadDirectory.ts`, and `desktop/src/features/sidebar/ui/SidebarThreadList.tsx` or their tests.
- Relay/server logging, agent-harness logs, telemetry upload, crash-report upload, user-visible diagnostics UI, balance/config constants, and Rust relay code.
- Root logger conversion of every existing `eprintln!` call.

# Rollback path

Revert the single lane commit. This removes the logging module, command registration, frontend listeners, and the direct dependency entry. Existing log files remain inert user data and may be removed manually; no schema or persisted application state requires migration.
