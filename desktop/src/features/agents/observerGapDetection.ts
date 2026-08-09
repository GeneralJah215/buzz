/**
 * Observer-frame discontinuity detection for the desktop.
 *
 * Observer frames are NOT reliable. They travel a 1000-slot broadcast inside
 * the harness (rate-limited to 90 frames/minute, so loss under load is
 * structural, not bad luck) and then a relay. Four desktop consumers used to
 * read them as if nothing could ever go missing: managed-agent lifecycle, the
 * transcript text accumulator, `control_result` RPC completion, and persisted
 * session config.
 *
 * `seq` is monotonic and process-local, so a hole is detectable here even when
 * the harness never noticed one. Two things arrive at this module:
 *
 * - **Harness-announced gaps** — an `observer_gap` frame the harness minted
 *   after its own bus dropped frames. It carries a receipt saying how much it
 *   recovered from its control replay ring and how much is gone for good.
 * - **Relay-level gaps** — a `seq` jump the desktop sees with no announcement,
 *   meaning frames were lost *after* the harness published them. There is no
 *   replay ring on this side of the relay, so these are never recoverable and
 *   are always reported as such.
 *
 * Nothing here guesses. An unparseable or absent receipt is treated as
 * incomplete, because the only reading of "I do not know" that is safe is the
 * pessimistic one.
 */

import type { ObserverEvent, TranscriptItem } from "./ui/agentSessionTypes";
import {
  createEmptyTranscriptState,
  processTranscriptEvent,
  type TranscriptState,
} from "./ui/agentSessionTranscript";

/** Frame kind the harness mints to announce its own dropped frames. */
export const OBSERVER_GAP_KIND = "observer_gap";

/**
 * `control_result` status published to unblock callers waiting on an RPC
 * completion that was destroyed in a gap. The ModelPicker's request would
 * otherwise hang forever with no retry: there is no second copy of that reply.
 */
export const CONTROL_RESULT_GAP_STATUS = "observer_gap_unreconcilable";

export type ObserverGap = {
  /** Last `seq` known good. The hole starts after it. */
  fromSeq: number;
  /** First `seq` after the hole, when known. */
  toSeq: number | null;
  /** Control-plane frames gone for good. */
  unrecoverable: number;
  /** Telemetry frames destroyed — the transcript has a hole this wide. */
  lostContent: number;
  /**
   * Whether every control-plane frame in the hole was recovered and
   * republished. **False means downstream state may be wrong**: an agent's
   * lifecycle badge, an RPC completion, or a persisted session config.
   */
  controlComplete: boolean;
  /** Where the loss happened, which decides whether recovery was possible. */
  source: "harness" | "relay";
};

function finiteNumber(value: unknown, fallback: number): number {
  return typeof value === "number" && Number.isFinite(value) ? value : fallback;
}

/**
 * Read a discontinuity receipt off an `observer_gap` frame.
 *
 * Handles both origins: a frame minted by the harness after its own bus
 * dropped frames, and one this module synthesized for a relay-level `seq`
 * jump. The frame declares its own origin, so a caller cannot mislabel one as
 * the other.
 *
 * `controlComplete` is only believed when it is literally `true`. A missing,
 * malformed, or non-boolean field reads as incomplete — a receipt we cannot
 * parse is not a receipt of success.
 */
export function parseObserverGapFrame(
  event: Pick<ObserverEvent, "kind" | "payload" | "seq">,
): ObserverGap | null {
  if (event.kind !== OBSERVER_GAP_KIND) return null;
  const payload = (event.payload ?? {}) as Record<string, unknown>;
  const toSeq = payload.toSeq;
  return {
    fromSeq: finiteNumber(payload.fromSeq, event.seq),
    toSeq: typeof toSeq === "number" && Number.isFinite(toSeq) ? toSeq : null,
    unrecoverable: finiteNumber(payload.unrecoverable, 0),
    lostContent: finiteNumber(payload.lostContent, 0),
    controlComplete: payload.controlComplete === true,
    source: payload.source === "relay" ? "relay" : "harness",
  };
}

/**
 * Detect frames lost between the harness and this process.
 *
 * `lastSeq` is the highest `seq` already ingested for this agent. A jump means
 * the relay, the subscription, or this process lost frames the harness believed
 * it had delivered — so no `observer_gap` frame is coming, and there is no
 * replay ring on this side to refill it. Always incomplete.
 *
 * Returns `null` for the ordinary cases: the first frame for an agent, an
 * in-order frame, and a re-delivered or out-of-order older frame (which the
 * store's `(seq, timestamp)` dedupe and sort already handle).
 */
export function detectRelaySeqGap(
  lastSeq: number | undefined,
  event: Pick<ObserverEvent, "seq" | "kind">,
): ObserverGap | null {
  if (lastSeq === undefined) return null;
  const missing = event.seq - lastSeq - 1;
  if (missing <= 0) return null;
  return {
    fromSeq: lastSeq,
    toSeq: event.seq,
    unrecoverable: missing,
    lostContent: 0,
    controlComplete: false,
    source: "relay",
  };
}

/**
 * Synthesize an observer event for a relay-level gap.
 *
 * Relay gaps become real `observer_gap` events in the per-agent journal so they
 * are indistinguishable from harness-announced ones downstream: they sort into
 * place, survive the store's full-rebuild path, and render the same marker. The
 * fractional `seq` places the marker inside the hole it describes without
 * colliding with any real frame — every harness-assigned `seq` is an integer.
 */
export function syntheticGapEvent(
  gap: ObserverGap,
  event: Pick<ObserverEvent, "timestamp" | "channelId" | "sessionId">,
): ObserverEvent {
  return {
    seq: gap.fromSeq + 0.5,
    timestamp: event.timestamp,
    kind: OBSERVER_GAP_KIND,
    agentIndex: null,
    channelId: event.channelId ?? null,
    sessionId: event.sessionId ?? null,
    turnId: null,
    payload: {
      fromSeq: gap.fromSeq,
      toSeq: gap.toSeq,
      unrecoverable: gap.unrecoverable,
      lostContent: gap.lostContent,
      controlComplete: gap.controlComplete,
      source: gap.source,
    },
  };
}

function gapText(gap: ObserverGap): string {
  const span =
    gap.toSeq === null
      ? `after frame ${gap.fromSeq}`
      : `between frames ${gap.fromSeq} and ${gap.toSeq}`;
  if (gap.controlComplete) {
    return `Some activity ${span} was dropped before it reached this device. Agent status was re-read from the harness and is current; ${gap.lostContent} telemetry frame(s) are missing from this transcript.`;
  }
  return `Frames ${span} were lost and could not be recovered. This transcript is incomplete, and agent status, model switches, and session settings shown for this agent may be out of date.`;
}

/**
 * The transcript marker for one discontinuity.
 *
 * Rendered through the existing `lifecycle` item shape so no new render class
 * or presenter is required. `renderClass` is `"error"` for an unrecoverable
 * hole and `"status"` for one the harness refilled — the two must not look the
 * same, because only one of them means the surrounding text is trustworthy.
 */
export function gapTranscriptItem(
  gap: ObserverGap,
  event: Pick<ObserverEvent, "seq" | "timestamp" | "channelId" | "sessionId">,
): TranscriptItem {
  return {
    id: `observer-gap:${event.seq}`,
    type: "lifecycle",
    renderClass: gap.controlComplete ? "status" : "error",
    title: gap.controlComplete
      ? "Some activity is missing"
      : "Activity was lost",
    text: gapText(gap),
    timestamp: event.timestamp,
    acpSource: OBSERVER_GAP_KIND,
    channelId: event.channelId ?? null,
    sessionId: event.sessionId ?? null,
  };
}

/**
 * Append a gap marker and break the streaming-text accumulator.
 *
 * Sealing the open message keys is the load-bearing half. `upsertMessage` does
 * `text: existing.text + text`, so without this a chunk arriving after a hole
 * is concatenated onto the chunk before it and the rendered message reads as
 * continuous prose with a span silently deleted from the middle. Sealing forces
 * the next chunk into a new item, so the marker sits visibly between them.
 */
export function appendGapMarker(
  state: TranscriptState,
  gap: ObserverGap,
  event: Pick<ObserverEvent, "seq" | "timestamp" | "channelId" | "sessionId">,
): TranscriptState {
  const item = gapTranscriptItem(gap, event);
  const itemsById = new Map(state.itemsById);
  itemsById.set(item.id, item);
  return {
    ...state,
    items: [...state.items, item],
    itemsById,
    sealedKeys: new Set([
      ...state.sealedKeys,
      ...state.activeMessageKey.values(),
    ]),
  };
}

/**
 * `processTranscriptEvent` plus gap markers.
 *
 * The transcript module has no dispatch arm for `observer_gap` and no terminal
 * `else`, so a gap frame passed to it alone produces nothing at all — the
 * silence this whole module exists to remove.
 */
export function processTranscriptEventWithGaps(
  state: TranscriptState,
  event: ObserverEvent,
): TranscriptState {
  const next = processTranscriptEvent(state, event);
  const gap = parseObserverGapFrame(event);
  return gap ? appendGapMarker(next, gap, event) : next;
}

/**
 * Full rebuild that keeps gap markers.
 *
 * The store rebuilds from scratch on out-of-order arrival and on trim. Deriving
 * markers from the journal rather than storing them means a rebuild cannot
 * quietly drop the evidence that frames are missing.
 */
export function buildTranscriptStateWithGaps(
  events: readonly ObserverEvent[],
): TranscriptState {
  let state = createEmptyTranscriptState();
  for (const event of events) {
    state = processTranscriptEventWithGaps(state, event);
  }
  return state;
}
