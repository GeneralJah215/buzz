import { execFileSync } from "node:child_process";
import { promises as fs } from "node:fs";
import path from "node:path";

import { parseChangedFiles } from "./check-file-sizes-core.mjs";

function git(args, cwd, options = {}) {
  return execFileSync("git", args, {
    cwd,
    encoding: "utf8",
    maxBuffer: 10 * 1024 * 1024,
    ...options,
  });
}

function toPosixPath(relativePath) {
  return relativePath.split(path.sep).join("/");
}

/// Rust test attributes, including the async ones.
const RUST_TEST = /^\s*#\[(?:\w+::)*(?:tokio::)?test(?:\([^)]*\))?\]/gm;

/// `test("…")` and `it("…")` at the start of a statement. Deliberately not
/// matching `.test(` or `latest(`, which are ordinary calls and identifiers.
const JS_TEST = /(?:^|[\s;{(])(?:test|it)\s*(?:\.\w+\s*)?\(/gm;

/**
 * Count the test declarations in one file's contents.
 *
 * Approximate by design. The ratchet compares this number against itself
 * across two commits, so a consistent undercount is harmless — what matters
 * is that deleting a test lowers it.
 */
export function countTests(content, extension) {
  const pattern = extension === ".rs" ? RUST_TEST : JS_TEST;
  pattern.lastIndex = 0;
  return (content.match(pattern) ?? []).length;
}

/**
 * Compare against the LAST COMMIT, not the branch's merge base.
 *
 * The file-size ratchet uses the merge base because a file's size is a
 * property of the branch. A deleted test is not: over a whole branch, tests
 * added by unrelated later work mask an earlier deletion, and the total still
 * rises. That is precisely how the 40-test loss slipped through — the same
 * branch had added far more than 40 tests elsewhere.
 *
 * Narrowing the window to one step means each change is judged on its own,
 * which is the only granularity at which "this edit removed tests" is visible.
 */
export function resolveTestCountBase(repoRoot, env = process.env) {
  if (env.CHECK_TEST_COUNTS_BASE) return env.CHECK_TEST_COUNTS_BASE;
  // On CI the working tree matches HEAD, so compare the commit to its parent.
  return env.GITHUB_ACTIONS === "true" ? "HEAD^1" : "HEAD";
}

function readBaseFile(repoRoot, baseRef, filePath) {
  return git(["show", `${baseRef}:${filePath}`], repoRoot, {
    encoding: null,
  }).toString("utf8");
}

/**
 * Fail when a change deletes more tests than it adds.
 *
 * This exists because a "pure refactor" that split four oversized modules
 * silently destroyed 40 tests — including the regression test guarding file
 * permissions on the store that holds plaintext agent keys — and the suite
 * went green, because a deleted test cannot fail. Nothing in the repo noticed.
 *
 * The comparison is on the TOTAL across every changed file, not per file, so
 * moving tests between modules is free while removing them is not. That is
 * exactly the distinction the failed refactor blurred.
 */
export async function runTestCountCheck({ projectRoot, roots, label }) {
  const repoRoot = path.dirname(projectRoot);
  const projectRelative = toPosixPath(path.basename(projectRoot));
  const baseRef = resolveTestCountBase(repoRoot);

  // A missing or shallow base must fail loudly rather than pass vacuously.
  git(["cat-file", "-e", `${baseRef}^{commit}`], repoRoot);

  const changes = parseChangedFiles(
    git(
      ["diff", "--name-status", "-z", "-M", baseRef, "--", projectRelative],
      repoRoot,
    ),
  );
  const trackedPaths = new Set(changes.map((change) => change.path));
  for (const filePath of git(
    ["ls-files", "--others", "--exclude-standard", "-z", "--", projectRelative],
    repoRoot,
  )
    .split("\0")
    .filter(Boolean)) {
    if (!trackedPaths.has(filePath)) changes.push({ status: "A", path: filePath });
  }

  const extensions = new Set([".rs", ".ts", ".tsx", ".mjs", ".js"]);
  let baseTotal = 0;
  let headTotal = 0;
  const drops = [];

  for (const change of changes) {
    const relativePath = toPosixPath(
      path.relative(projectRelative, change.path),
    );
    if (!roots.some((root) => relativePath.startsWith(`${root}/`))) continue;
    const extension = path.extname(relativePath);
    if (!extensions.has(extension)) continue;

    const basePath = change.oldPath ?? change.path;
    const baseCount =
      change.status === "A"
        ? 0
        : countTests(readBaseFile(repoRoot, baseRef, basePath), extension);

    // A deleted file has no working-tree contents; every test in it is gone.
    let headCount = 0;
    if (change.status !== "D") {
      headCount = countTests(
        await fs.readFile(path.join(repoRoot, change.path), "utf8"),
        extension,
      );
    }

    baseTotal += baseCount;
    headTotal += headCount;
    if (headCount < baseCount) {
      drops.push({ relativePath, baseCount, headCount });
    }
  }

  if (headTotal >= baseTotal) return;

  console.error(`${label} test-count ratchet failed (base ${baseRef}):`);
  console.error(
    `- ${baseTotal} tests in the changed files before, ${headTotal} after (${headTotal - baseTotal}).`,
  );
  for (const drop of drops) {
    console.error(
      `- ${drop.relativePath}: ${drop.baseCount} -> ${drop.headCount} tests`,
    );
  }
  console.error(
    "Moving tests between files is fine; the total must not fall. If a test is",
  );
  console.error(
    "genuinely obsolete, delete it in its own commit that says why.",
  );
  console.error(
    "Override for a deliberate removal: CHECK_TEST_COUNTS_ALLOW_DROP=1",
  );
  if (process.env.CHECK_TEST_COUNTS_ALLOW_DROP === "1") {
    console.error("Override set; passing anyway.");
    return;
  }
  process.exitCode = 1;
}
