import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

/**
 * Structural tripwire for BUG-052.
 *
 * `communityDestinationAdmission.test.mjs` proves the rule; this proves the
 * rule is actually in the path. `replaceCommunityDestinationRoute` is the one
 * sink that writes a remembered channel into the hash BEFORE the target
 * community has mounted, i.e. before anything can be validated live. Every call
 * to it must pass a channel id that came out of the admission check — never a
 * `destination.channelId` read straight from storage, which is what shipped the
 * defect, and which a second call site quietly re-introduced.
 *
 * Whole-tree scan on purpose: the two switch paths (community rail and
 * onboarding connect) had independently grown the same unguarded call, so a
 * per-file test would have passed while the bug was live in the other file.
 */

const SRC_ROOT = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../..",
);

const SINK = "replaceCommunityDestinationRoute";
// The module that defines the sink, and its own tests, name it without calling
// it as a navigation.
const DEFINITION_FILES = new Set([
  "app/communityViewTransition.ts",
  "app/communityViewTransition.test.mjs",
]);

function collectSourceFiles(dir, found = []) {
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) {
      collectSourceFiles(full, found);
    } else if (/\.(?:ts|tsx|mjs)$/.test(entry.name)) {
      found.push(full);
    }
  }
  return found;
}

function callSites() {
  return collectSourceFiles(SRC_ROOT)
    .map((file) => ({
      relative: path.relative(SRC_ROOT, file).split(path.sep).join("/"),
      source: fs.readFileSync(file, "utf8"),
    }))
    .filter(
      (file) =>
        !DEFINITION_FILES.has(file.relative) &&
        !file.relative.endsWith("communityDestinationSinkScan.test.mjs") &&
        file.source.includes(`${SINK}(`),
    );
}

test("the remembered-destination sink still has call sites to guard", () => {
  // If this ever hits zero the scan below became vacuous, which is how a
  // structural guard rots into a green no-op.
  assert.ok(
    callSites().length >= 2,
    "expected the community-rail and onboarding switch paths to call the sink",
  );
});

test("every pre-navigation into a remembered channel is admitted first", () => {
  for (const { relative, source } of callSites()) {
    assert.ok(
      source.includes("admitCommunityDestinationRoute("),
      `${relative} pre-navigates into a remembered channel without the BUG-052 admission check`,
    );

    const args = [
      ...source.matchAll(/replaceCommunityDestinationRoute\(\s*([^,]+),/g),
    ].map((match) => match[1].trim());
    for (const arg of args) {
      assert.equal(
        arg,
        "admittedChannelId",
        `${relative} routes to \`${arg}\` — only an admitted channel id may reach ${SINK}`,
      );
    }
  }
});
