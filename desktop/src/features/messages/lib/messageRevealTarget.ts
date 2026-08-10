import type { TimelineMessage } from "@/features/messages/types";
import { isBroadcastReply } from "@/features/messages/lib/threading";

/**
 * Where a message can actually be shown to the reader.
 *
 * The main timeline and the thread panel render DISJOINT row sets:
 * `buildMainTimelineEntries` drops every message with a `parentId` (except
 * broadcast replies), so a thread reply has no row on the main timeline and
 * never will, no matter how long anything waits for one. Search does not
 * respect that split — the client pass matches text in every loaded message,
 * replies included — so every consumer that wants to REVEAL a match has to
 * answer the same question first: which surface owns this id?
 *
 * This module is that single answer. It is deliberately pure and synchronous:
 * "can this be reached" is decidable from the loaded message set alone, so no
 * caller ever needs to poll, retry on a timer, or discover the answer by
 * watching a scroll attempt fail.
 */

/**
 * True when the message is rendered as a MAIN TIMELINE row.
 *
 * This is the exact predicate `buildMainTimelineEntries` filters on. It lives
 * here, and is imported there, so the two can never drift: if the main
 * timeline's membership rule changes, reveal resolution changes with it.
 */
export function isMainTimelineMessage(message: TimelineMessage): boolean {
  return message.parentId == null || isBroadcastReply(message.tags ?? []);
}

export type MessageRevealUnreachableReason =
  /** The id is not in the loaded window at all. */
  | "not-loaded"
  /** A reply whose thread root (or an ancestor) is not loaded. */
  | "detached-ancestry";

export type MessageRevealTarget = {
  /** The message the reader asked to see. */
  messageId: string;
  kind: "main-timeline" | "thread-reply" | "unreachable";
  /**
   * The row the MAIN timeline should scroll to and highlight, or null when
   * nothing on the main timeline stands for this match. For a thread reply
   * this is the thread root — the timeline points at the conversation that
   * contains the match while the panel shows the match itself.
   */
  mainTimelineMessageId: string | null;
  /** Thread root the panel must open on. Null unless `kind` is thread-reply. */
  threadHeadId: string | null;
  /** Ancestors that must be expanded before the reply is visible in the panel. */
  expandedReplyIds: ReadonlySet<string>;
  /** Why an unreachable target cannot be shown. Null when it can. */
  unreachableReason: MessageRevealUnreachableReason | null;
};

const NO_EXPANDED_REPLIES: ReadonlySet<string> = new Set<string>();

function unreachable(
  messageId: string,
  reason: MessageRevealUnreachableReason,
): MessageRevealTarget {
  return {
    messageId,
    kind: "unreachable",
    mainTimelineMessageId: null,
    threadHeadId: null,
    expandedReplyIds: NO_EXPANDED_REPLIES,
    unreachableReason: reason,
  };
}

/**
 * Resolve a message id to the surface that can show it.
 *
 * Returns null only when there is nothing to resolve (`messageId` is null).
 * An id that cannot be reached resolves to an `unreachable` target with a
 * reason — an explicit, inspectable "no", never an implicit "try again".
 */
export function resolveMessageRevealTarget(
  messageId: string | null,
  messageById: ReadonlyMap<string, TimelineMessage>,
): MessageRevealTarget | null {
  if (!messageId) {
    return null;
  }

  const message = messageById.get(messageId);
  if (!message) {
    return unreachable(messageId, "not-loaded");
  }

  if (isMainTimelineMessage(message)) {
    return {
      messageId,
      kind: "main-timeline",
      mainTimelineMessageId: messageId,
      threadHeadId: null,
      expandedReplyIds: NO_EXPANDED_REPLIES,
      unreachableReason: null,
    };
  }

  const threadHeadId = message.rootId ?? message.parentId ?? null;
  if (!threadHeadId || !messageById.has(threadHeadId)) {
    // The panel opens on a root id. Without the root in the loaded window there
    // is no thread to open, so this is a real dead end rather than a slow one.
    return unreachable(messageId, "detached-ancestry");
  }

  // Walk parent links up to the root, collecting the intermediate replies that
  // must be expanded for the match to be visible. `maxHops` bounds a corrupt
  // (cyclic) parent chain; the loop cannot outlive the loaded set.
  const expandedReplyIds = new Set<string>();
  let ancestorId: string | null = message.parentId ?? null;
  let hops = 0;
  const maxHops = messageById.size + 1;

  while (ancestorId && ancestorId !== threadHeadId && hops < maxHops) {
    const ancestor: TimelineMessage | undefined = messageById.get(ancestorId);
    if (!ancestor) {
      return unreachable(messageId, "detached-ancestry");
    }

    expandedReplyIds.add(ancestor.id);
    ancestorId = ancestor.parentId ?? null;
    hops += 1;
  }

  if (ancestorId !== threadHeadId) {
    return unreachable(messageId, "detached-ancestry");
  }

  return {
    messageId,
    kind: "thread-reply",
    mainTimelineMessageId: threadHeadId,
    threadHeadId,
    expandedReplyIds,
    unreachableReason: null,
  };
}
