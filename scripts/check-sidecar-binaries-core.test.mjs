// Tests for the BUG-046 sidecar guard.
//
// Every assertion here is written against a property the bug actually had:
// a 0-byte file that passed an existence check, a platform config whose
// externalBin array replaces rather than extends the base one, and a failure
// message that has to name the file and its size or the operator cannot act.

import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { after, describe, it } from "node:test";

import {
  checkSidecarBinaries,
  classifySidecar,
  effectiveExternalBin,
  exeSuffixForTriple,
  expectedSidecarFiles,
  formatFailureReport,
  mergeTauriConfig,
  platformForTriple,
  runSidecarBinaryCheck,
} from "./check-sidecar-binaries-core.mjs";

const REPO_ROOT = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "..",
);

const WINDOWS_TRIPLE = "x86_64-pc-windows-msvc";
const LINUX_TRIPLE = "x86_64-unknown-linux-gnu";
const MACOS_TRIPLE = "aarch64-apple-darwin";

const PE_HEADER = Buffer.from([0x4d, 0x5a, 0x90, 0x00]);
const ELF_HEADER = Buffer.from([0x7f, 0x45, 0x4c, 0x46]);
const MACHO_HEADER = Buffer.from([0xcf, 0xfa, 0xed, 0xfe]);

const tempDirs = [];

function makeTempDir() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "sidecar-guard-"));
  tempDirs.push(dir);
  return dir;
}

after(() => {
  for (const dir of tempDirs) {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

describe("mergeTauriConfig", () => {
  it("replaces arrays instead of concatenating them", () => {
    // The whole reason the checker needs its own merge. tauri.windows.conf.json
    // lists five sidecars; the base lists six. Concatenating would demand
    // buzz-backend-kubernetes on Windows and fail every Windows build.
    const merged = mergeTauriConfig(
      { bundle: { externalBin: ["a", "b", "c"], active: true } },
      { bundle: { externalBin: ["a", "b"] } },
    );
    assert.deepEqual(merged.bundle.externalBin, ["a", "b"]);
  });

  it("merges nested objects key by key and keeps untouched keys", () => {
    const merged = mergeTauriConfig(
      { bundle: { active: true, targets: "all" }, productName: "Buzz" },
      { bundle: { targets: "nsis" } },
    );
    assert.equal(merged.productName, "Buzz");
    assert.equal(merged.bundle.active, true);
    assert.equal(merged.bundle.targets, "nsis");
  });
});

describe("effectiveExternalBin", () => {
  it("returns the platform override list when one is present", () => {
    const base = {
      bundle: {
        externalBin: [
          "binaries/buzz-acp",
          "binaries/buzz-backend-kubernetes",
          "binaries/buzz",
        ],
      },
    };
    const override = {
      bundle: { externalBin: ["binaries/buzz-acp", "binaries/buzz"] },
    };
    assert.deepEqual(effectiveExternalBin(base, override), [
      "binaries/buzz-acp",
      "binaries/buzz",
    ]);
  });

  it("falls back to the base list when the override does not set one", () => {
    const base = { bundle: { externalBin: ["binaries/buzz"] } };
    assert.deepEqual(effectiveExternalBin(base, { bundle: {} }), [
      "binaries/buzz",
    ]);
  });

  it("returns an empty list rather than throwing when there is no bundle key", () => {
    assert.deepEqual(effectiveExternalBin({}, {}), []);
  });
});

describe("expectedSidecarFiles", () => {
  it("builds the <name>-<triple>.exe names Tauri looks for on Windows", () => {
    const files = expectedSidecarFiles({
      externalBin: ["binaries/buzz-acp"],
      triple: WINDOWS_TRIPLE,
    });
    assert.equal(
      files[0].relativePath,
      `binaries/buzz-acp-${WINDOWS_TRIPLE}.exe`,
    );
    assert.equal(files[0].name, "buzz-acp");
  });

  it("omits the .exe suffix off Windows", () => {
    const files = expectedSidecarFiles({
      externalBin: ["binaries/buzz-acp"],
      triple: LINUX_TRIPLE,
    });
    assert.equal(files[0].relativePath, `binaries/buzz-acp-${LINUX_TRIPLE}`);
  });

  it("maps the exe suffix off the triple, not the running host", () => {
    assert.equal(exeSuffixForTriple(WINDOWS_TRIPLE), ".exe");
    assert.equal(exeSuffixForTriple(LINUX_TRIPLE), "");
    assert.equal(exeSuffixForTriple(MACOS_TRIPLE), "");
  });
});

describe("classifySidecar", () => {
  it("REJECTS a 0-byte file — the exact shape of BUG-046", () => {
    const verdict = classifySidecar({
      exists: true,
      sizeBytes: 0,
      header: Buffer.alloc(0),
      triple: WINDOWS_TRIPLE,
    });
    assert.equal(verdict.ok, false);
    assert.equal(verdict.reason, "empty");
    assert.match(verdict.detail, /0 bytes/);
  });

  it("rejects a missing file", () => {
    const verdict = classifySidecar({
      exists: false,
      sizeBytes: 0,
      header: Buffer.alloc(0),
      triple: WINDOWS_TRIPLE,
    });
    assert.equal(verdict.ok, false);
    assert.equal(verdict.reason, "missing");
  });

  it("rejects a non-empty file that is not an executable", () => {
    // A text placeholder is just as unshippable as an empty one.
    const verdict = classifySidecar({
      exists: true,
      sizeBytes: 42,
      header: Buffer.from("TODO"),
      triple: WINDOWS_TRIPLE,
    });
    assert.equal(verdict.ok, false);
    assert.equal(verdict.reason, "not-executable");
  });

  it("rejects an ELF binary staged for a Windows target", () => {
    const verdict = classifySidecar({
      exists: true,
      sizeBytes: 1024,
      header: ELF_HEADER,
      triple: WINDOWS_TRIPLE,
    });
    assert.equal(verdict.ok, false);
    assert.equal(verdict.reason, "not-executable");
  });

  it("accepts a real PE / ELF / Mach-O binary for its own triple", () => {
    assert.equal(
      classifySidecar({
        exists: true,
        sizeBytes: 12_800_000,
        header: PE_HEADER,
        triple: WINDOWS_TRIPLE,
      }).ok,
      true,
    );
    assert.equal(
      classifySidecar({
        exists: true,
        sizeBytes: 12_800_000,
        header: ELF_HEADER,
        triple: LINUX_TRIPLE,
      }).ok,
      true,
    );
    assert.equal(
      classifySidecar({
        exists: true,
        sizeBytes: 12_800_000,
        header: MACHO_HEADER,
        triple: MACOS_TRIPLE,
      }).ok,
      true,
    );
  });
});

describe("formatFailureReport", () => {
  it("names the file, its size, and how to produce it properly", () => {
    const report = formatFailureReport({
      failures: [
        {
          relativePath: `binaries/buzz-acp-${WINDOWS_TRIPLE}.exe`,
          sizeBytes: 0,
          detail: "file is 0 bytes",
        },
      ],
      triple: WINDOWS_TRIPLE,
      buildHint: "cargo build --release -p buzz-acp",
      bundleHint: "./scripts/bundle-sidecars.sh",
    });
    assert.match(report, /buzz-acp-x86_64-pc-windows-msvc\.exe/);
    assert.match(report, /0 bytes/);
    assert.match(report, /cargo build --release -p buzz-acp/);
    assert.match(report, /bundle-sidecars\.sh/);
    // The operator must be told not to "fix" this by regenerating stubs.
    assert.match(report, /Do NOT create placeholder files/);
  });
});

// ---------------------------------------------------------------------------
// End-to-end against a real directory tree.
// ---------------------------------------------------------------------------

function stageFixture({ contents }) {
  const srcTauriDir = makeTempDir();
  fs.writeFileSync(
    path.join(srcTauriDir, "tauri.conf.json"),
    JSON.stringify({
      bundle: {
        externalBin: [
          "binaries/buzz-acp",
          "binaries/buzz-backend-kubernetes",
          "binaries/buzz",
        ],
      },
    }),
  );
  fs.writeFileSync(
    path.join(srcTauriDir, "tauri.windows.conf.json"),
    JSON.stringify({
      bundle: { externalBin: ["binaries/buzz-acp", "binaries/buzz"] },
    }),
  );
  const binariesDir = path.join(srcTauriDir, "binaries");
  fs.mkdirSync(binariesDir);
  for (const [name, body] of Object.entries(contents)) {
    fs.writeFileSync(
      path.join(binariesDir, `${name}-${WINDOWS_TRIPLE}.exe`),
      body,
    );
  }
  return srcTauriDir;
}

const REAL_ENOUGH = Buffer.concat([PE_HEADER, Buffer.alloc(4096)]);

describe("checkSidecarBinaries (real filesystem)", () => {
  it("passes when every Windows sidecar is a real PE binary", () => {
    const srcTauriDir = stageFixture({
      contents: { "buzz-acp": REAL_ENOUGH, buzz: REAL_ENOUGH },
    });
    const outcome = checkSidecarBinaries({
      srcTauriDir,
      triple: WINDOWS_TRIPLE,
      platform: "win32",
    });
    assert.equal(outcome.ok, true);
    assert.equal(outcome.results.length, 2);
  });

  it("does not demand the Linux-only sidecar on Windows", () => {
    // buzz-backend-kubernetes is in the base externalBin but not the Windows
    // override. Demanding it would fail every Windows build.
    const srcTauriDir = stageFixture({
      contents: { "buzz-acp": REAL_ENOUGH, buzz: REAL_ENOUGH },
    });
    const outcome = checkSidecarBinaries({
      srcTauriDir,
      triple: WINDOWS_TRIPLE,
      platform: "win32",
    });
    assert.equal(outcome.ok, true);
    assert.ok(
      !outcome.externalBin.includes("binaries/buzz-backend-kubernetes"),
      "Windows externalBin must come from the platform override",
    );
  });

  it("FAILS when one sidecar is 0 bytes", () => {
    const srcTauriDir = stageFixture({
      contents: { "buzz-acp": Buffer.alloc(0), buzz: REAL_ENOUGH },
    });
    const outcome = checkSidecarBinaries({
      srcTauriDir,
      triple: WINDOWS_TRIPLE,
      platform: "win32",
    });
    assert.equal(outcome.ok, false);
    const failed = outcome.results.filter((result) => !result.ok);
    assert.equal(failed.length, 1);
    assert.equal(failed[0].name, "buzz-acp");
    assert.equal(failed[0].reason, "empty");
  });

  it("FAILS when a sidecar is missing entirely", () => {
    const srcTauriDir = stageFixture({
      contents: { "buzz-acp": REAL_ENOUGH },
    });
    const outcome = checkSidecarBinaries({
      srcTauriDir,
      triple: WINDOWS_TRIPLE,
      platform: "win32",
    });
    assert.equal(outcome.ok, false);
    assert.equal(
      outcome.results.find((result) => result.name === "buzz").reason,
      "missing",
    );
  });

  it("throws with the path attached when a config is unreadable", () => {
    const srcTauriDir = makeTempDir();
    assert.throws(
      () =>
        checkSidecarBinaries({
          srcTauriDir,
          triple: WINDOWS_TRIPLE,
          platform: "win32",
        }),
      /Cannot read Tauri config/,
    );
  });

  it("throws with the path attached when a config is malformed JSON", () => {
    const srcTauriDir = makeTempDir();
    fs.writeFileSync(path.join(srcTauriDir, "tauri.conf.json"), "{ not json");
    assert.throws(
      () =>
        checkSidecarBinaries({
          srcTauriDir,
          triple: WINDOWS_TRIPLE,
          platform: "win32",
        }),
      /is not valid JSON/,
    );
  });
});

describe("runSidecarBinaryCheck exit codes", () => {
  const silence = () => {};

  it("returns 0 and lists sizes when everything is real", () => {
    const srcTauriDir = stageFixture({
      contents: { "buzz-acp": REAL_ENOUGH, buzz: REAL_ENOUGH },
    });
    const lines = [];
    const code = runSidecarBinaryCheck({
      srcTauriDir,
      explicitTriple: WINDOWS_TRIPLE,
      platform: "win32",
      env: {},
      log: (line) => lines.push(line),
      logError: silence,
    });
    assert.equal(code, 0);
    assert.match(lines.join("\n"), /2 sidecars OK/);
  });

  it("returns a NON-ZERO exit code on a 0-byte sidecar", () => {
    // The build must not be able to proceed. A warning here is the bug.
    const srcTauriDir = stageFixture({
      contents: { "buzz-acp": Buffer.alloc(0), buzz: REAL_ENOUGH },
    });
    const errors = [];
    const code = runSidecarBinaryCheck({
      srcTauriDir,
      explicitTriple: WINDOWS_TRIPLE,
      platform: "win32",
      env: {},
      log: silence,
      logError: (line) => errors.push(line),
    });
    assert.equal(code, 1);
    assert.match(errors.join("\n"), /BUNDLE BLOCKED/);
    assert.match(errors.join("\n"), /buzz-acp-x86_64-pc-windows-msvc\.exe/);
  });

  it("returns non-zero rather than vacuously passing when externalBin is empty", () => {
    const srcTauriDir = makeTempDir();
    fs.writeFileSync(
      path.join(srcTauriDir, "tauri.conf.json"),
      JSON.stringify({ bundle: { externalBin: [] } }),
    );
    const errors = [];
    const code = runSidecarBinaryCheck({
      srcTauriDir,
      explicitTriple: WINDOWS_TRIPLE,
      platform: "win32",
      env: {},
      log: silence,
      logError: (line) => errors.push(line),
    });
    assert.equal(code, 1);
    assert.match(errors.join("\n"), /Nothing was verified/);
  });

  it("prefers an explicit triple over the environment", () => {
    const srcTauriDir = stageFixture({
      contents: { "buzz-acp": REAL_ENOUGH, buzz: REAL_ENOUGH },
    });
    const code = runSidecarBinaryCheck({
      srcTauriDir,
      explicitTriple: WINDOWS_TRIPLE,
      platform: "win32",
      env: { TAURI_ENV_TARGET_TRIPLE: LINUX_TRIPLE },
      log: silence,
      logError: silence,
    });
    assert.equal(code, 0);
  });
});

// ---------------------------------------------------------------------------
// Wiring contract.
//
// The tests above prove the CHECK is correct. These prove it is still
// INSTALLED. Without them, deleting build.beforeBundleCommand from
// tauri.conf.json, or dropping the call out of bundle-sidecars.sh, leaves every
// gate in this repo green while the guard silently stops running — which is the
// same failure shape as BUG-046 itself: everything exits 0 and nothing objects.
// ---------------------------------------------------------------------------

function readRepoFile(relativePath) {
  return fs.readFileSync(path.join(REPO_ROOT, relativePath), "utf8");
}

const CHECKER_ENTRY = "desktop/scripts/check-sidecar-binaries.mjs";

describe("wiring contract: the guard is installed", () => {
  it("tauri.conf.json runs the checker as beforeBundleCommand", () => {
    const config = JSON.parse(
      readRepoFile("desktop/src-tauri/tauri.conf.json"),
    );
    const hook = config.build?.beforeBundleCommand;
    assert.ok(
      hook,
      "build.beforeBundleCommand is missing — the bundle-time guard is not installed",
    );
    const script = typeof hook === "string" ? hook : hook.script;
    assert.match(
      script,
      /check-sidecar-binaries\.mjs/,
      "beforeBundleCommand no longer invokes the sidecar checker",
    );
    // Hooks run with cwd = the src-tauri directory, so ".." is what puts the
    // relative `./scripts/...` path inside desktop/.
    if (typeof hook !== "string" && script.startsWith("./scripts/")) {
      assert.equal(
        hook.cwd,
        "..",
        "a './scripts/...' hook script needs cwd '..' or it resolves inside src-tauri",
      );
    }
  });

  it("the checker entry point turns a failed check into a non-zero exit", () => {
    // runSidecarBinaryCheck only RETURNS a code. If this file stops feeding it
    // to process.exit, the hook always succeeds and the guard is decorative.
    const source = readRepoFile(CHECKER_ENTRY);
    assert.match(
      source,
      /process\.exit\(/,
      "entry point must call process.exit",
    );
    assert.match(
      source,
      /process\.exit\(\s*\n?\s*runSidecarBinaryCheck\(/,
      "process.exit must receive runSidecarBinaryCheck's result directly",
    );
  });

  it("bundle-sidecars.sh verifies what it just staged", () => {
    const script = readRepoFile("scripts/bundle-sidecars.sh");
    assert.match(
      script,
      /check-sidecar-binaries\.mjs/,
      "bundle-sidecars.sh no longer verifies the binaries it stages",
    );
    assert.match(
      script,
      /-s "\$src"|! -s "\$src"/,
      "bundle-sidecars.sh no longer rejects a zero-length source binary",
    );
  });

  it("no --config delta or workflow overrides beforeBundleCommand", () => {
    // Tauri applies --config LAST, as RFC 7396, over base + platform config.
    // Every workflow that bundles passes --config, so a delta setting this key
    // to "" or null would disable the guard in exactly the jobs that ship.
    const workflowDir = path.join(REPO_ROOT, ".github", "workflows");
    const suspects = fs
      .readdirSync(workflowDir)
      .filter((name) => name.endsWith(".yml") || name.endsWith(".yaml"))
      .map((name) => path.join(".github", "workflows", name));

    for (const relativePath of suspects) {
      const source = readRepoFile(relativePath);
      // Strip comment lines so this file's own explanatory prose does not trip it.
      const code = source
        .split("\n")
        .filter((line) => !/^\s*(#|\/\/)/.test(line))
        .join("\n");
      assert.ok(
        !code.includes("beforeBundleCommand"),
        `${relativePath} sets build.beforeBundleCommand in a config delta; that disables the BUG-046 guard for every build it runs`,
      );
      assert.ok(
        !code.includes("externalBin"),
        `${relativePath} sets bundle.externalBin in a config delta; the checker reads only tauri.conf.json + the platform config, so it would validate a different list than the one bundled`,
      );
    }
  });

  it("the generated release config delta refuses both bypass keys", () => {
    // build-release-config.mjs writes the --config delta used by every release
    // build, so it is the one place a bypass could be introduced legitimately.
    // Grepping it for the key names is useless (its own guard names them), so
    // assert the guard exists and covers both.
    const source = readRepoFile("desktop/scripts/build-release-config.mjs");
    assert.match(
      source,
      /FORBIDDEN_KEY_PATHS/,
      "build-release-config.mjs no longer guards its output against bypass keys",
    );
    assert.match(source, /\["bundle",\s*"externalBin"\]/);
    assert.match(source, /\["build",\s*"beforeBundleCommand"\]/);
  });
});

describe("wiring contract: the sidecar lists agree", () => {
  const baseConfig = JSON.parse(
    readRepoFile("desktop/src-tauri/tauri.conf.json"),
  );
  const windowsConfig = JSON.parse(
    readRepoFile("desktop/src-tauri/tauri.windows.conf.json"),
  );

  it("the Windows externalBin list is a subset of the base list", () => {
    // The Windows override REPLACES the base array. A name present only in the
    // override would be bundled on Windows and never checked anywhere else.
    const base = new Set(baseConfig.bundle.externalBin);
    for (const entry of windowsConfig.bundle.externalBin) {
      assert.ok(
        base.has(entry),
        `${entry} is in tauri.windows.conf.json but not in tauri.conf.json`,
      );
    }
  });

  it("bundle-sidecars.sh stages every sidecar that externalBin bundles", () => {
    // Drift between the staging list and the bundling list is how a sidecar
    // ends up unstaged (missing) or unchecked (bundled but never verified).
    const script = readRepoFile("scripts/bundle-sidecars.sh");
    const match = script.match(/^SIDECARS=\(([^)]*)\)/m);
    assert.ok(match, "could not find the SIDECARS array in bundle-sidecars.sh");
    const staged = new Set(match[1].trim().split(/\s+/));
    // buzz-backend-kubernetes is appended conditionally for non-Windows targets.
    const conditional = new Set(["buzz-backend-kubernetes"]);

    for (const entry of baseConfig.bundle.externalBin) {
      const name = path.posix.basename(entry);
      assert.ok(
        staged.has(name) || conditional.has(name),
        `externalBin bundles '${name}' but bundle-sidecars.sh never stages it`,
      );
    }
  });
});

describe("platformForTriple", () => {
  // The production hook passes no platform, so this function alone decides
  // which platform config is read. Every filesystem test above passes an
  // explicit platform, so without these the real code path is untested.
  it("maps each triple family to its Tauri platform config", () => {
    assert.equal(platformForTriple(WINDOWS_TRIPLE), "win32");
    assert.equal(platformForTriple(MACOS_TRIPLE), "darwin");
    assert.equal(platformForTriple("x86_64-apple-darwin"), "darwin");
    assert.equal(platformForTriple(LINUX_TRIPLE), "linux");
    assert.equal(platformForTriple("aarch64-unknown-linux-musl"), "linux");
    assert.equal(platformForTriple("universal-apple-darwin"), "darwin");
  });
});

describe("non-Windows targets (no platform override file)", () => {
  // Every macOS and Linux build takes this branch: no tauri.<plat>.conf.json
  // exists, so the base six-entry list applies, including the sidecar the
  // Windows override drops.
  function stageLinuxFixture(contents) {
    const srcTauriDir = makeTempDir();
    fs.writeFileSync(
      path.join(srcTauriDir, "tauri.conf.json"),
      JSON.stringify({
        bundle: {
          externalBin: [
            "binaries/buzz-acp",
            "binaries/buzz-backend-kubernetes",
          ],
        },
      }),
    );
    const binariesDir = path.join(srcTauriDir, "binaries");
    fs.mkdirSync(binariesDir);
    for (const [name, body] of Object.entries(contents)) {
      fs.writeFileSync(path.join(binariesDir, `${name}-${LINUX_TRIPLE}`), body);
    }
    return srcTauriDir;
  }

  const REAL_ELF = Buffer.concat([ELF_HEADER, Buffer.alloc(4096)]);

  it("checks the full base list, with no .exe suffix, using the derived platform", () => {
    const srcTauriDir = stageLinuxFixture({
      "buzz-acp": REAL_ELF,
      "buzz-backend-kubernetes": REAL_ELF,
    });
    // Note: no `platform` argument — exercises platformForTriple, as the hook does.
    const code = runSidecarBinaryCheck({
      srcTauriDir,
      explicitTriple: LINUX_TRIPLE,
      env: {},
      log: () => {},
      logError: () => {},
    });
    assert.equal(code, 0);
  });

  it("FAILS on a 0-byte Linux sidecar", () => {
    const srcTauriDir = stageLinuxFixture({
      "buzz-acp": REAL_ELF,
      "buzz-backend-kubernetes": Buffer.alloc(0),
    });
    const errors = [];
    const code = runSidecarBinaryCheck({
      srcTauriDir,
      explicitTriple: LINUX_TRIPLE,
      env: {},
      log: () => {},
      logError: (line) => errors.push(line),
    });
    assert.equal(code, 1);
    assert.match(errors.join("\n"), /buzz-backend-kubernetes/);
  });
});

describe("resolveTriple honours the Tauri hook environment", () => {
  it("uses TAURI_ENV_TARGET_TRIPLE when no explicit triple is given", () => {
    // This is the real hook path for a cross-compile: Tauri exports the target
    // triple, and the checker must prefer it over the rustc host.
    const srcTauriDir = stageFixture({
      contents: { "buzz-acp": REAL_ENOUGH, buzz: REAL_ENOUGH },
    });
    const code = runSidecarBinaryCheck({
      srcTauriDir,
      env: { TAURI_ENV_TARGET_TRIPLE: WINDOWS_TRIPLE },
      log: () => {},
      logError: () => {},
    });
    assert.equal(code, 0);
  });
});
