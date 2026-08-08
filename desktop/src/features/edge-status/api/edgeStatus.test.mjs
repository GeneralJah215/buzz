/**
 * IPC wrapper tests.
 *
 * The edge sidecar is a separately-installed binary that an installer hook
 * upgrades on its own schedule, so it can be a different version than the
 * desktop app talking to it. These tests pin that a skewed sidecar returning
 * garbage produces a named `EdgeStatusShapeError` rather than a half-typed
 * object flowing into the sidebar.
 *
 * `@tauri-apps/api/core` dispatches through `window.__TAURI_INTERNALS__.invoke`,
 * so that is the seam the stub replaces (same idiom as
 * `shared/api/invites.test.mjs`).
 */

import assert from "node:assert/strict";
import test from "node:test";

globalThis.window = globalThis.window ?? {};
globalThis.window.setTimeout ??= setTimeout;
globalThis.window.clearTimeout ??= clearTimeout;

const {
  DEFAULT_QUARANTINE_LIMIT,
  EdgeStatusShapeError,
  fetchEdgeDeliverySummary,
  fetchEdgeEventDeliveryStates,
  fetchEdgeQuarantinedEvents,
  fetchEdgeWaitingAuthors,
  isEdgeDeliveryState,
  isEdgeUnavailableError,
  requeueQuarantinedEvent,
} = await import("./edgeStatus.ts");

/** Install an invoke stub; returns the recorded calls. */
function stubInvoke(handler) {
  const calls = [];
  globalThis.window.__TAURI_INTERNALS__ = {
    invoke: async (command, args) => {
      calls.push({ command, args });
      return handler(command, args);
    },
  };
  return calls;
}

function stubRejection(message) {
  return stubInvoke(() => {
    throw new Error(message);
  });
}

const GOOD_SUMMARY = {
  pending: 3,
  claimed: 1,
  deliveredExact: 12,
  deliveredViaDigest: 2,
  quarantined: 1,
};

const EVENT_ID =
  "8e39cba681211b3782d0e4483e9343719b9b7be66515252da5491f26421896b1";
const AUTHOR =
  "44b8e82baaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const CHANNEL =
  "0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f";

const GOOD_QUARANTINED = {
  eventId: EVENT_ID,
  channelId: CHANNEL,
  author: AUTHOR,
  createdAt: 1_780_000_000,
  attempts: 4,
  reason: "upstream rejected: created_at outside ingest window",
  updatedAt: 1_780_000_600,
};

const GOOD_WAITING = {
  author: AUTHOR,
  pending: 2,
  oldestPendingAt: 1_780_000_000,
};

async function assertShapeError(promiseFactory, expectedFragment) {
  await assert.rejects(promiseFactory, (error) => {
    assert.ok(
      error instanceof EdgeStatusShapeError,
      `expected EdgeStatusShapeError, got ${error?.name}: ${error?.message}`,
    );
    assert.match(error.message, expectedFragment);
    return true;
  });
}

// ── Happy paths + argument marshalling ───────────────────────────────────────

test("delivery summary passes a well-formed payload through", async () => {
  const calls = stubInvoke(() => ({ ...GOOD_SUMMARY }));
  assert.deepEqual(await fetchEdgeDeliverySummary(), GOOD_SUMMARY);
  assert.equal(calls[0].command, "edge_delivery_summary");
});

test("quarantined events sends the limit and parses rows", async () => {
  const calls = stubInvoke(() => [{ ...GOOD_QUARANTINED }]);
  const rows = await fetchEdgeQuarantinedEvents(7);
  assert.deepEqual(rows, [GOOD_QUARANTINED]);
  assert.equal(calls[0].command, "edge_quarantined_events");
  assert.deepEqual(calls[0].args, { limit: 7 });
});

test("quarantined events defaults its limit", async () => {
  const calls = stubInvoke(() => []);
  await fetchEdgeQuarantinedEvents();
  assert.deepEqual(calls[0].args, { limit: DEFAULT_QUARANTINE_LIMIT });
});

test("waiting authors parses rows", async () => {
  stubInvoke(() => [{ ...GOOD_WAITING }]);
  assert.deepEqual(await fetchEdgeWaitingAuthors(), [GOOD_WAITING]);
});

test("event delivery states turn tuples into a lookup", async () => {
  const calls = stubInvoke(() => [
    [EVENT_ID, "pending"],
    [CHANNEL, "syncedViaDigest"],
  ]);
  assert.deepEqual(await fetchEdgeEventDeliveryStates([EVENT_ID, CHANNEL]), {
    [EVENT_ID]: "pending",
    [CHANNEL]: "syncedViaDigest",
  });
  assert.deepEqual(calls[0].args, { eventIds: [EVENT_ID, CHANNEL] });
});

test("event delivery states short-circuits an empty request", async () => {
  const calls = stubInvoke(() => {
    throw new Error("must not be called");
  });
  assert.deepEqual(await fetchEdgeEventDeliveryStates([]), {});
  assert.equal(calls.length, 0);
});

test("requeue returns the sidecar's boolean verdict", async () => {
  const calls = stubInvoke(() => false);
  assert.equal(await requeueQuarantinedEvent(EVENT_ID), false);
  assert.equal(calls[0].command, "edge_requeue_quarantined");
  assert.deepEqual(calls[0].args, { eventId: EVENT_ID });
});

// ── Malformed responses ──────────────────────────────────────────────────────

test("summary rejects a non-object response", async () => {
  stubInvoke(() => "ok");
  await assertShapeError(fetchEdgeDeliverySummary, /expected an object/);
});

test("summary rejects a null response", async () => {
  stubInvoke(() => null);
  await assertShapeError(fetchEdgeDeliverySummary, /got null/);
});

test("summary rejects a missing counter", async () => {
  const { deliveredViaDigest: _dropped, ...partial } = GOOD_SUMMARY;
  stubInvoke(() => partial);
  await assertShapeError(fetchEdgeDeliverySummary, /deliveredViaDigest/);
});

test("summary rejects a stringly-typed counter", async () => {
  stubInvoke(() => ({ ...GOOD_SUMMARY, pending: "3" }));
  await assertShapeError(
    fetchEdgeDeliverySummary,
    /'pending' must be a finite number/,
  );
});

test("summary rejects NaN", async () => {
  stubInvoke(() => ({ ...GOOD_SUMMARY, claimed: Number.NaN }));
  await assertShapeError(fetchEdgeDeliverySummary, /'claimed'/);
});

test("quarantined events reject a non-array response", async () => {
  stubInvoke(() => ({ rows: [] }));
  await assertShapeError(
    () => fetchEdgeQuarantinedEvents(),
    /expected an array/,
  );
});

test("quarantined events reject a row with a missing reason", async () => {
  const { reason: _dropped, ...partial } = GOOD_QUARANTINED;
  stubInvoke(() => [partial]);
  await assertShapeError(
    () => fetchEdgeQuarantinedEvents(),
    /\[0\].*'reason'/s,
  );
});

test("quarantined events reject a numeric attempts count sent as a string", async () => {
  stubInvoke(() => [{ ...GOOD_QUARANTINED, attempts: "4" }]);
  await assertShapeError(() => fetchEdgeQuarantinedEvents(), /'attempts'/);
});

test("waiting authors reject a malformed row", async () => {
  stubInvoke(() => [{ ...GOOD_WAITING }, { author: AUTHOR }]);
  await assertShapeError(fetchEdgeWaitingAuthors, /\[1\].*'pending'/s);
});

test("delivery states reject a tuple of the wrong arity", async () => {
  stubInvoke(() => [[EVENT_ID, "pending", "extra"]]);
  await assertShapeError(
    () => fetchEdgeEventDeliveryStates([EVENT_ID]),
    /expected a \[eventId, state\] pair/,
  );
});

test("delivery states reject an unknown state from a newer sidecar", async () => {
  stubInvoke(() => [[EVENT_ID, "syncedSomehow"]]);
  await assertShapeError(
    () => fetchEdgeEventDeliveryStates([EVENT_ID]),
    /unknown delivery state "syncedSomehow"/,
  );
});

test("delivery states reject a non-string event id", async () => {
  stubInvoke(() => [[7, "pending"]]);
  await assertShapeError(
    () => fetchEdgeEventDeliveryStates([EVENT_ID]),
    /event id must be a non-empty string/,
  );
});

test("requeue rejects a non-boolean verdict", async () => {
  stubInvoke(() => "ok");
  await assertShapeError(
    () => requeueQuarantinedEvent(EVENT_ID),
    /expected a boolean/,
  );
});

// ── Unavailable classification ───────────────────────────────────────────────

test("sidecar-not-running rejections are classified as unavailable", async () => {
  for (const message of [
    "edge sidecar not running",
    "Sidecar is not running",
    "edge relay unavailable",
    "connection refused (127.0.0.1:4848)",
    "community binding does not hold for this relay",
  ]) {
    stubRejection(message);
    const error = await fetchEdgeDeliverySummary().then(
      () => null,
      (caught) => caught,
    );
    assert.ok(error, `${message} must reject`);
    assert.equal(
      isEdgeUnavailableError(error),
      true,
      `'${message}' must read as unavailable`,
    );
  }
});

test("a shape error is never classified as unavailable", async () => {
  // A version-skew bug that reported "unavailable" would be silently hidden.
  stubInvoke(() => ({ ...GOOD_SUMMARY, pending: "not running" }));
  const error = await fetchEdgeDeliverySummary().then(
    () => null,
    (caught) => caught,
  );
  assert.ok(error instanceof EdgeStatusShapeError);
  assert.equal(isEdgeUnavailableError(error), false);
});

test("an unrelated rejection is not classified as unavailable", () => {
  assert.equal(isEdgeUnavailableError(new Error("sqlite is locked")), false);
  assert.equal(isEdgeUnavailableError(""), false);
  assert.equal(isEdgeUnavailableError(undefined), false);
});

test("a bare string rejection is still classified", () => {
  assert.equal(isEdgeUnavailableError("edge sidecar not running"), true);
});

// ── State guard ──────────────────────────────────────────────────────────────

test("isEdgeDeliveryState accepts only the five contract states", () => {
  for (const state of [
    "pending",
    "claimed",
    "syncedExact",
    "syncedViaDigest",
    "quarantined",
  ]) {
    assert.equal(isEdgeDeliveryState(state), true);
  }
  for (const bogus of ["sent", "SYNCEDEXACT", "", null, 1, {}]) {
    assert.equal(isEdgeDeliveryState(bogus), false);
  }
});
