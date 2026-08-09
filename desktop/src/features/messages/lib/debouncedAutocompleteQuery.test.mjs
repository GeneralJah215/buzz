/**
 * BUG-048 guardrail — the shared scheduler that all three composer autocompletes
 * (`#channel`, `:emoji`, `@mention`) route their query updates through.
 *
 * Two properties are pinned here, at the one place a fourth autocomplete would
 * inherit them from:
 *
 *   1. CLOSING IS NEVER DEBOUNCED. `detect` returning `null` must call `onClose`
 *      before this function returns, and must cancel any pending timer. The
 *      assertions run in one synchronous block, so no timer CAN have fired and
 *      there is no duration for a later maintainer to widen. (An e2e wait CAN be
 *      widened: commit 7bcfe7e0a took one from 5 s to 10 s and declared the flow
 *      correct while it still failed 3/3.)
 *
 *   2. THE TIMER IS SCHEDULED EITHER WAY, and `detect` IS RE-RUN when it fires.
 *      This is the late-arrival behaviour the prefix hooks depend on:
 *      `detectPrefixQuery`'s multi-word path resolves against a list of known
 *      names that is fetched asynchronously, and returns `null` outright while
 *      that list is empty. So a synchronous close is not the last word — the
 *      same text can become detectable between the keystroke and the timer
 *      firing, with no further input. The two obvious tidy-ups (return early
 *      after `onClose`; reuse the value the synchronous call already computed)
 *      each silently delete a half of that, so each gets its own test rather
 *      than only a comment.
 *
 * ── Mutation coverage ─────────────────────────────────────────────────────────
 * Each mutation below was applied and the suite re-run; every kill is by a named
 * assertion, not a syntax error or a neighbouring guard.
 *   1. Delete the synchronous close (schedule and nothing else — the pre-fix
 *      behaviour): fails tests 1, 2 and 5. Tests 3, 4 and 6 stay green.
 *   2. Call `onOpen` immediately instead of via `setTimeout` (i.e. "the fix is
 *      to delete the debounce"): fails tests 2, 3, 4, 5 and 6. Test 1 stays
 *      green — test 3 is the control that exists for this mutation.
 *   3. `return` after the synchronous `onClose`, so a close cancels the
 *      re-check: fails test 5 ONLY.
 *   4. Reuse the synchronous detection result instead of re-running `detect`
 *      inside the timer: fails tests 4, 5 and 6 ONLY.
 */

import assert from "node:assert/strict";
import { test } from "node:test";

import { updateDebouncedAutocompleteQuery } from "./debouncedAutocompleteQuery.ts";

const DELAY_MS = 30;

function makeSpy() {
  const calls = [];
  const fn = (...args) => {
    calls.push(args);
  };
  fn.calls = calls;
  return fn;
}

/** Wait past `DELAY_MS` so a scheduled OPEN has certainly run. */
function afterTheDelay() {
  return new Promise((resolve) => setTimeout(resolve, DELAY_MS * 4));
}

test("a null detection closes synchronously, before this call returns", () => {
  const debounceTimerRef = { current: null };
  const onClose = makeSpy();
  const onOpen = makeSpy();

  updateDebouncedAutocompleteQuery({
    debounceTimerRef,
    delayMs: DELAY_MS,
    detect: () => null,
    onClose,
    onOpen,
  });

  // No `await` above and none before these assertions: the event loop has not
  // turned, so a debounced close could not possibly have landed yet.
  assert.equal(
    onClose.calls.length,
    1,
    "onClose must run synchronously — a debounced close leaves a window in which Enter is stolen from the composer",
  );
  assert.equal(onOpen.calls.length, 0, "nothing may be opened synchronously");
});

test("a null detection cancels a pending open, synchronously", async () => {
  const debounceTimerRef = { current: null };
  const onClose = makeSpy();
  const onOpen = makeSpy();

  updateDebouncedAutocompleteQuery({
    debounceTimerRef,
    delayMs: DELAY_MS,
    detect: () => ({ query: "gen" }),
    onClose,
    onOpen,
  });
  updateDebouncedAutocompleteQuery({
    debounceTimerRef,
    delayMs: DELAY_MS,
    detect: () => null,
    onClose,
    onOpen,
  });

  assert.equal(
    onClose.calls.length,
    1,
    "the second call must close immediately",
  );

  await afterTheDelay();
  assert.equal(
    onOpen.calls.length,
    0,
    "the first call's pending open must never fire — otherwise the list reappears after the trigger text is gone",
  );
});

test("opening stays debounced", async () => {
  const debounceTimerRef = { current: null };
  const onClose = makeSpy();
  const onOpen = makeSpy();

  updateDebouncedAutocompleteQuery({
    debounceTimerRef,
    delayMs: DELAY_MS,
    detect: () => ({ query: "gen" }),
    onClose,
    onOpen,
  });

  assert.equal(
    onOpen.calls.length,
    0,
    "the open must not be immediate — the debounce is the only reason this scheduler exists",
  );
  assert.notEqual(
    debounceTimerRef.current,
    null,
    "a timer must be pending so callers can cancel or flush it",
  );

  await afterTheDelay();
  assert.equal(onOpen.calls.length, 1, "the open lands once the delay elapses");
  assert.equal(
    debounceTimerRef.current,
    null,
    "the timer clears its own ref when it fires",
  );
});

test("a detection that goes stale before the timer fires closes instead of opening", async () => {
  const debounceTimerRef = { current: null };
  const onClose = makeSpy();
  const onOpen = makeSpy();
  let present = true;

  updateDebouncedAutocompleteQuery({
    debounceTimerRef,
    delayMs: DELAY_MS,
    detect: () => (present ? { query: "gen" } : null),
    onClose,
    onOpen,
  });
  present = false;

  await afterTheDelay();
  assert.equal(onOpen.calls.length, 0, "a stale query must not open a list");
  assert.equal(onClose.calls.length, 1, "the timer closes instead");
});

test("a synchronous close still schedules the re-check, so a query that only becomes detectable later still opens", async () => {
  // `detectPrefixQuery`'s multi-word path returns null outright while the known
  // -name list is empty, so "no query at the cursor" is not always final:
  // `@First Last` becomes detectable the moment the member list lands, with no
  // further keystroke. Returning early after `onClose` would delete that.
  const debounceTimerRef = { current: null };
  const onClose = makeSpy();
  const onOpen = makeSpy();
  let namesLoaded = false;

  updateDebouncedAutocompleteQuery({
    debounceTimerRef,
    delayMs: DELAY_MS,
    detect: () => (namesLoaded ? { query: "First Last" } : null),
    onClose,
    onOpen,
  });

  assert.equal(onClose.calls.length, 1, "it still closes synchronously");
  namesLoaded = true;

  await afterTheDelay();
  assert.deepEqual(
    onOpen.calls[0]?.[0],
    { query: "First Last" },
    "the timer must still have been scheduled — a multi-word name that resolved while it was pending has to open the list without another keystroke",
  );
});

test("detect is re-run when the timer fires, so a late-arriving name list still resolves", async () => {
  // The `#channel` and `@mention` hooks read their known-name list through a
  // ref precisely because it can finish loading between the keystroke and the
  // timer. `detect` is a thunk, and calling it twice is what preserves that.
  const debounceTimerRef = { current: null };
  const onClose = makeSpy();
  const onOpen = makeSpy();

  // Stands in for the async name list: at keystroke time only the single-word
  // form resolves; by the time the timer fires the multi-word name has loaded.
  let knownNames = ["dev"];
  const detect = () =>
    knownNames.includes("dev ops")
      ? { query: "dev ops", startIndex: 4 }
      : { query: "dev", startIndex: 4 };

  updateDebouncedAutocompleteQuery({
    debounceTimerRef,
    delayMs: DELAY_MS,
    detect,
    onClose,
    onOpen,
  });
  knownNames = ["dev", "dev ops"];

  await afterTheDelay();
  assert.equal(onOpen.calls.length, 1, "precondition: the open landed");
  assert.deepEqual(
    onOpen.calls[0][0],
    { query: "dev ops", startIndex: 4 },
    "onOpen must receive the result of the SECOND detection — reusing the synchronous one silently drops multi-word names that resolved while the timer was pending",
  );
});
