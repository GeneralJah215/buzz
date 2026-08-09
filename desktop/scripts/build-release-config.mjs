import { writeFileSync } from "node:fs";
import { resolve } from "node:path";

// Write a tauri.release.conf.json with release-only overrides.
//
// Tauri's --config flag merges the provided JSON on top of the base
// tauri.conf.json, so this file must contain ONLY the delta fields —
// not a copy of the base config.
//
// For OSS release builds this script emits:
// 1. bundle.macOS.minimumSystemVersion = "10.15" for broad compatibility.
// 2. bundle.createUpdaterArtifacts = true so Tauri produces the .tar.gz
//    archive and .sig signature during the build.
// 3. plugins.updater with the public key and endpoint from env vars.
//    Both BUZZ_UPDATER_PUBLIC_KEY and BUZZ_UPDATER_ENDPOINT are required -
//    the script fails if either is missing (OSS builds always ship with updater).
//
// Apple code signing and notarization happen post-build via
// block/apple-codesign-action in release.yml, so no signingIdentity is
// emitted here and the Tauri build is invoked with --no-sign.

const outputConfigPath = resolve(
  process.cwd(),
  "src-tauri/tauri.release.conf.json",
);

const updaterPubkey = process.env.BUZZ_UPDATER_PUBLIC_KEY;
const updaterEndpoint = process.env.BUZZ_UPDATER_ENDPOINT;

const missing = [];
if (!updaterPubkey) missing.push("BUZZ_UPDATER_PUBLIC_KEY");
if (!updaterEndpoint) missing.push("BUZZ_UPDATER_ENDPOINT");
if (missing.length > 0) {
  console.error(
    `Error: required environment variable(s) missing: ${missing.join(", ")}`,
  );
  process.exit(1);
}

const releaseConfig = {
  bundle: {
    macOS: {
      minimumSystemVersion: "10.15",
    },
    createUpdaterArtifacts: true,
  },
  plugins: {
    updater: {
      pubkey: updaterPubkey,
      endpoints: [updaterEndpoint],
    },
  },
};

// Tauri applies --config after the platform-specific config using RFC 7396, so
// whatever this delta says wins over everything. Two keys must never appear in
// it (BUG-046):
//
//   bundle.externalBin        — replaces the platform sidecar list (null deletes
//                               it), so the bundler would ship a different list
//                               than scripts/check-sidecar-binaries-core.mjs
//                               verified, and the difference would go unchecked.
//   build.beforeBundleCommand — "" or null disables the zero-length sidecar
//                               guard entirely, in exactly the jobs that ship.
//
// The previous version of this check looked for `externalBin` on the object
// literal declared directly above, where it can never appear, so it could not
// fire. Walk the config that is actually written instead.
const FORBIDDEN_KEY_PATHS = [
  ["bundle", "externalBin"],
  ["build", "beforeBundleCommand"],
];

function hasKeyPath(value, keyPath) {
  let cursor = value;
  for (const key of keyPath) {
    if (typeof cursor !== "object" || cursor === null || !(key in cursor)) {
      return false;
    }
    cursor = cursor[key];
  }
  return true;
}

for (const keyPath of FORBIDDEN_KEY_PATHS) {
  if (hasKeyPath(releaseConfig, keyPath)) {
    throw new Error(
      `Release config must not define ${keyPath.join(".")}: a --config delta ` +
        "overrides the base and platform configs, which would bypass the " +
        "sidecar guard (BUG-046).",
    );
  }
}

console.log(`Updater enabled -> ${updaterEndpoint}`);

writeFileSync(outputConfigPath, `${JSON.stringify(releaseConfig, null, 2)}\n`);
console.log(`Wrote ${outputConfigPath}`);
