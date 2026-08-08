import assert from "node:assert/strict";
import test from "node:test";

import { countTests, resolveTestCountBase } from "./check-test-counts-core.mjs";

test("counts plain and attributed Rust tests", () => {
  const source = `
#[cfg(test)]
mod tests {
    #[test]
    fn one() {}

    #[tokio::test]
    async fn two() {}

    #[test]
    #[should_panic]
    fn three() {}
}
`;
  assert.equal(countTests(source, ".rs"), 3);
});

test("a Rust file with no tests counts zero", () => {
  assert.equal(countTests("pub fn latest() -> u8 { 1 }\n", ".rs"), 0);
});

test("counts JavaScript test and it declarations", () => {
  const source = `
test("a", () => {});
it("b", () => {});
test.skip("c", () => {});
describe("group", () => {
  test("d", () => {});
});
`;
  assert.equal(countTests(source, ".mjs"), 4);
});

// The whole point of the ratchet is that a deletion lowers the count. A
// counter that also fired on ordinary calls would drift and make the
// comparison meaningless.
test("does not count method calls or identifiers that merely contain test", () => {
  const source = `
const value = suite.test(thing);
const latest = pick(rows);
const contest = "it(";
`;
  assert.equal(countTests(source, ".mjs"), 0);
});

test("the base is the previous commit, not the branch merge-base", () => {
  assert.equal(resolveTestCountBase("/repo", {}), "HEAD");
  assert.equal(
    resolveTestCountBase("/repo", { GITHUB_ACTIONS: "true" }),
    "HEAD^1",
  );
  assert.equal(
    resolveTestCountBase("/repo", { CHECK_TEST_COUNTS_BASE: "abc123" }),
    "abc123",
  );
});

// A merge-base window is what let 40 deleted tests through: the same branch
// had added far more than 40 tests elsewhere, so the running total still rose.
test("an explicit base always wins over the CI default", () => {
  assert.equal(
    resolveTestCountBase("/repo", {
      GITHUB_ACTIONS: "true",
      CHECK_TEST_COUNTS_BASE: "abc123",
    }),
    "abc123",
  );
});
