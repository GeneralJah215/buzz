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

/**
 * What changes when the sidecar also says the digest is carrying this row.
 *
 * One entry per state, holding label + tone + description together, so the
 * three cannot drift apart — a warning-toned badge still reading "Sync failed"
 * would be worse than either alone.
 *
 * Only `quarantined` is here, and that is the whole point of the flag. For the
 * other five states it adds nothing a reader does not already have:
 * `pendingViaDigest` and `syncedViaDigest` say "digest" in their own labels,
 * and `pending` / `claimed` / `syncedExact` are by definition not on the digest
 * path. A quarantined row is the one case where the label alone is ambiguous
 * and, until BUG-023, wrong: the quarantine list could say "the edge is already
 * carrying this upstream, the refused retry was correct, nothing to do" while
 * the badge for the same event said "Sync failed" — which reads as stuck.
 *
 * The tone moves off `destructive` deliberately. Red is the app's "act now"
 * colour and there is nothing to act on here; amber matches how the quarantine
 * list already draws these rows.
 */
const CARRIED_BY_DIGEST_OVERRIDES: Partial<
  Record<
    EdgeDeliveryStateKey,
    { label: string; tone: DeliveryTone; description: string }
  >
> = {
  quarantined: {
    label: "Sync failed, carried by digest",
    tone: "warning",
    description:
      "Its own replay was refused upstream, so this machine is carrying it to canonical history inside a catch-up digest instead. There is nothing to retry.",
  },
};

/** The override in force for a state, or `null` when nothing changes. */
function carriedOverride(state: unknown, carriedByDigest: boolean) {
  if (!carriedByDigest) {
    return null;
  }
  const key = edgeDeliveryStateKey(state);
  return key === null ? null : (CARRIED_BY_DIGEST_OVERRIDES[key] ?? null);
}

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
 *
 * @param carriedByDigest the sidecar's `carriedByDigest` for this same row.
 *   Pass it whenever you have it: for a quarantined row it is the difference
 *   between "this is stuck" and "this is handled".
 */
export function deliveryLabel(state: unknown, carriedByDigest = false): string {
  const override = carriedOverride(state, carriedByDigest);
  if (override !== null) {
    return override.label;
  }
  const key = edgeDeliveryStateKey(state);
  return key === null ? UNKNOWN_DELIVERY_LABEL : DELIVERY_LABELS[key];
}

/** Badge tone for a delivery state, from the repo's existing variant set. */
export function deliveryTone(
  state: unknown,
  carriedByDigest = false,
): DeliveryTone {
  const override = carriedOverride(state, carriedByDigest);
  if (override !== null) {
    return override.tone;
  }
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
  carriedByDigest = false,
): string {
  const override = carriedOverride(state, carriedByDigest);
  const key = edgeDeliveryStateKey(state);
  let base: string;
  if (override !== null) {
    base = override.description;
  } else if (key === null) {
    base = "This build does not recognise the state the edge sidecar reported.";
  } else {
    base = DELIVERY_DESCRIPTIONS[key];
  }
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
 * True when NO author can move this event: this machine's edge identity carries
 * it upstream in a digest instead.
 *
 * Kept separate from `isLocalOnly` because the two answer different questions.
 * Anything summing a "waiting for an author" figure has to exclude these rows
 * — there is no author to wait for.
 *
 * `pendingViaDigest` answers it from the state alone. Every other state needs
 * the sidecar's flag, which is why it is a parameter: a quarantined row on the
 * digest path is also carried, and reading that off the state was exactly the
 * thing that could not be done (BUG-023). An unrecognised state still answers
 * `false` — this build cannot say what such a row is doing.
 */
export function isCarriedByDigest(
  state: unknown,
  carriedByDigest = false,
): boolean {
  const key = edgeDeliveryStateKey(state);
  if (key === null) {
    return false;
  }
  return key === "pendingViaDigest" || carriedByDigest;
}

/**
 * Every label this module can produce, for exhaustiveness assertions: one per
 * state, then the digest-carried variants that read differently.
 */
export function allDeliveryLabels(): string[] {
  return [
    ...EDGE_DELIVERY_STATE_KEYS.map((key) => DELIVERY_LABELS[key]),
    ...EDGE_DELIVERY_STATE_KEYS.flatMap(
      (key) => CARRIED_BY_DIGEST_OVERRIDES[key]?.label ?? [],
    ),
  ];
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
