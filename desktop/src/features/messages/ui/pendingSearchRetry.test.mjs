/**
 * Guardrails for the main timeline's pending search-target retry (BUG-058).
 *
 * The retry used to have no exit: `pendingSearchTargetRef` held any id whose
 * `scrollToMessage` failed, and re-attempted it on every render pass. For a
 * thread reply — which the main timeline never renders — that is an attempt
 * that can never succeed, so the ref never cleared and Enter never did
 * anything.
 *
 * "Can this ever be rendered" is answerable from the loaded messages. These
 * tests pin that an unanswerable target is abandoned on the spot, and that the
 * two legitimate waits (virtualizer window, DOM not painted yet) still happen.
 * None of this is time-based: no test here waits for anything, so there is no
 * timeout for a later change to quietly widen.
 */

import assert from "node:assert/strict";
import { describe, test } from "node:test";

const {
  decidePendingSearchRetry,
  describeAbandonedSearchTarget,
  isRenderableInMainTimeline,
} = await import("./pendingSearchRetry.ts");

const CHANNEL_ID = "11111111-2222-3333-4444-555555555555";

function makeMessage(id, overrides = {}) {
  return {
    id,
    createdAt: 1_700_000_000,
    author: "tester",
    time: "12:00",
    body: `body of ${id}`,
    depth: 0,
    parentId: null,
    rootId: null,
    kind: 9,
    tags: [["h", CHANNEL_ID]],
    ...overrides,
  };
}

const LOADED = [
  makeMessage("root"),
  makeMessage("reply", { parentId: "root", rootId: "root" }),
  makeMessage("shout", {
    parentId: "root",
    rootId: "root",
    tags: [
      ["h", CHANNEL_ID],
      ["broadcast", "1"],
    ],
  }),
];

describe("isRenderableInMainTimeline", () => {
  test("a loaded root has a row", () => {
    assert.equal(isRenderableInMainTimeline("root", LOADED), true);
  });

  test("a loaded broadcast reply has a row", () => {
    assert.equal(isRenderableInMainTimeline("shout", LOADED), true);
  });

  test("a loaded thread reply has no row and never will", () => {
    assert.equal(
      isRenderableInMainTimeline("reply", LOADED),
      false,
      "this is the id the retry used to hold forever",
    );
  });

  test("an id outside the loaded window has no row", () => {
    assert.equal(isRenderableInMainTimeline("gone", LOADED), false);
  });
});

describe("decidePendingSearchRetry", () => {
  test("abandons a target with no main-timeline row", () => {
    assert.equal(
      decidePendingSearchRetry({
        isRenderable: false,
        isRowInDom: false,
        isVirtualized: true,
      }),
      "abandon",
      "a thread reply has no main-timeline row; retrying for it never terminates",
    );
  });

  test("abandons an unrenderable target in a non-virtualized list too", () => {
    assert.equal(
      decidePendingSearchRetry({
        isRenderable: false,
        isRowInDom: false,
        isVirtualized: false,
      }),
      "abandon",
    );
  });

  test("abandons even when a stale row with that id is still in the DOM", () => {
    assert.equal(
      decidePendingSearchRetry({
        isRenderable: false,
        isRowInDom: true,
        isVirtualized: true,
      }),
      "abandon",
      "the loaded message set decides, not whatever is currently painted",
    );
  });

  test("asks the virtualizer to realize a renderable row that is windowed out", () => {
    assert.equal(
      decidePendingSearchRetry({
        isRenderable: true,
        isRowInDom: false,
        isVirtualized: true,
      }),
      "realize-index",
    );
  });

  test("scrolls a renderable row that is already mounted", () => {
    assert.equal(
      decidePendingSearchRetry({
        isRenderable: true,
        isRowInDom: true,
        isVirtualized: true,
      }),
      "scroll",
    );
  });

  test("scrolls a renderable row when the list is not virtualized", () => {
    assert.equal(
      decidePendingSearchRetry({
        isRenderable: true,
        isRowInDom: false,
        isVirtualized: false,
      }),
      "scroll",
      "without a virtualizer there is no index to realize — the row is one render away",
    );
  });
});

describe("describeAbandonedSearchTarget", () => {
  test("names the abandoned id and carries the guardrail prefix", () => {
    const message = describeAbandonedSearchTarget("abc123");

    assert.match(message, /^\[GUARDRAIL\]/);
    assert.match(
      message,
      /abc123/,
      "a give-up that does not say which target it gave up on is not a report",
    );
  });
});
