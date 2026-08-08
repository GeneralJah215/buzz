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
import { readFileSync } from "node:fs";
import test from "node:test";

globalThis.window = globalThis.window ?? {};
globalThis.window.setTimeout ??= setTimeout;
globalThis.window.clearTimeout ??= clearTimeout;

const {
  DEFAULT_QUARANTINE_LIMIT,
  EDGE_DELIVERY_STATE_WIRE_NAMES,
  EDGE_DELIVERY_STATES,
  EDGE_REQUEUE_OUTCOME_WIRE_NAMES,
  EDGE_UNAVAILABLE_MESSAGE,
  EdgeStatusShapeError,
  edgeDeliveryStateKey,
  edgeRequeueOutcomeKey,
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
  pendingViaDigest: 17,
  claimed: 1,
  syncedExact: 12,
  syncedViaDigest: 2,
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
  carriedByDigest: false,
  demotionReason: null,
  updatedAt: 1_780_000_600,
};

const GOOD_WAITING = {
  author: AUTHOR,
  pending: 2,
  ancestorBlocked: 5,
  pendingViaDigest: 9,
  oldestPendingAt: 1_780_000_000,
};

/** One delivery-state row in the sidecar's object shape. */
function deliveryRow(eventId, state, demotionReason = null) {
  return { eventId, state, demotionReason };
}

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

test("event delivery states turn rows into a lookup", async () => {
  const calls = stubInvoke(() => [
    deliveryRow(EVENT_ID, "pending"),
    deliveryRow(CHANNEL, "syncedViaDigest"),
  ]);
  assert.deepEqual(await fetchEdgeEventDeliveryStates([EVENT_ID, CHANNEL]), {
    [EVENT_ID]: { state: "pending", demotionReason: null },
    [CHANNEL]: { state: "syncedViaDigest", demotionReason: null },
  });
  assert.deepEqual(calls[0].args, { eventIds: [EVENT_ID, CHANNEL] });
});

/**
 * The reason a row left the exact path has to arrive WITH the label, not be
 * inferable from it. `pendingViaDigest` says where the event went; only the
 * reason says whether that is routine ("older than the relay drift window") or
 * something to act on ("permanently rejected upstream"), and the badge cannot
 * show what the parser threw away.
 */
test("a demoted row carries the reason it was demoted", async () => {
  stubInvoke(() => [
    deliveryRow(
      EVENT_ID,
      "pendingViaDigest",
      "older than the relay drift window",
    ),
    deliveryRow(CHANNEL, "pending"),
  ]);

  const lookup = await fetchEdgeEventDeliveryStates([EVENT_ID, CHANNEL]);
  assert.equal(lookup[EVENT_ID].state, "pendingViaDigest");
  assert.equal(
    lookup[EVENT_ID].demotionReason,
    "older than the relay drift window",
  );
  assert.equal(
    lookup[CHANNEL].demotionReason,
    null,
    "a row that was never demoted reports null, not a borrowed reason",
  );
});

/**
 * `null` is the sidecar saying "never demoted"; an absent key is a sidecar too
 * old to answer. Collapsing the two would render a blank reason for every
 * deferred event and look exactly like a normal undemoted row.
 */
test("a delivery row without a demotionReason key is a shape error", async () => {
  stubInvoke(() => [{ eventId: EVENT_ID, state: "pending" }]);
  await assertShapeError(
    () => fetchEdgeEventDeliveryStates([EVENT_ID]),
    /'demotionReason' is missing/,
  );
});

/**
 * The pair shape is gone. A parser still reading `[eventId, state]` would be
 * discarding `demotionReason` on every row without a word.
 */
test("the old [eventId, state] pair shape is refused", async () => {
  stubInvoke(() => [[EVENT_ID, "pending"]]);
  await assertShapeError(
    () => fetchEdgeEventDeliveryStates([EVENT_ID]),
    /expected an object, got array/,
  );
});

test("event delivery states short-circuits an empty request", async () => {
  const calls = stubInvoke(() => {
    throw new Error("must not be called");
  });
  assert.deepEqual(await fetchEdgeEventDeliveryStates([]), {});
  assert.equal(calls.length, 0);
});

test("requeue returns the sidecar's verdict AND which verdict it was", async () => {
  const calls = stubInvoke(() => ({
    requeued: false,
    outcome: "carriedByDigest",
  }));
  assert.deepEqual(await requeueQuarantinedEvent(EVENT_ID), {
    requeued: false,
    outcome: "carriedByDigest",
  });
  assert.equal(calls[0].command, "edge_requeue_quarantined");
  assert.deepEqual(calls[0].args, { eventId: EVENT_ID });
});

/**
 * The bare boolean is the pre-rename shape and must not be accepted: it cannot
 * tell "no such row of yours" from "the edge is already carrying this row
 * upstream, stop pressing Retry", which is exactly the confusion the outcome
 * was added to end.
 */
test("a bare boolean requeue reply is refused", async () => {
  for (const verdict of [true, false]) {
    stubInvoke(() => verdict);
    await assertShapeError(
      () => requeueQuarantinedEvent(EVENT_ID),
      /expected an object, got boolean/,
    );
  }

  stubInvoke(() => ({ requeued: false }));
  await assertShapeError(
    () => requeueQuarantinedEvent(EVENT_ID),
    /'outcome' must be a string/,
  );
});

/** A newer sidecar's fourth outcome reaches the caller, it does not throw. */
test("an unrecognised requeue outcome is passed through", async () => {
  stubInvoke(() => ({ requeued: false, outcome: "deferredUntilWindowOpens" }));
  assert.deepEqual(await requeueQuarantinedEvent(EVENT_ID), {
    requeued: false,
    outcome: "deferredUntilWindowOpens",
  });
  assert.equal(edgeRequeueOutcomeKey("deferredUntilWindowOpens"), null);
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

/**
 * Every counter is required, including the three whose absence would read as
 * "nothing on that path" — the most reassuring possible lie from a sidecar
 * this build cannot actually understand.
 */
test("summary rejects a missing counter", async () => {
  for (const dropped of Object.keys(GOOD_SUMMARY)) {
    const partial = { ...GOOD_SUMMARY };
    delete partial[dropped];
    stubInvoke(() => partial);
    await assertShapeError(
      fetchEdgeDeliverySummary,
      new RegExp(`'${dropped}'`),
    );
  }
});

/**
 * The counters are six independent numbers, not one number split six ways. A
 * crossed pair would render a calm summary over a stuck outbox, so each key is
 * asserted to land in its own field.
 */
test("every summary counter reads its own key", async () => {
  const distinct = {
    pending: 1,
    pendingViaDigest: 2,
    claimed: 3,
    syncedExact: 4,
    syncedViaDigest: 5,
    quarantined: 6,
  };
  stubInvoke(() => ({ ...distinct }));
  assert.deepEqual(await fetchEdgeDeliverySummary(), distinct);
});

test("summary rejects a stringly-typed counter", async () => {
  stubInvoke(() => ({ ...GOOD_SUMMARY, pending: "3" }));
  await assertShapeError(
    fetchEdgeDeliverySummary,
    /'pending' must be a non-negative whole number/,
  );
});

test("summary rejects NaN", async () => {
  stubInvoke(() => ({ ...GOOD_SUMMARY, claimed: Number.NaN }));
  await assertShapeError(fetchEdgeDeliverySummary, /'claimed'/);
});

test("summary rejects a negative count", async () => {
  // "-1 quarantined" is not a small number of stuck events, it is a broken
  // producer, and rendering it would put a nonsense figure in front of the
  // operator. Rust sends these as u64, so a negative can only be a contract
  // break.
  stubInvoke(() => ({ ...GOOD_SUMMARY, quarantined: -1 }));
  await assertShapeError(
    fetchEdgeDeliverySummary,
    /'quarantined' must be a non-negative whole number, got -1/,
  );
});

test("summary rejects a fractional count", async () => {
  stubInvoke(() => ({ ...GOOD_SUMMARY, pending: 2.5 }));
  await assertShapeError(fetchEdgeDeliverySummary, /'pending'.*got 2\.5/);
});

test("summary rejects Infinity", async () => {
  stubInvoke(() => ({ ...GOOD_SUMMARY, claimed: Number.POSITIVE_INFINITY }));
  await assertShapeError(fetchEdgeDeliverySummary, /'claimed'/);
});

test("a quarantine row rejects a negative attempt count", async () => {
  stubInvoke(() => [{ ...GOOD_QUARANTINED, attempts: -3 }]);
  await assertShapeError(
    () => fetchEdgeQuarantinedEvents(),
    /'attempts' must be a non-negative whole number, got -3/,
  );
});

test("a waiting-author row rejects a fractional pending count", async () => {
  stubInvoke(() => [{ ...GOOD_WAITING, pending: 1.5 }]);
  await assertShapeError(fetchEdgeWaitingAuthors, /'pending'.*got 1\.5/);
});

test("timestamps must be whole seconds but may predate the epoch", async () => {
  // A timestamp is not a count: negative is nonsense data the age formatter
  // already clamps, while a fractional 'unix second' is a shape break.
  stubInvoke(() => [{ ...GOOD_QUARANTINED, createdAt: 1_780_000_000.5 }]);
  await assertShapeError(
    () => fetchEdgeQuarantinedEvents(),
    /'createdAt' must be a whole number of unix seconds/,
  );

  stubInvoke(() => [{ ...GOOD_QUARANTINED, createdAt: -5 }]);
  const [row] = await fetchEdgeQuarantinedEvents();
  assert.equal(row.createdAt, -5);
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

/**
 * The three counts are three different stalls and only `pending` is fixed by
 * the author returning. A build that dropped `ancestorBlocked` would go back to
 * telling the operator to wait for an author whose drain client is running and
 * correctly claiming nothing; one that dropped `pendingViaDigest` would leave
 * a queue with no explanation on screen.
 */
test("a waiting-author row must carry all three stall counts", async () => {
  for (const dropped of ["pending", "ancestorBlocked", "pendingViaDigest"]) {
    const partial = { ...GOOD_WAITING };
    delete partial[dropped];
    stubInvoke(() => [partial]);
    await assertShapeError(fetchEdgeWaitingAuthors, new RegExp(`'${dropped}'`));
  }

  stubInvoke(() => [
    { ...GOOD_WAITING, pending: 1, ancestorBlocked: 2, pendingViaDigest: 3 },
  ]);
  const [row] = await fetchEdgeWaitingAuthors();
  assert.equal(row.pending, 1);
  assert.equal(row.ancestorBlocked, 2);
  assert.equal(row.pendingViaDigest, 3);
});

/**
 * `carriedByDigest` is the flag that tells the quarantine list its Retry button
 * is a lie on this row — the sidecar refuses that retry outright. A missing
 * flag quietly defaulting to `false` would re-arm the button, so it is a shape
 * error, and both values have to survive the parse.
 */
test("a quarantine row must say whether the digest already carries it", async () => {
  const { carriedByDigest: _dropped, ...partial } = GOOD_QUARANTINED;
  stubInvoke(() => [partial]);
  await assertShapeError(
    () => fetchEdgeQuarantinedEvents(),
    /'carriedByDigest' must be a boolean/,
  );

  for (const carried of [true, false]) {
    stubInvoke(() => [{ ...GOOD_QUARANTINED, carriedByDigest: carried }]);
    const [row] = await fetchEdgeQuarantinedEvents();
    assert.equal(row.carriedByDigest, carried);
  }
});

test("a quarantine row's demotionReason may be null but not absent", async () => {
  const { demotionReason: _dropped, ...partial } = GOOD_QUARANTINED;
  stubInvoke(() => [partial]);
  await assertShapeError(
    () => fetchEdgeQuarantinedEvents(),
    /'demotionReason' is missing/,
  );

  stubInvoke(() => [
    { ...GOOD_QUARANTINED, demotionReason: "permanently rejected upstream" },
  ]);
  const [row] = await fetchEdgeQuarantinedEvents();
  assert.equal(row.demotionReason, "permanently rejected upstream");

  stubInvoke(() => [{ ...GOOD_QUARANTINED, demotionReason: 7 }]);
  await assertShapeError(
    () => fetchEdgeQuarantinedEvents(),
    /'demotionReason' must be a string or null/,
  );
});

test("delivery states reject a row that is not an object", async () => {
  stubInvoke(() => [[EVENT_ID, "pending", "extra"]]);
  await assertShapeError(
    () => fetchEdgeEventDeliveryStates([EVENT_ID]),
    /expected an object, got array/,
  );
});

/**
 * Inverted on purpose (was "delivery states reject an unknown state from a
 * newer sidecar"). Throwing discarded the whole batch — 500 badges lost to one
 * row — and made `UNKNOWN_DELIVERY_LABEL` unreachable for real data. The
 * sidecar gains states on its own release schedule, so this WILL happen.
 */
test("an unknown state costs one badge, not the whole batch", async () => {
  const THIRD_ID = `1${EVENT_ID.slice(1)}`;
  stubInvoke(() => [
    deliveryRow(EVENT_ID, "pending"),
    deliveryRow(CHANNEL, "syncedSomehowInTheFuture"),
    deliveryRow(THIRD_ID, "quarantined"),
  ]);

  const lookup = await fetchEdgeEventDeliveryStates([
    EVENT_ID,
    CHANNEL,
    THIRD_ID,
  ]);

  assert.equal(
    lookup[EVENT_ID].state,
    "pending",
    "known rows before must survive",
  );
  assert.equal(lookup[THIRD_ID].state, "quarantined", "and known rows after");
  assert.equal(
    Object.hasOwn(lookup, CHANNEL),
    false,
    "the unreadable row is omitted, so the badge falls back to 'unknown'",
  );
});

test("a batch of nothing but unknown states resolves empty", async () => {
  stubInvoke(() => [
    deliveryRow(EVENT_ID, "brandNew"),
    deliveryRow(CHANNEL, "alsoBrandNew"),
  ]);
  assert.deepEqual(
    await fetchEdgeEventDeliveryStates([EVENT_ID, CHANNEL]),
    {},
    "an unreadable batch is empty, never a rejection",
  );
});

test("delivery states still reject a structurally malformed row", async () => {
  // Member types are a contract break with no safe reading; only an
  // unrecognised state STRING is tolerated.
  stubInvoke(() => [deliveryRow(EVENT_ID, 7)]);
  await assertShapeError(
    () => fetchEdgeEventDeliveryStates([EVENT_ID]),
    /'state' must be a string, got number/,
  );

  stubInvoke(() => [deliveryRow(EVENT_ID, null)]);
  await assertShapeError(
    () => fetchEdgeEventDeliveryStates([EVENT_ID]),
    /'state' must be a string, got null/,
  );

  stubInvoke(() => [EVENT_ID]);
  await assertShapeError(
    () => fetchEdgeEventDeliveryStates([EVENT_ID]),
    /expected an object, got string/,
  );
});

test("delivery states reject a non-string event id", async () => {
  stubInvoke(() => [deliveryRow(7, "pending")]);
  await assertShapeError(
    () => fetchEdgeEventDeliveryStates([EVENT_ID]),
    /'eventId' must be a string/,
  );

  stubInvoke(() => [deliveryRow("", "pending")]);
  await assertShapeError(
    () => fetchEdgeEventDeliveryStates([EVENT_ID]),
    /'eventId' must be a non-empty string/,
  );
});

test("an event id of '__proto__' is kept as data, not swallowed", async () => {
  // Plain assignment would hit the prototype setter: the row would vanish
  // without a word (and take the object's prototype with it).
  stubInvoke(() => [
    deliveryRow("__proto__", "quarantined"),
    deliveryRow(EVENT_ID, "pending"),
  ]);

  const lookup = await fetchEdgeEventDeliveryStates(["__proto__", EVENT_ID]);

  assert.equal(Object.hasOwn(lookup, "__proto__"), true, "kept as a real key");
  assert.equal(lookup.__proto__.state, "quarantined");
  assert.equal(lookup[EVENT_ID].state, "pending");
  assert.deepEqual(Object.keys(lookup).sort(), ["__proto__", EVENT_ID].sort());
  assert.equal(
    Object.getPrototypeOf(lookup),
    Object.prototype,
    "and the prototype is untouched",
  );
});

test("requeue rejects a non-object verdict", async () => {
  stubInvoke(() => "ok");
  await assertShapeError(
    () => requeueQuarantinedEvent(EVENT_ID),
    /expected an object/,
  );
});

// ── Unavailable classification ───────────────────────────────────────────────

test("only the exact sidecar-not-running sentinel is unavailable", async () => {
  stubRejection(EDGE_UNAVAILABLE_MESSAGE);
  const error = await fetchEdgeDeliverySummary().then(
    () => null,
    (caught) => caught,
  );
  assert.ok(error, "the sentinel must still reject");
  assert.equal(isEdgeUnavailableError(error), true);
});

/**
 * Inverted on purpose (was "sidecar-not-running rejections are classified as
 * unavailable", which accepted all of these by substring). Classifying a real
 * fault as "unavailable" made the status surface go silent with `error ===
 * null` — the operator is told nothing is wrong while events are stuck.
 */
test("real faults are errors, never silence", async () => {
  for (const message of [
    "relay returned 503 Service Unavailable",
    "relay returned 421 Misdirected Request: canonical relay/community binding mismatch",
    "edge database community binding mismatch",
    "sqlite: disk I/O error, store unavailable",
    "connection refused (127.0.0.1:4848)",
    "edge sidecar not running yet, retrying",
    "Edge Sidecar Not Running",
  ]) {
    stubRejection(message);
    const error = await fetchEdgeDeliverySummary().then(
      () => null,
      (caught) => caught,
    );
    assert.ok(error, `${message} must reject`);
    assert.equal(
      isEdgeUnavailableError(error),
      false,
      `'${message}' is a real fault and must reach the operator`,
    );
  }
});

/**
 * The classification above is only safe because it matches a string Rust
 * actually produces. Read it out of the Rust source rather than trusting a
 * copy: if `EDGE_UNAVAILABLE` is reworded, this fails here instead of the
 * whole surface silently switching to "always an error".
 */
test("the sentinel is exactly the Rust EDGE_UNAVAILABLE constant", () => {
  const rustPath = new URL(
    "../../../../src-tauri/src/relay/edge.rs",
    import.meta.url,
  );
  const source = readFileSync(rustPath, "utf8");
  const match = source.match(
    /pub const EDGE_UNAVAILABLE:\s*&str\s*=\s*"([^"]*)"/,
  );
  assert.ok(
    match,
    "could not find `pub const EDGE_UNAVAILABLE` in relay/edge.rs — if it moved, re-point this test",
  );
  assert.equal(
    EDGE_UNAVAILABLE_MESSAGE,
    match[1],
    "the TypeScript sentinel and the Rust constant must be the same string",
  );
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

test("isEdgeDeliveryState accepts only the six contract states", () => {
  for (const state of [
    "pending",
    "pendingViaDigest",
    "claimed",
    "syncedExact",
    "syncedViaDigest",
    "quarantined",
  ]) {
    assert.equal(isEdgeDeliveryState(state), true);
  }
  for (const bogus of [
    "sent",
    "SYNCEDEXACT",
    "",
    "deliveredExact",
    "deliveredViaDigest",
    null,
    1,
    {},
  ]) {
    assert.equal(isEdgeDeliveryState(bogus), false);
  }
});

/**
 * The wire contract with the sidecar, pinned literal by literal. A rename on
 * either side must fail HERE — loudly, in one place — rather than reaching the
 * timeline as six silently-unknown badges.
 *
 * If the sidecar does rename a state, the fix is one line in
 * `EDGE_DELIVERY_STATE_WIRE_NAMES` (api/edgeStatus.ts) plus this pin. Nothing
 * else in the feature spells a wire state out.
 *
 * The literals here must match `EventDeliveryState` in
 * `crates/buzz-edge/src/storage_status.rs`, which is the authority.
 */
test("the six delivery-state wire strings are pinned", () => {
  assert.deepEqual(
    { ...EDGE_DELIVERY_STATE_WIRE_NAMES },
    {
      pending: "pending",
      pendingViaDigest: "pendingViaDigest",
      claimed: "claimed",
      syncedExact: "syncedExact",
      syncedViaDigest: "syncedViaDigest",
      quarantined: "quarantined",
    },
  );
  assert.deepEqual(
    [...EDGE_DELIVERY_STATES],
    [
      "pending",
      "pendingViaDigest",
      "claimed",
      "syncedExact",
      "syncedViaDigest",
      "quarantined",
    ],
    "order is local-only → terminal, and every wire value is listed",
  );
});

/**
 * The pre-rename spellings are gone from the vocabulary. Keeping them readable
 * "for compatibility" would let a stale sidecar's `deliveredExact` land on a
 * synced badge, which is the version-skew silence this whole file exists to
 * prevent.
 */
test("the retired delivered* spellings are not still accepted", () => {
  for (const retired of ["deliveredExact", "deliveredViaDigest"]) {
    assert.equal(edgeDeliveryStateKey(retired), null, retired);
    assert.equal(
      Object.values(EDGE_DELIVERY_STATE_WIRE_NAMES).includes(retired),
      false,
      `${retired} must not be a wire value any more`,
    );
  }
});

/**
 * The three requeue outcomes, pinned the same way and for the same reason: the
 * quarantine list branches on them to decide between "sent back to the queue",
 * "look again", and "nothing is wrong, stop clicking".
 *
 * These must match `RequeueOutcome` in `crates/buzz-edge/src/storage_status.rs`.
 */
test("the three requeue-outcome wire strings are pinned", () => {
  assert.deepEqual(
    { ...EDGE_REQUEUE_OUTCOME_WIRE_NAMES },
    {
      requeued: "requeued",
      notFound: "notFound",
      carriedByDigest: "carriedByDigest",
    },
  );
  for (const [key, wire] of Object.entries(EDGE_REQUEUE_OUTCOME_WIRE_NAMES)) {
    assert.equal(edgeRequeueOutcomeKey(wire), key);
  }
  assert.equal(edgeRequeueOutcomeKey("declined"), null);
  assert.equal(edgeRequeueOutcomeKey(null), null);
});

test("wire strings translate to internal keys and back", () => {
  for (const [key, wire] of Object.entries(EDGE_DELIVERY_STATE_WIRE_NAMES)) {
    assert.equal(edgeDeliveryStateKey(wire), key);
  }
  assert.equal(edgeDeliveryStateKey("syncedSomehow"), null);
  assert.equal(edgeDeliveryStateKey(null), null);
});

test("the two synced states are two states, not one", () => {
  // Success criterion (3): an exact replay and a digest replay must never
  // collapse into a single "sent".
  assert.notEqual(
    EDGE_DELIVERY_STATE_WIRE_NAMES.syncedExact,
    EDGE_DELIVERY_STATE_WIRE_NAMES.syncedViaDigest,
  );
  assert.equal(new Set(EDGE_DELIVERY_STATES).size, 6);
});

test("the two pending paths are two states, not one", () => {
  // A `pendingViaDigest` row has no author coming for it. Sharing a wire value
  // with `pending` would put it back into a "waiting for an author" figure.
  assert.notEqual(
    EDGE_DELIVERY_STATE_WIRE_NAMES.pending,
    EDGE_DELIVERY_STATE_WIRE_NAMES.pendingViaDigest,
  );
});
