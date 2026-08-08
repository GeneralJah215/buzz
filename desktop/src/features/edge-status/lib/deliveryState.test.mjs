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

import { EDGE_DELIVERY_STATES } from "../api/edgeStatus.ts";
import {
  allDeliveryLabels,
  coerceDeliveryState,
  deliveryDescription,
  deliveryLabel,
  deliveryTone,
  formatQueuedAge,
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

test("all five contract states are covered (no state falls through to unknown)", () => {
  assert.deepEqual(
    [...EDGE_DELIVERY_STATES],
    ["pending", "claimed", "syncedExact", "syncedViaDigest", "quarantined"],
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
  const local = deliveryLabel("pending");
  assert.equal(local, "Delivered locally");

  for (const synced of ["syncedExact", "syncedViaDigest"]) {
    assert.notEqual(
      deliveryLabel(synced),
      local,
      `${synced} must not reuse the local-delivery label`,
    );
  }

  // The local label must not claim canonical history, and the synced labels
  // must not claim mere local delivery.
  assert.ok(!/synced|history/i.test(local));
  assert.ok(/synced/i.test(deliveryLabel("syncedExact")));
  assert.ok(/synced/i.test(deliveryLabel("syncedViaDigest")));
});

test("exact and digest sync stay distinguishable", () => {
  assert.notEqual(
    deliveryLabel("syncedExact"),
    deliveryLabel("syncedViaDigest"),
  );
  assert.ok(/digest/i.test(deliveryLabel("syncedViaDigest")));
});

test("quarantined reads as a failure, not as delivered", () => {
  const label = deliveryLabel("quarantined");
  assert.equal(label, "Sync failed");
  assert.notEqual(label, deliveryLabel("syncedExact"));
});

test("allDeliveryLabels matches the per-state labels", () => {
  assert.deepEqual(
    allDeliveryLabels(),
    EDGE_DELIVERY_STATES.map((state) => deliveryLabel(state)),
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
  assert.equal(deliveryTone("quarantined"), "destructive");
  for (const state of EDGE_DELIVERY_STATES) {
    if (state === "quarantined") continue;
    assert.notEqual(deliveryTone(state), "destructive");
  }
});

// ── Axis predicates ──────────────────────────────────────────────────────────

test("local-only covers exactly the pre-sync states", () => {
  assert.equal(isLocalOnly("pending"), true);
  assert.equal(isLocalOnly("claimed"), true);
  assert.equal(isLocalOnly("syncedExact"), false);
  assert.equal(isLocalOnly("syncedViaDigest"), false);
  assert.equal(isLocalOnly("quarantined"), false);
});

test("digest sync counts as reaching canonical history", () => {
  assert.equal(isSyncedToCanonicalHistory("syncedExact"), true);
  assert.equal(isSyncedToCanonicalHistory("syncedViaDigest"), true);
  assert.equal(isSyncedToCanonicalHistory("pending"), false);
  assert.equal(isSyncedToCanonicalHistory("quarantined"), false);
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
