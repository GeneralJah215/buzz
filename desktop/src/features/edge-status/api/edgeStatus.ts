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
 * THE ONE PLACE the wire spelling of a delivery state is written down.
 *
 * Keys are this app's internal names and never move — every switch, label map,
 * tone map and predicate in the feature is keyed on them. Values are exactly
 * the strings the sidecar sends. When the sidecar renames a state, the whole
 * frontend change is editing the value on that one line; nothing else in this
 * feature spells a wire state out.
 *
 * SPEC-2026-08-05 success criterion (3) requires the two axes to stay
 * separate: every event in the edge store is already *delivered locally*, and
 * these values describe only the second axis — whether it reached canonical
 * history. See `lib/deliveryState.ts` for the labels.
 *
 * The `exact` and `digest` routes stay apart in the live states too, not only
 * the terminal ones: a `pendingViaDigest` row is waiting for THIS machine's
 * edge identity to carry it upstream in a catch-up digest, while a `pending`
 * row is waiting for its own author to push it. Collapsing those two leaves
 * the operator with a queue nothing on screen explains.
 */
export const EDGE_DELIVERY_STATE_WIRE_NAMES = {
  pending: "pending",
  pendingViaDigest: "pendingViaDigest",
  claimed: "claimed",
  syncedExact: "syncedExact",
  syncedViaDigest: "syncedViaDigest",
  quarantined: "quarantined",
} as const;

/** Internal, rename-proof name of a delivery state. */
export type EdgeDeliveryStateKey = keyof typeof EDGE_DELIVERY_STATE_WIRE_NAMES;

/** The value as it travels over IPC. */
export type EdgeDeliveryState =
  (typeof EDGE_DELIVERY_STATE_WIRE_NAMES)[EdgeDeliveryStateKey];

/** Internal names, ordered from "local only" to "terminal". */
export const EDGE_DELIVERY_STATE_KEYS = Object.keys(
  EDGE_DELIVERY_STATE_WIRE_NAMES,
) as EdgeDeliveryStateKey[];

/** Wire values, in the same order. */
export const EDGE_DELIVERY_STATES: readonly EdgeDeliveryState[] =
  EDGE_DELIVERY_STATE_KEYS.map((key) => EDGE_DELIVERY_STATE_WIRE_NAMES[key]);

const WIRE_NAME_TO_KEY: ReadonlyMap<string, EdgeDeliveryStateKey> = new Map(
  EDGE_DELIVERY_STATE_KEYS.map((key) => [
    EDGE_DELIVERY_STATE_WIRE_NAMES[key],
    key,
  ]),
);

/**
 * Wire value → internal name, or `null` when this build does not know the
 * state. The single translation point: every consumer switches on the key, so
 * a sidecar rename never reaches a comparison anywhere else.
 */
export function edgeDeliveryStateKey(
  value: unknown,
): EdgeDeliveryStateKey | null {
  if (typeof value !== "string") {
    return null;
  }
  return WIRE_NAME_TO_KEY.get(value) ?? null;
}

/**
 * THE ONE PLACE the wire spelling of a requeue outcome is written down, with
 * the same rules as `EDGE_DELIVERY_STATE_WIRE_NAMES` above.
 *
 * A bare `requeued: false` cannot separate "no such quarantined row of yours"
 * from "the edge is already carrying this row upstream, stop pressing Retry".
 * Those are the same boolean and completely different advice, which is why the
 * sidecar sends the outcome alongside it.
 */
export const EDGE_REQUEUE_OUTCOME_WIRE_NAMES = {
  requeued: "requeued",
  notFound: "notFound",
  carriedByDigest: "carriedByDigest",
} as const;

/** Internal, rename-proof name of a requeue outcome. */
export type EdgeRequeueOutcomeKey =
  keyof typeof EDGE_REQUEUE_OUTCOME_WIRE_NAMES;

const REQUEUE_OUTCOME_KEYS = Object.keys(
  EDGE_REQUEUE_OUTCOME_WIRE_NAMES,
) as EdgeRequeueOutcomeKey[];

const REQUEUE_WIRE_NAME_TO_KEY: ReadonlyMap<string, EdgeRequeueOutcomeKey> =
  new Map(
    REQUEUE_OUTCOME_KEYS.map((key) => [
      EDGE_REQUEUE_OUTCOME_WIRE_NAMES[key],
      key,
    ]),
  );

/**
 * Wire value → internal name, or `null` for an outcome this build predates.
 * A newer sidecar's fourth outcome must degrade to "the sidecar declined and
 * did not say why this build understands", never to a thrown retry.
 */
export function edgeRequeueOutcomeKey(
  value: unknown,
): EdgeRequeueOutcomeKey | null {
  if (typeof value !== "string") {
    return null;
  }
  return REQUEUE_WIRE_NAME_TO_KEY.get(value) ?? null;
}

export type EdgeDeliverySummary = {
  pending: number;
  /** Pending rows the edge identity carries; no author will ever claim them. */
  pendingViaDigest: number;
  claimed: number;
  syncedExact: number;
  syncedViaDigest: number;
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
  /**
   * True when the edge identity is already carrying this event upstream in a
   * catch-up digest. The sidecar refuses a retry on such a row, so the list
   * must not offer one.
   */
  carriedByDigest: boolean;
  /** Why the event left the exact path, or `null` when it never did. */
  demotionReason: string | null;
  /** Unix seconds of the last drain attempt. */
  updatedAt: number;
};

export type EdgeWaitingAuthor = {
  author: string;
  /** Rows this author's drain client can claim right now. */
  pending: number;
  /**
   * Rows the author *cannot* claim, because an ancestor of theirs never
   * reached canonical history. Waiting for the author does nothing here; the
   * ancestor has to be retried or discarded.
   */
  ancestorBlocked: number;
  /**
   * Rows the edge identity carries in a digest. No author is coming for them,
   * so they must never be added into a "waiting for an author" figure.
   */
  pendingViaDigest: number;
  /**
   * Unix seconds of the oldest queued event for this author, across ALL three
   * counts above. It is the author's overall wait and the sidecar's sort key —
   * never the age to print beside one count, because it can come from a bucket
   * that count knows nothing about.
   */
  oldestPendingAt: number;
  /**
   * Unix seconds of the oldest row the author can claim right now, or `null`
   * when `pending` is 0. This is the age that belongs beside `pending`.
   */
  oldestClaimableAt: number | null;
  /**
   * Unix seconds of the oldest ancestor-blocked row, or `null` when
   * `ancestorBlocked` is 0. This is the age that belongs beside
   * `ancestorBlocked` — the aggregate above could be a claimable or
   * digest-carried row's age, in the one section whose point is that these
   * events are not waiting for anybody.
   */
  oldestAncestorBlockedAt: number | null;
};

/** One event's delivery label plus the reason it left the exact path. */
export type EdgeDeliveryStateEntry = {
  state: EdgeDeliveryState;
  /**
   * True when this machine's edge identity is already carrying the event
   * upstream in a catch-up digest — the same fact the quarantine list reports
   * on `EdgeQuarantinedEvent.carriedByDigest`, for the same row.
   *
   * It is what lets a `quarantined` badge say what the quarantine list says:
   * the event's own replay was refused AND the digest has it, so there is
   * nothing to do. Without it the badge could only say "Sync failed", which
   * reads as stuck.
   */
  carriedByDigest: boolean;
  /** Why it was demoted, or `null` when it was not. */
  demotionReason: string | null;
};

/** `eventId` → delivery state, for the per-message badges in a timeline. */
export type EdgeDeliveryStateLookup = Record<string, EdgeDeliveryStateEntry>;

/** What the sidecar actually did with a manual quarantine retry. */
export type EdgeRequeueResult = {
  requeued: boolean;
  /** Raw wire value; read it through `edgeRequeueOutcomeKey`. */
  outcome: string;
};

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
 * The exact sentinel the Rust side returns when no sidecar is reachable —
 * `EDGE_UNAVAILABLE` in `desktop/src-tauri/src/relay/edge.rs`, returned on both
 * the no-binding and transport-failure paths, and on nothing else.
 *
 * Matched exactly, never by substring. Substring sniffing swept in real faults
 * that happen to contain an innocent word: `relay returned 503 Service
 * Unavailable`, `... community binding mismatch`, `sqlite: disk I/O error,
 * store unavailable`. Each of those made the status surface go silent with no
 * error at all, which is the worst possible answer to "is anything stuck?".
 */
export const EDGE_UNAVAILABLE_MESSAGE = "edge sidecar not running";

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
 * True only for the one rejection that means "the optional edge sidecar isn't
 * running here". Everything else — a 503, a binding mismatch, a SQLite fault —
 * is a real error and must reach the operator. A shape error is never treated
 * as unavailable either; that would silently swallow a contract break.
 */
export function isEdgeUnavailableError(error: unknown): boolean {
  if (error instanceof EdgeStatusShapeError) {
    return false;
  }
  return errorMessageOf(error).trim() === EDGE_UNAVAILABLE_MESSAGE;
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

function requireBoolean(
  source: Record<string, unknown>,
  field: string,
  context: string,
): boolean {
  const value = source[field];
  if (typeof value !== "boolean") {
    throw new EdgeStatusShapeError(
      `${context}: field '${field}' must be a boolean, got ${describe(value)}`,
    );
  }
  return value;
}

/**
 * A string field that may be `null` but may NOT be absent.
 *
 * The distinction is the whole point: `null` is the sidecar saying "this row
 * was never demoted", while a missing key is a sidecar too old to answer the
 * question at all. Treating the second as the first would render a blank where
 * the operator's only actionable detail belongs.
 */
function requireNullableString(
  source: Record<string, unknown>,
  field: string,
  context: string,
): string | null {
  if (!Object.hasOwn(source, field)) {
    throw new EdgeStatusShapeError(
      `${context}: field '${field}' is missing — 'null' means "not demoted", an absent key means the sidecar never said`,
    );
  }
  const value = source[field];
  if (value === null) {
    return null;
  }
  if (typeof value !== "string") {
    throw new EdgeStatusShapeError(
      `${context}: field '${field}' must be a string or null, got ${describe(value)}`,
    );
  }
  return value;
}

/** Numbers report their value; `got number` tells nobody why -1 was refused. */
function describeValue(value: unknown): string {
  return typeof value === "number" ? String(value) : describe(value);
}

/**
 * A count of things. Rust sends these as `u64`, so anything negative or
 * fractional is a broken producer, not a small count: `-1 pending` and
 * `2.5 quarantined` would both render, and "-1 stuck" is worse than an error.
 */
function requireCount(
  source: Record<string, unknown>,
  field: string,
  context: string,
): number {
  const value = source[field];
  if (typeof value !== "number" || !Number.isInteger(value) || value < 0) {
    throw new EdgeStatusShapeError(
      `${context}: field '${field}' must be a non-negative whole number, got ${describeValue(value)}`,
    );
  }
  return value;
}

/**
 * Unix seconds. Whole numbers, but unlike a count they may legitimately be
 * negative (a pre-1970 `created_at` is nonsense the relay would reject, yet it
 * is a *timestamp* problem, not a shape one — the ages just render clamped).
 */
function requireTimestamp(
  source: Record<string, unknown>,
  field: string,
  context: string,
): number {
  const value = source[field];
  if (typeof value !== "number" || !Number.isInteger(value)) {
    throw new EdgeStatusShapeError(
      `${context}: field '${field}' must be a whole number of unix seconds, got ${describeValue(value)}`,
    );
  }
  return value;
}

/**
 * A timestamp field that may be `null` but may NOT be absent, with the same
 * rule as `requireNullableString`: `null` is the sidecar saying "that bucket is
 * empty", while a missing key is a sidecar too old to answer. Reading the
 * second as the first would silently put an unrelated bucket's age back on
 * screen, which is the defect this field exists to end.
 */
function requireNullableTimestamp(
  source: Record<string, unknown>,
  field: string,
  context: string,
): number | null {
  if (!Object.hasOwn(source, field)) {
    throw new EdgeStatusShapeError(
      `${context}: field '${field}' is missing — 'null' means "nothing queued in that bucket", an absent key means the sidecar never said`,
    );
  }
  if (source[field] === null) {
    return null;
  }
  return requireTimestamp(source, field, context);
}

/** Narrowing guard over the wire value of a delivery state. */
export function isEdgeDeliveryState(
  value: unknown,
): value is EdgeDeliveryState {
  return edgeDeliveryStateKey(value) !== null;
}

function parseDeliverySummary(raw: unknown): EdgeDeliverySummary {
  const context = "edge_delivery_summary";
  const record = requireRecord(raw, context);
  return {
    pending: requireCount(record, "pending", context),
    pendingViaDigest: requireCount(record, "pendingViaDigest", context),
    claimed: requireCount(record, "claimed", context),
    syncedExact: requireCount(record, "syncedExact", context),
    syncedViaDigest: requireCount(record, "syncedViaDigest", context),
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
    createdAt: requireTimestamp(record, "createdAt", context),
    attempts: requireCount(record, "attempts", context),
    reason: requireString(record, "reason", context),
    // Required, never inferred. A missing flag defaulting to `false` would put
    // a live Retry button on a row the sidecar refuses to retry.
    carriedByDigest: requireBoolean(record, "carriedByDigest", context),
    demotionReason: requireNullableString(record, "demotionReason", context),
    updatedAt: requireTimestamp(record, "updatedAt", context),
  };
}

function parseWaitingAuthor(raw: unknown, index: number): EdgeWaitingAuthor {
  const context = `edge_waiting_authors[${index}]`;
  const record = requireRecord(raw, context);
  return {
    author: requireString(record, "author", context),
    pending: requireCount(record, "pending", context),
    ancestorBlocked: requireCount(record, "ancestorBlocked", context),
    pendingViaDigest: requireCount(record, "pendingViaDigest", context),
    oldestPendingAt: requireTimestamp(record, "oldestPendingAt", context),
    // Required, never inferred from the aggregate. Falling back to
    // `oldestPendingAt` is exactly the substitution that put a digest row's age
    // beside a count of claimable rows.
    oldestClaimableAt: requireNullableTimestamp(
      record,
      "oldestClaimableAt",
      context,
    ),
    oldestAncestorBlockedAt: requireNullableTimestamp(
      record,
      "oldestAncestorBlockedAt",
      context,
    ),
  };
}

/**
 * Turn the sidecar's `{ eventId, state, demotionReason }` rows into a lookup.
 *
 * Objects, not `[eventId, state]` pairs: a demoted row needs the reason it was
 * demoted next to the label, because "pendingViaDigest" says where the event
 * went and only `demotionReason` says why — and why is the actionable half.
 * "Older than the relay drift window" is routine; "permanently rejected
 * upstream" is not, and the badge must be able to tell the operator which.
 *
 * A *structurally* broken row (not an object, missing or mistyped members)
 * still throws: that is a contract break with no safe reading. An unrecognised
 * state STRING does not — the sidecar ships separately and gains states before
 * this build knows them, and one such row must cost one neutral badge, not the
 * entire batch. Omitted ids fall through to `UNKNOWN_DELIVERY_LABEL` in
 * `lib/deliveryState.ts`, which is exactly what that path is for.
 */
function parseDeliveryStateLookup(raw: unknown): EdgeDeliveryStateLookup {
  const context = "edge_event_delivery_states";
  const rows = requireArray(raw, context);
  const lookup: EdgeDeliveryStateLookup = {};

  rows.forEach((row, index) => {
    const rowContext = `${context}[${index}]`;
    const record = requireRecord(row, rowContext);
    const eventId = requireString(record, "eventId", rowContext);
    if (eventId.length === 0) {
      throw new EdgeStatusShapeError(
        `${rowContext}: field 'eventId' must be a non-empty string, got ""`,
      );
    }
    const state = requireString(record, "state", rowContext);
    // Required, never defaulted: a missing flag read as `false` would badge a
    // quarantined row the digest is already carrying as plain "Sync failed",
    // which is the disagreement with the quarantine list this field ends.
    const carriedByDigest = requireBoolean(
      record,
      "carriedByDigest",
      rowContext,
    );
    const demotionReason = requireNullableString(
      record,
      "demotionReason",
      rowContext,
    );
    if (!isEdgeDeliveryState(state)) {
      // A state this build predates. Leave the id out entirely: a reason with
      // no readable label attached explains nothing.
      return;
    }
    // Not `lookup[eventId] = entry`: an id of `__proto__` would hit the
    // prototype setter instead of creating a key, dropping the row silently
    // (and mutating the object's prototype on the way past).
    Object.defineProperty(lookup, eventId, {
      configurable: true,
      enumerable: true,
      value: {
        state,
        carriedByDigest,
        demotionReason,
      } satisfies EdgeDeliveryStateEntry,
      writable: true,
    });
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
 * Put a quarantined event back on the drain queue.
 *
 * A declined requeue is a user-visible outcome, not a silent no-op, and the
 * three declines are not interchangeable: `notFound` means the row is gone or
 * was never the caller's, while `carriedByDigest` means the edge is already
 * carrying it upstream and pressing Retry can never do anything. The caller
 * must say which.
 */
export async function requeueQuarantinedEvent(
  eventId: string,
): Promise<EdgeRequeueResult> {
  const context = "edge_requeue_quarantined";
  const raw = await invokeTauri<unknown>(context, { eventId });
  const record = requireRecord(raw, context);
  return {
    requeued: requireBoolean(record, "requeued", context),
    outcome: requireString(record, "outcome", context),
  };
}
