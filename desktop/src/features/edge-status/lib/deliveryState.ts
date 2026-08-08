/**
 * Pure presentation helpers for the two delivery axes. No React here.
 *
 * SPEC-2026-08-05 success criterion (3): "the two delivery states — delivered
 * locally vs synced to canonical history — labeled separately everywhere they
 * surface." Every event in the edge store is ALREADY delivered locally; the
 * label below communicates only the SECOND axis. Collapsing the two into one
 * word ("sent", "delivered") is the exact failure the criterion forbids, so
 * the labels here are deliberately distinct strings and a test asserts it.
 */

import type { VariantProps } from "class-variance-authority";

import {
  EDGE_DELIVERY_STATE_KEYS,
  EDGE_DELIVERY_STATE_WIRE_NAMES,
  edgeDeliveryStateKey,
  type EdgeDeliveryState,
  type EdgeDeliveryStateKey,
} from "@/features/edge-status/api/edgeStatus";
import type { badgeVariants } from "@/shared/ui/badge";

/**
 * The repo's existing badge tone vocabulary. Derived from `badgeVariants` so
 * this module can never invent a colour the design system does not have —
 * dropping a variant from `shared/ui/badge` breaks typecheck here.
 */
export type DeliveryTone = NonNullable<
  VariantProps<typeof badgeVariants>["variant"]
>;

/** Shown when a version-skewed sidecar reports a state this build predates. */
export const UNKNOWN_DELIVERY_LABEL = "Sync state unknown";
export const UNKNOWN_DELIVERY_TONE: DeliveryTone = "outline";

// Every map below is keyed on the INTERNAL state name, never on the wire
// spelling. The wire spelling lives in exactly one place —
// `EDGE_DELIVERY_STATE_WIRE_NAMES` in `api/edgeStatus.ts` — so a sidecar
// rename is a one-line edit there and nothing in this file moves.
const DELIVERY_LABELS: Record<EdgeDeliveryStateKey, string> = {
  // Queued in the local store and fanned out to every local client. This is
  // the "delivered locally" half of the pair — NOT "sent".
  pending: "Delivered locally",
  // Still local only, but off the author's path: this machine's edge identity
  // will carry it upstream inside a catch-up digest. Nobody is waiting on the
  // author, so it must not read like `pending`.
  pendingViaDigest: "Sync deferred",
  // An author drain has leased the event and is republishing it upstream.
  claimed: "Syncing",
  // The original event id landed in canonical history.
  syncedExact: "Synced to history",
  // Too old for the relay's ingest drift gate, so it reached canonical
  // history inside a catch-up digest under a different event id.
  syncedViaDigest: "Synced via digest",
  // The drain gave up. Still readable locally; absent from canonical history.
  quarantined: "Sync failed",
};

const DELIVERY_TONES: Record<EdgeDeliveryStateKey, DeliveryTone> = {
  pending: "outline",
  pendingViaDigest: "warning",
  claimed: "secondary",
  syncedExact: "success",
  syncedViaDigest: "info",
  quarantined: "destructive",
};

const DELIVERY_DESCRIPTIONS: Record<EdgeDeliveryStateKey, string> = {
  pending: "Everyone on this machine has it. Not in canonical history yet.",
  pendingViaDigest:
    "Its author can no longer replay it, so this machine will carry it to canonical history inside a catch-up digest.",
  claimed: "Its author is republishing it to canonical history now.",
  syncedExact: "In canonical history under its original event id.",
  syncedViaDigest:
    "Too old for the relay's ingest window, so it reached canonical history inside a catch-up digest.",
  quarantined:
    "Still readable locally, but every attempt to reach canonical history failed.",
};

/**
 * Turn any wire value into a known state, or `null` when it is not one.
 * Never throws — a sidecar newer than this build must degrade to a neutral
 * badge, not take the timeline down with it.
 */
export function coerceDeliveryState(value: unknown): EdgeDeliveryState | null {
  const key = edgeDeliveryStateKey(value);
  return key === null ? null : EDGE_DELIVERY_STATE_WIRE_NAMES[key];
}

/**
 * The canonical-history label for a delivery state. `pending` reads
 * "Delivered locally" precisely because local delivery already happened and
 * the upstream sync has not.
 */
export function deliveryLabel(state: unknown): string {
  const key = edgeDeliveryStateKey(state);
  return key === null ? UNKNOWN_DELIVERY_LABEL : DELIVERY_LABELS[key];
}

/** Badge tone for a delivery state, from the repo's existing variant set. */
export function deliveryTone(state: unknown): DeliveryTone {
  const key = edgeDeliveryStateKey(state);
  return key === null ? UNKNOWN_DELIVERY_TONE : DELIVERY_TONES[key];
}

/**
 * Longer hover copy explaining what the label means.
 *
 * `demotionReason` is appended when the sidecar sent one. A grey "Sync
 * deferred" with no reason is barely better than no badge at all: the label
 * says where the event went, and only the reason says whether that is routine
 * ("older than the relay drift window") or something the operator has to act
 * on ("permanently rejected upstream").
 */
export function deliveryDescription(
  state: unknown,
  demotionReason?: string | null,
): string {
  const key = edgeDeliveryStateKey(state);
  const base =
    key === null
      ? "This build does not recognise the state the edge sidecar reported."
      : DELIVERY_DESCRIPTIONS[key];
  const reason =
    typeof demotionReason === "string" ? demotionReason.trim() : "";
  return reason.length === 0 ? base : `${base} Reason: ${reason}`;
}

/**
 * True once the event exists in canonical history, by either route. Digest
 * sync counts: the content converged, even though the event id did not.
 */
export function isSyncedToCanonicalHistory(state: unknown): boolean {
  const key = edgeDeliveryStateKey(state);
  return key === "syncedExact" || key === "syncedViaDigest";
}

/**
 * True while the event lives only in the local edge store. An unknown state
 * is NOT reported as local-only — claiming "not synced" on a state we cannot
 * read would be a lie in the more alarming direction.
 */
export function isLocalOnly(state: unknown): boolean {
  const key = edgeDeliveryStateKey(state);
  return key === "pending" || key === "pendingViaDigest" || key === "claimed";
}

/**
 * True when the event is queued but NO author can move it: this machine's edge
 * identity carries it upstream in a digest instead.
 *
 * Kept separate from `isLocalOnly` because the two answer different questions.
 * Anything summing a "waiting for an author" figure has to exclude these rows
 * — there is no author to wait for.
 */
export function isCarriedByDigest(state: unknown): boolean {
  return edgeDeliveryStateKey(state) === "pendingViaDigest";
}

/** Every label this module can produce, for exhaustiveness assertions. */
export function allDeliveryLabels(): string[] {
  return EDGE_DELIVERY_STATE_KEYS.map((key) => DELIVERY_LABELS[key]);
}

const MINUTE_SECONDS = 60;
const HOUR_SECONDS = 3600;
const DAY_SECONDS = 86400;

/**
 * Compact age for a queued event, matching the app's existing `12m ago` shape
 * but without the "ago" suffix — callers read it as a duration ("waiting 12m").
 *
 * @param unixSeconds when the oldest queued event was authored
 * @param nowMs current wall clock in milliseconds
 */
export function formatQueuedAge(unixSeconds: number, nowMs: number): string {
  if (!Number.isFinite(unixSeconds) || !Number.isFinite(nowMs)) {
    return "unknown";
  }
  const delta = Math.max(0, Math.floor(nowMs / 1000) - unixSeconds);

  if (delta < MINUTE_SECONDS) {
    return "under a minute";
  }
  if (delta < HOUR_SECONDS) {
    return `${Math.floor(delta / MINUTE_SECONDS)}m`;
  }
  if (delta < DAY_SECONDS) {
    return `${Math.floor(delta / HOUR_SECONDS)}h`;
  }
  return `${Math.floor(delta / DAY_SECONDS)}d`;
}
