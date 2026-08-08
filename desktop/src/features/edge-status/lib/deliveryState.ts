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
  EDGE_DELIVERY_STATES,
  isEdgeDeliveryState,
  type EdgeDeliveryState,
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

const DELIVERY_LABELS: Record<EdgeDeliveryState, string> = {
  // Queued in the local store and fanned out to every local client. This is
  // the "delivered locally" half of the pair — NOT "sent".
  pending: "Delivered locally",
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

const DELIVERY_TONES: Record<EdgeDeliveryState, DeliveryTone> = {
  pending: "outline",
  claimed: "secondary",
  syncedExact: "success",
  syncedViaDigest: "info",
  quarantined: "destructive",
};

const DELIVERY_DESCRIPTIONS: Record<EdgeDeliveryState, string> = {
  pending: "Everyone on this machine has it. Not in canonical history yet.",
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
  return isEdgeDeliveryState(value) ? value : null;
}

/**
 * The canonical-history label for a delivery state. `pending` reads
 * "Delivered locally" precisely because local delivery already happened and
 * the upstream sync has not.
 */
export function deliveryLabel(state: unknown): string {
  const known = coerceDeliveryState(state);
  return known === null ? UNKNOWN_DELIVERY_LABEL : DELIVERY_LABELS[known];
}

/** Badge tone for a delivery state, from the repo's existing variant set. */
export function deliveryTone(state: unknown): DeliveryTone {
  const known = coerceDeliveryState(state);
  return known === null ? UNKNOWN_DELIVERY_TONE : DELIVERY_TONES[known];
}

/** Longer hover copy explaining what the label means. */
export function deliveryDescription(state: unknown): string {
  const known = coerceDeliveryState(state);
  return known === null
    ? "This build does not recognise the state the edge sidecar reported."
    : DELIVERY_DESCRIPTIONS[known];
}

/**
 * True once the event exists in canonical history, by either route. Digest
 * sync counts: the content converged, even though the event id did not.
 */
export function isSyncedToCanonicalHistory(state: unknown): boolean {
  const known = coerceDeliveryState(state);
  return known === "syncedExact" || known === "syncedViaDigest";
}

/**
 * True while the event lives only in the local edge store. An unknown state
 * is NOT reported as local-only — claiming "not synced" on a state we cannot
 * read would be a lie in the more alarming direction.
 */
export function isLocalOnly(state: unknown): boolean {
  const known = coerceDeliveryState(state);
  return known === "pending" || known === "claimed";
}

/** Every label this module can produce, for exhaustiveness assertions. */
export function allDeliveryLabels(): string[] {
  return EDGE_DELIVERY_STATES.map((state) => DELIVERY_LABELS[state]);
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
