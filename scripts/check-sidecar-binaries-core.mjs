// =============================================================================
// check-sidecar-binaries-core.mjs — refuse to bundle a broken sidecar
// =============================================================================
// BUG-046. `desktop/src-tauri/binaries/` held 0-byte files for every sidecar.
// Tauri bundles whatever is at the `externalBin` paths without inspecting it,
// so `pnpm tauri build` produced installers that overwrote a working install
// with empty executables. Every agent then died with
// `%1 is not a valid Win32 application (os error 193)`, and the build, the
// bundle step, and the installer all exited 0.
//
// The defect was never the stub files — it was that nothing between staging and
// shipping ever asserted the payload was a real executable. This module is that
// assertion. It is wired into `build.beforeBundleCommand` in tauri.conf.json,
// which the Tauri CLI runs itself immediately before the bundling phase, so it
// cannot be skipped by anyone invoking `tauri build` however they like.
//
// Everything here resolves paths from the module's own location, never from
// `process.cwd()`, because the CLI's hook working directory is not guaranteed.
// =============================================================================

import { execFileSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";

/** Platform-specific Tauri config that overrides the base config, by os.platform(). */
export const PLATFORM_CONFIG_BY_OS = {
  win32: "tauri.windows.conf.json",
  darwin: "tauri.macos.conf.json",
  linux: "tauri.linux.conf.json",
};

/**
 * Which platform's config applies, derived from the TARGET triple rather than
 * the running host. Tauri selects the platform override by target, so a
 * cross-compile (`tauri build --target x86_64-pc-windows-msvc` from Linux, or
 * `bundle-sidecars.sh <triple>`) must be judged against the target's
 * externalBin list, not the builder's.
 */
export function platformForTriple(triple) {
  if (triple.includes("windows")) {
    return "win32";
  }
  if (triple.includes("apple") || triple.includes("darwin")) {
    return "darwin";
  }
  return "linux";
}

/**
 * Merge a platform override into the base Tauri config the way the Tauri CLI
 * does: objects merge key by key, every other value (arrays included) is
 * REPLACED wholesale.
 *
 * This distinction is load-bearing. `tauri.windows.conf.json` in this repo sets
 * its own `bundle.externalBin` listing five sidecars; the base config lists six
 * (it adds buzz-backend-kubernetes). A checker that concatenated the two would
 * demand a Linux-only sidecar on Windows and fail every Windows build.
 */
export function mergeTauriConfig(base, override) {
  if (!isPlainObject(base) || !isPlainObject(override)) {
    return override === undefined ? base : override;
  }
  const merged = { ...base };
  for (const [key, overrideValue] of Object.entries(override)) {
    merged[key] =
      isPlainObject(base[key]) && isPlainObject(overrideValue)
        ? mergeTauriConfig(base[key], overrideValue)
        : overrideValue;
  }
  return merged;
}

function isPlainObject(value) {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** The `bundle.externalBin` list that actually applies on this platform. */
export function effectiveExternalBin(base, override) {
  const merged = mergeTauriConfig(base, override ?? {});
  return merged?.bundle?.externalBin ?? [];
}

/** MSVC and MinGW emit `.exe`; nothing else does. */
export function exeSuffixForTriple(triple) {
  return triple.includes("windows") ? ".exe" : "";
}

/**
 * Tauri resolves each `externalBin` entry `binaries/<name>` to the on-disk file
 * `binaries/<name>-<target-triple><exe>`. Mirror that exactly.
 */
export function expectedSidecarFiles({ externalBin, triple }) {
  const exe = exeSuffixForTriple(triple);
  return externalBin.map((entry) => ({
    entry,
    name: path.posix.basename(entry),
    relativePath: `${entry}-${triple}${exe}`,
  }));
}

/**
 * The magic bytes every real executable for a given triple must start with.
 * A 0-byte file fails on length; a text placeholder or a truncated copy fails
 * here. Both are "we are about to ship something that cannot execute".
 */
export function expectedFormatForTriple(triple) {
  if (triple.includes("windows")) {
    return {
      label: "PE (Windows executable)",
      matches: (header) => header[0] === 0x4d && header[1] === 0x5a, // "MZ"
    };
  }
  if (triple.includes("apple") || triple.includes("darwin")) {
    return {
      label: "Mach-O (macOS executable)",
      matches: (header) => {
        const magic = header.readUInt32BE(0);
        return (
          magic === 0xfeedface || // 32-bit
          magic === 0xfeedfacf || // 64-bit
          magic === 0xcefaedfe || // 32-bit, byte-swapped
          magic === 0xcffaedfe || // 64-bit, byte-swapped
          magic === 0xcafebabe || // universal / fat
          magic === 0xbebafeca
        );
      },
    };
  }
  return {
    label: "ELF (Linux executable)",
    matches: (header) =>
      header[0] === 0x7f &&
      header[1] === 0x45 &&
      header[2] === 0x4c &&
      header[3] === 0x46, // "\x7fELF"
  };
}

/**
 * Classify one staged sidecar. Pure: takes the facts, returns a verdict, so the
 * decision table is unit-testable without touching a filesystem.
 *
 * Returns `{ ok: true }` or `{ ok: false, reason, detail }`.
 */
export function classifySidecar({ exists, sizeBytes, header, triple }) {
  if (!exists) {
    return {
      ok: false,
      reason: "missing",
      detail: "file does not exist",
    };
  }
  if (sizeBytes === 0) {
    return {
      ok: false,
      reason: "empty",
      detail: "file is 0 bytes — an empty placeholder, not an executable",
    };
  }
  const format = expectedFormatForTriple(triple);
  if (header.length < 4 || !format.matches(header)) {
    return {
      ok: false,
      reason: "not-executable",
      detail: `file does not begin with ${format.label} magic bytes (starts with ${formatHeader(header)}) — it is truncated or is not an executable at all`,
    };
  }
  return { ok: true };
}

function formatHeader(header) {
  if (header.length === 0) {
    return "nothing";
  }
  return Array.from(header.subarray(0, 4))
    .map((byte) => `0x${byte.toString(16).padStart(2, "0")}`)
    .join(" ");
}

/** Read the facts `classifySidecar` needs about one absolute path. */
export function inspectFile(absolutePath) {
  let stats;
  try {
    stats = fs.statSync(absolutePath);
  } catch (error) {
    if (error.code === "ENOENT") {
      return { exists: false, sizeBytes: 0, header: Buffer.alloc(0) };
    }
    // Anything else (EACCES, EBUSY, a directory in the way) is a real problem
    // and must not be mistaken for "missing". Re-raise with the path attached.
    throw new Error(`Cannot stat sidecar '${absolutePath}': ${error.message}`, {
      cause: error,
    });
  }

  if (stats.size === 0) {
    return { exists: true, sizeBytes: 0, header: Buffer.alloc(0) };
  }

  const header = Buffer.alloc(Math.min(4, stats.size));
  const fd = fs.openSync(absolutePath, "r");
  try {
    fs.readSync(fd, header, 0, header.length, 0);
  } finally {
    fs.closeSync(fd);
  }
  return { exists: true, sizeBytes: stats.size, header };
}

/** The host triple, as `bundle-sidecars.sh` computes it. */
export function hostTargetTriple() {
  const output = execFileSync("rustc", ["-vV"], { encoding: "utf8" });
  const match = output.match(/^host:\s*(\S+)$/m);
  if (!match) {
    throw new Error(
      `Could not read the host target triple from 'rustc -vV' output:\n${output}`,
    );
  }
  return match[1];
}

/**
 * Resolve the triple to check. An explicit value wins (so `tauri build
 * --target <triple>` can be honoured); otherwise fall back to the rustc host,
 * which is what `bundle-sidecars.sh` uses when invoked with no argument.
 */
export function resolveTriple({ explicitTriple, env = process.env } = {}) {
  return (
    explicitTriple ||
    env.TAURI_ENV_TARGET_TRIPLE ||
    env.BUZZ_SIDECAR_TARGET ||
    hostTargetTriple()
  );
}

function humanSize(bytes) {
  if (bytes < 1024) {
    return `${bytes} bytes`;
  }
  const mb = bytes / (1024 * 1024);
  return mb >= 1
    ? `${mb.toFixed(1)} MB (${bytes} bytes)`
    : `${(bytes / 1024).toFixed(1)} KB (${bytes} bytes)`;
}

/**
 * Build the failure report. Kept separate from I/O so the exact operator-facing
 * text is asserted by tests rather than eyeballed once and left to rot.
 */
export function formatFailureReport({
  failures,
  triple,
  buildHint,
  bundleHint,
}) {
  const lines = [
    "",
    "==============================================================================",
    " BUNDLE BLOCKED — sidecar binaries are not shippable (BUG-046 guard)",
    "==============================================================================",
    "",
    `Target triple: ${triple}`,
    "",
    `${failures.length} of the executables Tauri is about to bundle cannot run.`,
    "Bundling them would produce an installer that REPLACES a working install",
    "with broken files. Every agent would then fail with:",
    "    %1 is not a valid Win32 application (os error 193)",
    "",
  ];

  for (const failure of failures) {
    lines.push(`  FAIL  ${failure.relativePath}`);
    lines.push(`        size:   ${humanSize(failure.sizeBytes)}`);
    lines.push(`        reason: ${failure.detail}`);
    lines.push("");
  }

  lines.push("To produce these properly, from the repository root:");
  lines.push("");
  lines.push(`    ${buildHint}`);
  lines.push(`    ${bundleHint}`);
  lines.push("");
  lines.push(
    "Do NOT create placeholder files to get past this check. An empty or",
  );
  lines.push(
    "truncated sidecar is the exact defect this guard exists to stop, and it",
  );
  lines.push("has already broken a working install once.");
  lines.push(
    "==============================================================================",
  );
  lines.push("");
  return lines.join("\n");
}

function buildHintFor(triple) {
  const packages = [
    "buzz-acp",
    "buzz-agent",
    "buzz-dev-mcp",
    "git-credential-nostr",
    "buzz-cli",
  ];
  if (!triple.includes("windows")) {
    packages.splice(2, 0, "buzz-backend-kubernetes");
  }
  return `cargo build --release ${packages.map((p) => `-p ${p}`).join(" ")}`;
}

/**
 * Check every sidecar Tauri will bundle for this platform.
 *
 * Throws on the first structural problem (unreadable config, unreadable file);
 * returns `{ ok, triple, results }` otherwise. Callers decide how to exit.
 */
export function checkSidecarBinaries({
  srcTauriDir,
  triple,
  platform = platformForTriple(triple),
}) {
  const basePath = path.join(srcTauriDir, "tauri.conf.json");
  const base = readJson(basePath);

  const overrideName = PLATFORM_CONFIG_BY_OS[platform];
  const overridePath = overrideName
    ? path.join(srcTauriDir, overrideName)
    : null;
  const override =
    overridePath && fs.existsSync(overridePath) ? readJson(overridePath) : {};

  const externalBin = effectiveExternalBin(base, override);
  if (externalBin.length === 0) {
    // Not an error worth failing a build over, but it is worth saying out loud:
    // silently checking nothing is how this class of bug survives.
    return { ok: true, triple, externalBin, results: [], empty: true };
  }

  const results = expectedSidecarFiles({ externalBin, triple }).map((file) => {
    const absolutePath = path.join(srcTauriDir, file.relativePath);
    const facts = inspectFile(absolutePath);
    const verdict = classifySidecar({ ...facts, triple });
    return { ...file, absolutePath, ...facts, ...verdict };
  });

  return {
    ok: results.every((result) => result.ok),
    triple,
    externalBin,
    results,
  };
}

function readJson(absolutePath) {
  let raw;
  try {
    raw = fs.readFileSync(absolutePath, "utf8");
  } catch (error) {
    throw new Error(
      `Cannot read Tauri config '${absolutePath}': ${error.message}`,
      { cause: error },
    );
  }
  try {
    return JSON.parse(raw);
  } catch (error) {
    throw new Error(
      `Tauri config '${absolutePath}' is not valid JSON: ${error.message}`,
      { cause: error },
    );
  }
}

/**
 * CLI entry point. Prints a passing summary or the failure report, and returns
 * the process exit code. Never returns 0 on a failed check.
 */
export function runSidecarBinaryCheck({
  srcTauriDir,
  explicitTriple,
  platform,
  env = process.env,
  log = console.log,
  logError = console.error,
}) {
  const triple = resolveTriple({ explicitTriple, env });
  const outcome = checkSidecarBinaries({
    srcTauriDir,
    triple,
    platform: platform ?? platformForTriple(triple),
  });

  if (outcome.empty) {
    logError(
      `[check-sidecars] No bundle.externalBin entries found for platform '${platform}'. Nothing was verified — this is almost certainly a config mistake.`,
    );
    return 1;
  }

  if (outcome.ok) {
    log(`[check-sidecars] ${outcome.results.length} sidecars OK (${triple}):`);
    for (const result of outcome.results) {
      log(`  ok  ${result.name.padEnd(24)} ${humanSize(result.sizeBytes)}`);
    }
    return 0;
  }

  const failures = outcome.results.filter((result) => !result.ok);
  logError(
    formatFailureReport({
      failures,
      triple,
      buildHint: buildHintFor(triple),
      bundleHint: `./scripts/bundle-sidecars.sh${explicitTriple ? ` ${triple}` : ""}`,
    }),
  );
  return 1;
}
