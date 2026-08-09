// Entry point for the BUG-046 sidecar guard.
//
// Wired into `build.beforeBundleCommand` in tauri.conf.json, so the Tauri CLI
// runs it itself immediately before bundling. Both commands that can emit an
// installer — `tauri build` and the standalone `tauri bundle` — run it; see
// scripts/check-sidecar-binaries-core.mjs for the exact boundary of what does
// not (`--no-bundle`, which ships nothing, and `--config` deltas, which the
// wiring contract test forbids from touching the key). Also runnable by hand:
//
//     node desktop/scripts/check-sidecar-binaries.mjs [target-triple]
//
// Paths resolve from this file's location, never from process.cwd(), because
// the CLI's hook working directory is not guaranteed.

import path from "node:path";
import { fileURLToPath } from "node:url";
import { runSidecarBinaryCheck } from "../../scripts/check-sidecar-binaries-core.mjs";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const srcTauriDir = path.resolve(__dirname, "..", "src-tauri");

const explicitTriple = process.argv[2];

process.exit(
  runSidecarBinaryCheck({
    srcTauriDir,
    explicitTriple,
  }),
);
