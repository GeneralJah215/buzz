/**
 * Delivery-label tests.
 *
 * The load-bearing assertion in this file is that "delivered locally" and
 * "synced to canonical history" never collapse into one word. SPEC-2026-08-05
 * success criterion (3) makes the separation a shipping requirement, and the
 * cheapest way to violate it is a well-meaning refactor that maps `pending`
 * and `syncedExact` to the same friendly string ("sent", "delivered").
 */

import assert from "node:assert/strict";
import test from "node:test";

import {
  EDGE_DELIVERY_STATE_KEYS,
  EDGE_DELIVERY_STATE_WIRE_NAMES as WIRE,
  EDGE_DELIVERY_STATES,
} from "../api/edgeStatus.ts";
import {
  allDeliveryLabels,
  coerceDeliveryState,
  deliveryDescription,
  deliveryLabel,
  deliveryTone,
  formatQueuedAge,
  isCarriedByDigest,
  isLocalOnly,
  isSyncedToCanonicalHistory,
  UNKNOWN_DELIVERY_LABEL,
  UNKNOWN_DELIVERY_TONE,
} from "./deliveryState.ts";

// The tone vocabulary the repo's Badge component actually ships.
const BADGE_VARIANTS = new Set([
  "default",
  "secondary",
  "outline",
  "destructive",
  "warning",
  "success",
  "info",
]);

test("every delivery state maps to a distinct label", () => {
  const labels = EDGE_DELIVERY_STATES.map((state) => deliveryLabel(state));
  assert.equal(
    new Set(labels).size,
    EDGE_DELIVERY_STATES.length,
    `labels must be one-to-one with states, got ${JSON.stringify(labels)}`,
  );
});

/**
 * This file talks about states through `EDGE_DELIVERY_STATE_WIRE_NAMES`, never
 * by spelling a wire string. The literal strings are pinned once, in
 * `api/edgeStatus.test.mjs`; here we pin the INTERNAL names, which a sidecar
 * rename must never move.
 */
test("all six contract states are covered (no state falls through to unknown)", () => {
  assert.deepEqual(
    [...EDGE_DELIVERY_STATE_KEYS],
    [
      "pending",
      "pendingViaDigest",
      "claimed",
      "syncedExact",
      "syncedViaDigest",
      "quarantined",
    ],
  );
  for (const state of EDGE_DELIVERY_STATES) {
    assert.notEqual(
      deliveryLabel(state),
      UNKNOWN_DELIVERY_LABEL,
      `${state} must have a real label`,
    );
  }
});

test("delivered-locally is never conflated with synced", () => {
  const local = deliveryLabel(WIRE.pending);
  assert.equal(local, "Delivered locally");

  for (const synced of [WIRE.syncedExact, WIRE.syncedViaDigest]) {
    assert.notEqual(
      deliveryLabel(synced),
      local,
      `${synced} must not reuse the local-delivery label`,
    );
  }

  // The local label must not claim canonical history, and the synced labels
  // must not claim mere local delivery.
  assert.ok(!/synced|history/i.test(local));
  assert.ok(/synced/i.test(deliveryLabel(WIRE.syncedExact)));
  assert.ok(/synced/i.test(deliveryLabel(WIRE.syncedViaDigest)));
});

/**
 * A `pendingViaDigest` row is queued with NO author coming for it: this
 * machine's edge identity carries it upstream. Wearing `pending`'s label would
 * put it straight back into the "waiting for an author" story that the split
 * exists to end.
 */
test("a digest-carried row does not read as waiting for its author", () => {
  const deferred = deliveryLabel(WIRE.pendingViaDigest);
  assert.notEqual(deferred, deliveryLabel(WIRE.pending));
  assert.notEqual(deferred, deliveryLabel(WIRE.claimed));
  assert.notEqual(deferred, UNKNOWN_DELIVERY_LABEL);
  // It has not reached canonical history, so it must not claim to have.
  assert.equal(isSyncedToCanonicalHistory(WIRE.pendingViaDigest), false);
  assert.equal(isCarriedByDigest(WIRE.pendingViaDigest), true);
  for (const other of EDGE_DELIVERY_STATES) {
    if (other === WIRE.pendingViaDigest) continue;
    assert.equal(
      isCarriedByDigest(other),
      false,
      `${other} is not carried by the digest`,
    );
  }
});

/**
 * "Sync deferred" tells the operator WHERE the event went; only the reason
 * says whether that is routine or a problem, and a grey badge with no reason
 * is barely better than no badge at all.
 */
test("a demotion reason reaches the description the badge shows", () => {
  const bare = deliveryDescription(WIRE.pendingViaDigest);
  const withReason = deliveryDescription(
    WIRE.pendingViaDigest,
    "permanently rejected upstream",
  );
  assert.notEqual(withReason, bare, "the reason must change what is shown");
  assert.match(withReason, /permanently rejected upstream/);
  assert.ok(withReason.startsWith(bare), "and must not replace the meaning");

  // Nothing to add, nothing added.
  assert.equal(deliveryDescription(WIRE.pendingViaDigest, null), bare);
  assert.equal(deliveryDescription(WIRE.pendingViaDigest, "   "), bare);
  // Even an unreadable state still surfaces the reason it came with.
  assert.match(
    deliveryDescription("brandNewState", "older than the relay drift window"),
    /older than the relay drift window/,
  );
});

test("exact and digest sync stay distinguishable", () => {
  assert.notEqual(
    deliveryLabel(WIRE.syncedExact),
    deliveryLabel(WIRE.syncedViaDigest),
  );
  assert.ok(/digest/i.test(deliveryLabel(WIRE.syncedViaDigest)));
});

test("quarantined reads as a failure, not as delivered", () => {
  const label = deliveryLabel(WIRE.quarantined);
  assert.equal(label, "Sync failed");
  assert.notEqual(label, deliveryLabel(WIRE.syncedExact));
});

/**
 * BUG-023. The quarantine list flags a row `carriedByDigest` and drops its
 * Retry button, because the sidecar refuses that retry: the edge identity is
 * already carrying the event upstream in a catch-up digest. The badge for the
 * same event could only say "Sync failed" — the same words a genuinely stuck
 * row gets, and the opposite advice.
 */
test("a quarantined row the digest is carrying does not read as stuck", () => {
  const stuck = deliveryLabel(WIRE.quarantined);
  const carried = deliveryLabel(WIRE.quarantined, true);

  assert.notEqual(
    carried,
    stuck,
    "the two rows need different words, or the badge cannot say which is which",
  );
  // Both halves have to be there: the replay failed AND the digest has it.
  assert.match(carried, /failed/i, "the exact replay did fail; do not hide it");
  assert.match(carried, /digest/i, "and the digest is carrying it");

  // Red is this app's "act now" colour and there is nothing to act on.
  assert.equal(deliveryTone(WIRE.quarantined), "destructive");
  assert.notEqual(deliveryTone(WIRE.quarantined, true), "destructive");
  assert.ok(BADGE_VARIANTS.has(deliveryTone(WIRE.quarantined, true)));

  // The hover copy says what to do, which is nothing.
  const description = deliveryDescription(
    WIRE.quarantined,
    "permanently rejected upstream",
    true,
  );
  assert.match(description, /nothing to retry/i);
  assert.match(
    description,
    /permanently rejected upstream/,
    "the demotion reason still reaches the tooltip",
  );
  assert.notEqual(description, deliveryDescription(WIRE.quarantined, null));
});

/**
 * The flag is the sidecar's, not this module's guess. Every state has to accept
 * it, and the five whose labels already say where the event went must not
 * change — a second spelling of "via digest" is how two surfaces drift apart.
 */
test("the digest flag changes only the state whose label was ambiguous", () => {
  for (const state of EDGE_DELIVERY_STATES) {
    if (state === WIRE.quarantined) continue;
    assert.equal(
      deliveryLabel(state, true),
      deliveryLabel(state),
      `${state} already says what it is; the flag must not restate it`,
    );
    assert.equal(deliveryTone(state, true), deliveryTone(state));
  }

  // An unreadable state stays neutral rather than borrowing the carried copy.
  assert.equal(deliveryLabel("brandNewState", true), UNKNOWN_DELIVERY_LABEL);
  assert.equal(deliveryTone("brandNewState", true), UNKNOWN_DELIVERY_TONE);

  // And the predicate answers for a quarantined row too, which is the whole
  // fact that could not be read off the state before.
  assert.equal(isCarriedByDigest(WIRE.quarantined, true), true);
  assert.equal(isCarriedByDigest(WIRE.quarantined), false);
  assert.equal(isCarriedByDigest("brandNewState", true), false);
});

test("allDeliveryLabels covers the carried variants too", () => {
  const labels = allDeliveryLabels();
  for (const state of EDGE_DELIVERY_STATES) {
    assert.ok(labels.includes(deliveryLabel(state)), `${state} label listed`);
    assert.ok(
      labels.includes(deliveryLabel(state, true)),
      `${state} carried-by-digest label listed`,
    );
  }
  assert.equal(
    new Set(labels).size,
    labels.length,
    `every label this module produces is distinct, got ${JSON.stringify(labels)}`,
  );
});

// ── Unknown-state fallback ───────────────────────────────────────────────────

test("an unknown state string falls back safely instead of throwing", () => {
  for (const bogus of [
    "sent",
    "SYNCEDEXACT",
    "",
    "delivered",
    "pending ",
    null,
    undefined,
    42,
    {},
    [],
  ]) {
    assert.doesNotThrow(() => deliveryLabel(bogus));
    assert.equal(deliveryLabel(bogus), UNKNOWN_DELIVERY_LABEL);
    assert.equal(deliveryTone(bogus), UNKNOWN_DELIVERY_TONE);
    assert.equal(typeof deliveryDescription(bogus), "string");
    assert.equal(coerceDeliveryState(bogus), null);
  }
});

test("an unknown state is reported as neither local-only nor synced", () => {
  // Guessing in either direction would put a false claim on screen.
  assert.equal(isLocalOnly("who-knows"), false);
  assert.equal(isSyncedToCanonicalHistory("who-knows"), false);
  assert.equal(isCarriedByDigest("who-knows"), false);
});

// ── Tones ────────────────────────────────────────────────────────────────────

test("every tone comes from the repo's existing badge variant set", () => {
  for (const state of EDGE_DELIVERY_STATES) {
    assert.ok(
      BADGE_VARIANTS.has(deliveryTone(state)),
      `${state} tone '${deliveryTone(state)}' is not a Badge variant`,
    );
  }
  assert.ok(BADGE_VARIANTS.has(UNKNOWN_DELIVERY_TONE));
});

test("quarantined is the only destructive tone", () => {
  assert.equal(deliveryTone(WIRE.quarantined), "destructive");
  for (const state of EDGE_DELIVERY_STATES) {
    if (state === WIRE.quarantined) continue;
    assert.notEqual(deliveryTone(state), "destructive");
  }
});

// ── Axis predicates ──────────────────────────────────────────────────────────

test("local-only covers exactly the pre-sync states", () => {
  assert.equal(isLocalOnly(WIRE.pending), true);
  assert.equal(isLocalOnly(WIRE.pendingViaDigest), true);
  assert.equal(isLocalOnly(WIRE.claimed), true);
  assert.equal(isLocalOnly(WIRE.syncedExact), false);
  assert.equal(isLocalOnly(WIRE.syncedViaDigest), false);
  assert.equal(isLocalOnly(WIRE.quarantined), false);
});

test("digest sync counts as reaching canonical history", () => {
  assert.equal(isSyncedToCanonicalHistory(WIRE.syncedExact), true);
  assert.equal(isSyncedToCanonicalHistory(WIRE.syncedViaDigest), true);
  assert.equal(isSyncedToCanonicalHistory(WIRE.pending), false);
  assert.equal(isSyncedToCanonicalHistory(WIRE.quarantined), false);
});

test("coerceDeliveryState passes known states through untouched", () => {
  for (const state of EDGE_DELIVERY_STATES) {
    assert.equal(coerceDeliveryState(state), state);
  }
});

// ── Queued age ───────────────────────────────────────────────────────────────

test("formatQueuedAge renders compact durations", () => {
  const nowMs = 1_800_000_000_000;
  const nowSeconds = Math.floor(nowMs / 1000);
  assert.equal(formatQueuedAge(nowSeconds, nowMs), "under a minute");
  assert.equal(formatQueuedAge(nowSeconds - 90, nowMs), "1m");
  assert.equal(formatQueuedAge(nowSeconds - 7200, nowMs), "2h");
  assert.equal(formatQueuedAge(nowSeconds - 3 * 86400, nowMs), "3d");
});

test("formatQueuedAge clamps a future timestamp instead of going negative", () => {
  const nowMs = 1_800_000_000_000;
  assert.equal(
    formatQueuedAge(Math.floor(nowMs / 1000) + 600, nowMs),
    "under a minute",
  );
});

test("formatQueuedAge survives a garbage timestamp", () => {
  assert.equal(formatQueuedAge(Number.NaN, 1_800_000_000_000), "unknown");
  assert.equal(formatQueuedAge(1, Number.POSITIVE_INFINITY), "unknown");
});
