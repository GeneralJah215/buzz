import { isMainTimelineMessage } from "@/features/messages/lib/messageRevealTarget";
import type { TimelineMessage } from "@/features/messages/types";

/**
 * What the main timeline's pending search-target retry should do next.
 *
 * The retry exists for one legitimate reason: a row that IS part of the main
 * timeline's data may not be in the DOM yet (deferred snapshot, settle-gated
 * prepend, virtualizer window). That converges on render, not on time.
 *
 * It must NOT exist for a target the main timeline will never render — a
 * thread reply, or an id that left the loaded window. `pendingSearchTargetRef`
 * used to hold those forever, so the find bar counted matches that Enter could
 * never land on (BUG-058). "Renderable" is decidable from the loaded messages,
 * so an unrenderable target is abandoned on the spot rather than waited on.
 */
/**
 * Whether the main timeline can ever paint a row for this id.
 *
 * Read the LIVE message list, not the deferred/buffered snapshot: a row that
 * is merely late still counts as renderable, and the retry is allowed to wait
 * for it. A message that IS in the window but is a thread reply has no
 * main-timeline row and never will (`buildMainTimelineEntries` filters it
 * out), and an id absent from the window has none either. Both answers are
 * available now — neither needs a wait to discover.
 */
export function isRenderableInMainTimeline(
  messageId: string,
  messages: readonly TimelineMessage[],
): boolean {
  return messages.some(
    (message) => message.id === messageId && isMainTimelineMessage(message),
  );
}

export type PendingSearchRetryDecision =
  /** The target has no main-timeline row and never will. Stop. */
  | "abandon"
  /** Virtualized and windowed out: ask the virtualizer to realize its index. */
  | "realize-index"
  /** The row exists (or can exist) in the DOM: scroll and highlight it. */
  | "scroll";

export function decidePendingSearchRetry({
  isRenderable,
  isRowInDom,
  isVirtualized,
}: {
  /** The target is in the loaded set AND is a main-timeline row. */
  isRenderable: boolean;
  /** A row element with the target's id is currently mounted. */
  isRowInDom: boolean;
  isVirtualized: boolean;
}): PendingSearchRetryDecision {
  if (!isRenderable) {
    return "abandon";
  }

  if (isVirtualized && !isRowInDom) {
    return "realize-index";
  }

  return "scroll";
}

/** Operator-readable explanation for a give-up, logged when one happens. */
export function describeAbandonedSearchTarget(messageId: string): string {
  return `[GUARDRAIL] search jump abandoned for ${messageId}: no main-timeline row exists for it in the loaded window (a thread reply is revealed in its thread panel instead)`;
}
