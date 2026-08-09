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
import { after, describe, it } from "node:test";

import {
  checkSidecarBinaries,
  classifySidecar,
  effectiveExternalBin,
  exeSuffixForTriple,
  expectedSidecarFiles,
  formatFailureReport,
  mergeTauriConfig,
  runSidecarBinaryCheck,
} from "./check-sidecar-binaries-core.mjs";

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
