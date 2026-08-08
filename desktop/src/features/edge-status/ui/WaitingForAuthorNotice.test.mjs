/**
 * "Waiting for an author" is only true for one of the three reasons an event
 * can be sitting in the outbox, and the other two used to be rendered as if it
 * were:
 *
 *   - `ancestorBlocked` rows belong to an author who IS online and draining.
 *     The sidecar refuses to lease them because an ancestor of theirs never
 *     reached canonical history, so the drain client runs every cycle and
 *     correctly claims nothing. Telling the operator to wait is unactionable;
 *     the ancestor is the thing to act on.
 *   - `pendingViaDigest` rows have no author coming for them at all — this
 *     machine's edge identity carries them upstream. Summing them into a
 *     waiting figure invents a person to wait for.
 *
 * These tests fail if either count is folded back into the waiting story.
 */

import assert from "node:assert/strict";
import { afterEach, beforeEach, test } from "node:test";

import { JSDOM } from "jsdom";

const dom = new JSDOM("<!doctype html><html><body></body></html>", {
  pretendToBeVisual: true,
  url: "http://localhost",
});

Object.assign(globalThis, {
  document: dom.window.document,
  Event: dom.window.Event,
  HTMLElement: dom.window.HTMLElement,
  IS_REACT_ACT_ENVIRONMENT: true,
  window: dom.window,
});

const React = (await import("react")).default;
const { act } = await import("react");
const { createRoot } = await import("react-dom/client");
const { WaitingForAuthorNotice } = await import("./WaitingForAuthorNotice.tsx");

const AUTHOR =
  "44b8e82baaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const OTHER_AUTHOR =
  "9c3f0011aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

const WAITING_LABEL = "Identities with events waiting to sync";
const BLOCKED_LABEL =
  "Events blocked behind an ancestor that never reached canonical history";

const NOW_SECONDS = Math.floor(Date.now() / 1000);

function author(overrides = {}) {
  return {
    author: AUTHOR,
    pending: 0,
    ancestorBlocked: 0,
    pendingViaDigest: 0,
    // The aggregate spans all three buckets, so it is the oldest of the three
    // and belongs to none of them in particular.
    oldestPendingAt: NOW_SECONDS - 7200,
    oldestClaimableAt: NOW_SECONDS - 7200,
    oldestAncestorBlockedAt: NOW_SECONDS - 7200,
    ...overrides,
  };
}

let root = null;
let container = null;

async function render(authors) {
  container = dom.window.document.createElement("div");
  dom.window.document.body.appendChild(container);
  root = createRoot(container);
  await act(async () => {
    root.render(React.createElement(WaitingForAuthorNotice, { authors }));
  });
}

async function unmount() {
  if (!root) return;
  const current = root;
  root = null;
  await act(async () => {
    current.unmount();
  });
  container?.remove();
  container = null;
}

function section(label) {
  return container.querySelector(`section[aria-label="${label}"]`);
}

beforeEach(() => {
  root = null;
  container = null;
});

afterEach(async () => {
  await unmount();
});

test("renders nothing when no identity has anything queued", async () => {
  await render([]);
  assert.equal(container.textContent, "");

  await unmount();
  await render([author()]);
  assert.equal(
    container.textContent,
    "",
    "an author whose three counts are all zero is not waiting for anything",
  );
});

test("an author with claimable rows is shown as waiting", async () => {
  await render([author({ pending: 3 })]);

  const waiting = section(WAITING_LABEL);
  assert.ok(waiting, "the waiting section must be rendered");
  assert.match(waiting.textContent, /3 events queued/);
  assert.match(waiting.textContent, /oldest waiting 2h/);
  assert.equal(section(BLOCKED_LABEL), null);
});

/**
 * The pin: an author with nothing but ancestor-blocked rows is NOT waiting.
 * Rendering them under "waiting for an author to come online" is the bug —
 * their author is already online and claiming nothing, correctly.
 */
test("ancestor-blocked rows never appear as waiting for an author", async () => {
  await render([author({ ancestorBlocked: 4 })]);

  assert.equal(
    section(WAITING_LABEL),
    null,
    "an author who cannot claim is not an author to wait for",
  );
  assert.ok(
    !/Waiting for an author/.test(container.textContent),
    "nothing on screen may tell the operator to wait",
  );

  const blocked = section(BLOCKED_LABEL);
  assert.ok(blocked, "they get their own section instead");
  assert.match(blocked.textContent, /4 events blocked/);
  assert.match(
    blocked.textContent,
    /will not help/,
    "the copy must say waiting is not the fix",
  );
  assert.match(
    blocked.textContent,
    /Retry or discard/,
    "and must name the actionable thing",
  );
});

test("the two counts stay in their own sections, never summed", async () => {
  await render([author({ pending: 2, ancestorBlocked: 3 })]);

  const waiting = section(WAITING_LABEL);
  const blocked = section(BLOCKED_LABEL);
  assert.match(waiting.textContent, /2 events queued/);
  assert.match(blocked.textContent, /3 events blocked/);
  assert.ok(
    !/5 events/.test(container.textContent),
    "2 claimable + 3 blocked is not '5 waiting'",
  );
  assert.ok(
    !/blocked/.test(waiting.textContent),
    "the blocked count must not leak into the waiting list",
  );
});

test("a digest-carried queue is reported apart from any waiting figure", async () => {
  await render([author({ pending: 2, pendingViaDigest: 6 })]);

  const waiting = section(WAITING_LABEL);
  assert.match(waiting.textContent, /2 events queued/);
  assert.ok(
    !/6/.test(waiting.textContent),
    "the digest rows have no author and must not be counted as waiting",
  );
  assert.ok(
    !/8 events/.test(container.textContent),
    "2 claimable + 6 digest-carried is not '8 waiting'",
  );
  assert.match(container.textContent, /6 more events/);
  assert.match(container.textContent, /catch-up digest/);
  assert.match(
    container.textContent,
    /not\s+counted above/,
    "the footnote must say it is outside the figure above",
  );
});

test("a queue that is nothing but digest rows raises no author alarm", async () => {
  await render([author({ pendingViaDigest: 1 })]);

  assert.equal(section(WAITING_LABEL), null);
  assert.equal(section(BLOCKED_LABEL), null);
  assert.match(container.textContent, /1 more event\b/);
  assert.ok(
    !/Waiting for an author/.test(container.textContent),
    "nobody is waiting on an author here",
  );
});

test("each identity is listed under the sections that apply to it", async () => {
  await render([
    author({ pending: 1 }),
    author({ author: OTHER_AUTHOR, ancestorBlocked: 2 }),
  ]);

  const waiting = section(WAITING_LABEL);
  const blocked = section(BLOCKED_LABEL);
  assert.equal(
    waiting.querySelectorAll("li").length,
    1,
    "only the genuinely-waiting identity is listed as waiting",
  );
  assert.equal(blocked.querySelectorAll("li").length, 1);
  assert.match(waiting.textContent, /44b8e82b/);
  assert.match(blocked.textContent, /9c3f0011/);
});

test("the ancestor-blocked age is not labelled as waiting", async () => {
  // These events are not waiting for anybody — their author is online and
  // correctly claiming nothing — so the section must not describe its age as a
  // wait, which would send the operator back to the advice it exists to refuse.
  await render([author({ ancestorBlocked: 2 })]);

  const blocked = section(BLOCKED_LABEL);
  assert.ok(blocked);
  assert.ok(
    !/oldest waiting/i.test(blocked.textContent),
    `the blocked section must not call its age "waiting": ${blocked.textContent}`,
  );
  assert.match(blocked.textContent, /oldest blocked/);
});

/**
 * BUG-023's second half. `oldestPendingAt` is one `MIN(received_at)` over the
 * claimable, blocked and digest rows together, so printing it beside a count
 * claims an age that count need not have. Here the author's claimable row is
 * two minutes old, the blocked row is an hour old, and a digest row nobody is
 * waiting for is a day old — so the aggregate ("1d") is wrong for both
 * sections, and each section has to read its own bucket.
 */
test("each section shows the age of its own rows, not the author's oldest", async () => {
  await render([
    author({
      pending: 1,
      ancestorBlocked: 1,
      pendingViaDigest: 1,
      oldestPendingAt: NOW_SECONDS - 86_400,
      oldestClaimableAt: NOW_SECONDS - 120,
      oldestAncestorBlockedAt: NOW_SECONDS - 3_600,
    }),
  ]);

  assert.match(section(WAITING_LABEL).textContent, /oldest waiting 2m/);
  assert.match(section(BLOCKED_LABEL).textContent, /oldest blocked 1h/);
  assert.ok(
    !/1d/.test(container.textContent),
    `the digest row's age belongs to neither section: ${container.textContent}`,
  );
});

/**
 * `null` means the bucket is empty. A section only renders when its own count
 * is above zero so it should not arise, but if a sidecar ever sends a count
 * without its age the row must drop the AGE and keep the count — never fall
 * back to the aggregate, which is the substitution this fix removed.
 */
test("a missing bucket age drops the age, not the count", async () => {
  await render([
    author({
      pending: 1,
      ancestorBlocked: 1,
      oldestPendingAt: NOW_SECONDS - 86_400,
      oldestClaimableAt: null,
      oldestAncestorBlockedAt: null,
    }),
  ]);

  assert.match(section(WAITING_LABEL).textContent, /1 event queued/);
  assert.match(section(BLOCKED_LABEL).textContent, /1 event blocked/);
  assert.ok(
    !/oldest/i.test(container.textContent),
    `no age may be invented: ${container.textContent}`,
  );
  assert.ok(!/1d/.test(container.textContent));
});
