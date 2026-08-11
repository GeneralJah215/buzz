import type { RelayEvent } from "@/shared/api/types";
import {
  dedupeMessagesById,
  isNormalizedTimeline,
  markNormalizedTimeline,
  normalizeTimelineMessages,
  sortMessages,
} from "./messageQueryKeys";
import { getChannelIdFromTags, getThreadReference } from "./threading";

function getLocalRenderKey(message: RelayEvent) {
  return message.localKey ?? message.id;
}

function isMatchingPendingMessage(pending: RelayEvent, incoming: RelayEvent) {
  if (
    !pending.pending ||
    incoming.pending ||
    pending.content !== incoming.content ||
    pending.kind !== incoming.kind ||
    pending.pubkey.toLowerCase() !== incoming.pubkey.toLowerCase() ||
    getChannelIdFromTags(pending.tags) !== getChannelIdFromTags(incoming.tags)
  ) {
    return false;
  }

  const pendingThread = getThreadReference(pending.tags);
  const incomingThread = getThreadReference(incoming.tags);

  return (
    pendingThread.parentId === incomingThread.parentId &&
    pendingThread.rootId === incomingThread.rootId
  );
}

export function reconcileIncomingMessage(
  current: RelayEvent[],
  incoming: RelayEvent,
): RelayEvent[] {
  const normalizedCurrent = dedupeMessagesById(current);
  const replacedPending = normalizedCurrent.find((message) =>
    isMatchingPendingMessage(message, incoming),
  );
  const incomingWithLocalKey = replacedPending
    ? {
        ...incoming,
        localKey: replacedPending.localKey ?? replacedPending.id,
      }
    : incoming;
  const incomingLocalKey = getLocalRenderKey(incomingWithLocalKey);
  const deduped = normalizedCurrent.filter(
    (message) =>
      message.id !== incoming.id &&
      getLocalRenderKey(message) !== incomingLocalKey,
  );

  return [...deduped, incomingWithLocalKey];
}

function mergeMessagesWithNormalizer(
  current: RelayEvent[],
  incoming: RelayEvent,
  normalize: (messages: RelayEvent[]) => RelayEvent[],
): RelayEvent[] {
  return normalize(reconcileIncomingMessage(current, incoming));
}

export function mergeMessages(
  current: RelayEvent[],
  incoming: RelayEvent,
): RelayEvent[] {
  return mergeMessagesWithNormalizer(current, incoming, sortMessages);
}

/**
 * Retained rows in one channel's flattened timeline cache.
 *
 * `useLiveChannelUpdates` merges every inbound message into the cache of EVERY
 * subscribed channel, not just the one on screen, and nothing ever trimmed the
 * result — so a session with many busy channels grew one uncapped array per
 * channel plus the garbage of rebuilding it per message.
 *
 * What a trim can cost the reader, and why it does not:
 *
 * - **A background channel.** Its cache is a staging area. Opening the channel
 *   runs `useChannelMessagesQuery`, which refetches the newest window from the
 *   relay and calls `reconcileChannelWindowMessages` — that REPLACES the cache
 *   from the authoritative window store. Trimmed rows are re-supplied there.
 * - **The channel on screen.** Its cache is a projection of the window store
 *   (`projectChannelWindowMessages`), which runs on every live event that moves
 *   the window and on every older-page load. The window store is the source of
 *   truth and is not trimmed here, so a trimmed row is restored by the next
 *   projection.
 * - **A local pending send.** Never dropped, at any length — see `trimTimelineCache`.
 *
 * 60 pages of scrollback (`CHANNEL_WINDOW_PAGE_SIZE` is 50), so reaching the cap
 * by reading requires deliberately paging back past 3,000 messages; reaching it
 * by accumulation takes 3,000 live messages in one channel in one session.
 */
export const MAX_TIMELINE_CACHE_MESSAGES = 3000;

/**
 * Drop the oldest rows over the cap, keeping every `pending` row regardless of
 * age. A pending row is a local send with no relay acknowledgement yet: no
 * refetch and no window projection can bring it back, so it is the one row a
 * cap must never be allowed to eat.
 */
function trimTimelineCache(messages: RelayEvent[]): RelayEvent[] {
  const overflow = messages.length - MAX_TIMELINE_CACHE_MESSAGES;
  if (overflow <= 0) return messages;
  const rescued = messages.slice(0, overflow).filter((entry) => entry.pending);
  const kept = messages.slice(overflow);
  return markNormalizedTimeline(
    rescued.length > 0 ? rescued.concat(kept) : kept,
  );
}

/**
 * Append `incoming` to an already-normalized cache when it is unambiguously the
 * newest row and collides with nothing, which is the shape of essentially every
 * live message.
 *
 * The general path costs six transient arrays the length of the cache plus an
 * O(M log M) sort whose comparator runs on every pair, per message, per
 * channel. This costs one array and one linear scan of cheap field compares.
 * Returns `null` whenever anything is not provably safe — an unsorted or
 * unknown input array, an equal-or-older timestamp (the same-second id tiebreak
 * is decided by a sort, not by arrival), a duplicate id or render key, or a
 * pending row this message acknowledges — and the caller falls back.
 */
function appendNewestTimelineMessage(
  current: RelayEvent[],
  incoming: RelayEvent,
): RelayEvent[] | null {
  if (current.length === 0 || !isNormalizedTimeline(current)) return null;
  if (incoming.created_at <= current[current.length - 1].created_at)
    return null;

  const incomingLocalKey = getLocalRenderKey(incoming);
  for (const message of current) {
    if (message.id === incoming.id) return null;
    if (getLocalRenderKey(message) === incomingLocalKey) return null;
    // Only a pending row can be superseded, and only by a non-pending arrival;
    // isMatchingPendingMessage encodes both, and its tag parsing is reached
    // only for the handful of rows that are actually pending.
    if (message.pending && isMatchingPendingMessage(message, incoming)) {
      return null;
    }
  }

  return trimTimelineCache(markNormalizedTimeline(current.concat(incoming)));
}

export function mergeTimelineCacheMessages(
  current: RelayEvent[],
  incoming: RelayEvent,
): RelayEvent[] {
  const appended = appendNewestTimelineMessage(current, incoming);
  if (appended) return appended;
  return trimTimelineCache(
    mergeMessagesWithNormalizer(current, incoming, normalizeTimelineMessages),
  );
}
