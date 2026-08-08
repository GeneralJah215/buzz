/**
 * Typed IPC wrappers for the `buzz-edge` status commands
 * (`desktop/src-tauri/src/commands/edge_status.rs`).
 *
 * Two rules shape this file:
 *
 * 1. **Nothing is blindly cast.** The edge sidecar is a separately-installed
 *    binary upgraded by a scheduled task, so a version-skewed sidecar can
 *    answer with a payload this build has never seen. Every wrapper validates
 *    the shape it got back and throws an `EdgeStatusShapeError` naming the
 *    offending field, rather than handing a malformed object to the sidebar
 *    and crashing the render tree.
 *
 * 2. **"Sidecar not running" is a normal state, not an error.** The whole
 *    local-relay feature is optional and off by default, so every command
 *    rejects with a string error on a machine that never installed it. Callers
 *    use `isEdgeUnavailableError` to tell that expected quiet from a genuine
 *    fault and render nothing instead of an error banner.
 */

import { invokeTauri } from "@/shared/api/tauri";

/**
 * The delivery-state machine, ordered from "local only" to "terminal".
 *
 * SPEC-2026-08-05 success criterion (3) requires the two axes to stay
 * separate: every event in the edge store is already *delivered locally*, and
 * these values describe only the second axis — whether it reached canonical
 * history. See `lib/deliveryState.ts` for the labels.
 */
export const EDGE_DELIVERY_STATES = [
  "pending",
  "claimed",
  "syncedExact",
  "syncedViaDigest",
  "quarantined",
] as const;

export type EdgeDeliveryState = (typeof EDGE_DELIVERY_STATES)[number];

export type EdgeDeliverySummary = {
  pending: number;
  claimed: number;
  deliveredExact: number;
  deliveredViaDigest: number;
  quarantined: number;
};

export type EdgeQuarantinedEvent = {
  eventId: string;
  channelId: string;
  author: string;
  /** Unix seconds, as authored (nostr `created_at`). */
  createdAt: number;
  attempts: number;
  reason: string;
  /** Unix seconds of the last drain attempt. */
  updatedAt: number;
};

export type EdgeWaitingAuthor = {
  author: string;
  pending: number;
  /** Unix seconds of the oldest queued event for this author. */
  oldestPendingAt: number;
};

/** `eventId` → delivery state, for the per-message badges in a timeline. */
export type EdgeDeliveryStateLookup = Record<string, EdgeDeliveryState>;

/** How many quarantine rows the list asks for when the caller does not say. */
export const DEFAULT_QUARANTINE_LIMIT = 50;

/**
 * A response that does not match the IPC contract. Distinct from a transport
 * rejection so `isEdgeUnavailableError` can never mistake a real
 * version-skew bug for the benign "sidecar is off" case and hide it.
 */
export class EdgeStatusShapeError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "EdgeStatusShapeError";
  }
}

/**
 * Lowercased substrings that mean "the edge sidecar is simply not there".
 * Covers both rejection families the commands document: the sidecar process
 * being absent, and the community binding not holding.
 */
export const EDGE_UNAVAILABLE_MARKERS = [
  "sidecar not running",
  "sidecar is not running",
  "edge sidecar",
  "edge not running",
  "not running",
  "not available",
  "unavailable",
  "connection refused",
  "econnrefused",
  "community binding",
  "not bound",
] as const;

function errorMessageOf(error: unknown): string {
  if (error instanceof Error) {
    return error.message;
  }
  if (typeof error === "string") {
    return error;
  }
  return "";
}

/**
 * True when a rejection means "the optional edge sidecar isn't running here".
 * A shape error is never treated as unavailable — that would silently swallow
 * a genuine contract break.
 */
export function isEdgeUnavailableError(error: unknown): boolean {
  if (error instanceof EdgeStatusShapeError) {
    return false;
  }
  const message = errorMessageOf(error).toLowerCase();
  if (message.length === 0) {
    return false;
  }
  return EDGE_UNAVAILABLE_MARKERS.some((marker) => message.includes(marker));
}

// ── Shape validation ─────────────────────────────────────────────────────────

function describe(value: unknown): string {
  if (value === null) {
    return "null";
  }
  if (Array.isArray(value)) {
    return "array";
  }
  return typeof value;
}

function requireRecord(
  value: unknown,
  context: string,
): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new EdgeStatusShapeError(
      `${context}: expected an object, got ${describe(value)}`,
    );
  }
  return value as Record<string, unknown>;
}

function requireArray(value: unknown, context: string): unknown[] {
  if (!Array.isArray(value)) {
    throw new EdgeStatusShapeError(
      `${context}: expected an array, got ${describe(value)}`,
    );
  }
  return value;
}

function requireString(
  source: Record<string, unknown>,
  field: string,
  context: string,
): string {
  const value = source[field];
  if (typeof value !== "string") {
    throw new EdgeStatusShapeError(
      `${context}: field '${field}' must be a string, got ${describe(value)}`,
    );
  }
  return value;
}

function requireCount(
  source: Record<string, unknown>,
  field: string,
  context: string,
): number {
  const value = source[field];
  if (typeof value !== "number" || !Number.isFinite(value)) {
    throw new EdgeStatusShapeError(
      `${context}: field '${field}' must be a finite number, got ${describe(value)}`,
    );
  }
  return value;
}

/** Narrowing guard over the wire value of a delivery state. */
export function isEdgeDeliveryState(
  value: unknown,
): value is EdgeDeliveryState {
  return (
    typeof value === "string" &&
    (EDGE_DELIVERY_STATES as readonly string[]).includes(value)
  );
}

function parseDeliverySummary(raw: unknown): EdgeDeliverySummary {
  const context = "edge_delivery_summary";
  const record = requireRecord(raw, context);
  return {
    pending: requireCount(record, "pending", context),
    claimed: requireCount(record, "claimed", context),
    deliveredExact: requireCount(record, "deliveredExact", context),
    deliveredViaDigest: requireCount(record, "deliveredViaDigest", context),
    quarantined: requireCount(record, "quarantined", context),
  };
}

function parseQuarantinedEvent(
  raw: unknown,
  index: number,
): EdgeQuarantinedEvent {
  const context = `edge_quarantined_events[${index}]`;
  const record = requireRecord(raw, context);
  return {
    eventId: requireString(record, "eventId", context),
    channelId: requireString(record, "channelId", context),
    author: requireString(record, "author", context),
    createdAt: requireCount(record, "createdAt", context),
    attempts: requireCount(record, "attempts", context),
    reason: requireString(record, "reason", context),
    updatedAt: requireCount(record, "updatedAt", context),
  };
}

function parseWaitingAuthor(raw: unknown, index: number): EdgeWaitingAuthor {
  const context = `edge_waiting_authors[${index}]`;
  const record = requireRecord(raw, context);
  return {
    author: requireString(record, "author", context),
    pending: requireCount(record, "pending", context),
    oldestPendingAt: requireCount(record, "oldestPendingAt", context),
  };
}

function parseDeliveryStateLookup(raw: unknown): EdgeDeliveryStateLookup {
  const context = "edge_event_delivery_states";
  const rows = requireArray(raw, context);
  const lookup: EdgeDeliveryStateLookup = {};

  rows.forEach((row, index) => {
    const entry = requireArray(row, `${context}[${index}]`);
    if (entry.length !== 2) {
      throw new EdgeStatusShapeError(
        `${context}[${index}]: expected a [eventId, state] pair, got ${entry.length} element(s)`,
      );
    }
    const [eventId, state] = entry;
    if (typeof eventId !== "string" || eventId.length === 0) {
      throw new EdgeStatusShapeError(
        `${context}[${index}]: event id must be a non-empty string, got ${describe(eventId)}`,
      );
    }
    if (!isEdgeDeliveryState(state)) {
      throw new EdgeStatusShapeError(
        `${context}[${index}]: unknown delivery state ${JSON.stringify(state)} for event '${eventId}'`,
      );
    }
    lookup[eventId] = state;
  });

  return lookup;
}

// ── Commands ─────────────────────────────────────────────────────────────────

/** Aggregate outbox counters for the status strip. */
export async function fetchEdgeDeliverySummary(): Promise<EdgeDeliverySummary> {
  return parseDeliverySummary(
    await invokeTauri<unknown>("edge_delivery_summary"),
  );
}

/** Events the drain gave up on, newest first (ordering is the sidecar's). */
export async function fetchEdgeQuarantinedEvents(
  limit: number = DEFAULT_QUARANTINE_LIMIT,
): Promise<EdgeQuarantinedEvent[]> {
  const raw = await invokeTauri<unknown>("edge_quarantined_events", { limit });
  return requireArray(raw, "edge_quarantined_events").map(
    parseQuarantinedEvent,
  );
}

/** Identities holding queued events with nobody online to push them. */
export async function fetchEdgeWaitingAuthors(): Promise<EdgeWaitingAuthor[]> {
  const raw = await invokeTauri<unknown>("edge_waiting_authors");
  return requireArray(raw, "edge_waiting_authors").map(parseWaitingAuthor);
}

/** Per-event delivery states for the message badges in a rendered window. */
export async function fetchEdgeEventDeliveryStates(
  eventIds: readonly string[],
): Promise<EdgeDeliveryStateLookup> {
  if (eventIds.length === 0) {
    return {};
  }
  const raw = await invokeTauri<unknown>("edge_event_delivery_states", {
    eventIds: [...eventIds],
  });
  return parseDeliveryStateLookup(raw);
}

/**
 * Put a quarantined event back on the drain queue. Resolves `false` when the
 * sidecar declined (already drained, or no longer queued) — that is a
 * user-visible outcome, not a silent no-op.
 */
export async function requeueQuarantinedEvent(
  eventId: string,
): Promise<boolean> {
  const raw = await invokeTauri<unknown>("edge_requeue_quarantined", {
    eventId,
  });
  if (typeof raw !== "boolean") {
    throw new EdgeStatusShapeError(
      `edge_requeue_quarantined: expected a boolean, got ${describe(raw)}`,
    );
  }
  return raw;
}
