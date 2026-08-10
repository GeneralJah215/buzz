import * as React from "react";

import { useSearchMessagesQuery } from "@/features/search/hooks";
import {
  resolveMessageRevealTarget,
  type MessageRevealTarget,
} from "@/features/messages/lib/messageRevealTarget";
import type { TimelineMessage } from "@/features/messages/types";
import type { SearchHit } from "@/shared/api/types";
import { hasPrimaryShortcutModifier } from "@/shared/lib/platform";

const MIN_QUERY_LENGTH = 2;
const DEBOUNCE_MS = 300;

/** Stable identity so the merge memo does not thrash while the relay is behind. */
const NO_RELAY_HITS: SearchHit[] = [];

type UseChannelFindOptions = {
  channelId: string | null;
  /**
   * False when the layout has no place to paint the find bar (the single-panel
   * viewport hides the whole main column). The shortcut must then stay out of
   * the way entirely — see the keydown handler.
   */
  canRenderFindBar?: boolean;
  messages: TimelineMessage[];
  onSearchHit?: (hit: SearchHit) => void;
};

export function useChannelFind({
  canRenderFindBar = true,
  channelId,
  messages,
  onSearchHit,
}: UseChannelFindOptions) {
  const [isOpen, setIsOpen] = React.useState(false);
  const [query, setQuery] = React.useState("");
  const [debouncedQuery, setDebouncedQuery] = React.useState("");
  const [activeIndex, setActiveIndex] = React.useState(0);
  // Bumped on every find shortcut press. The bar re-focuses and selects its
  // input whenever this changes, so a second ⌘F/Ctrl+F while the bar is
  // already open behaves like every other find bar instead of doing nothing.
  const [focusRequestId, setFocusRequestId] = React.useState(0);

  const reset = React.useCallback(() => {
    setIsOpen(false);
    setQuery("");
    setDebouncedQuery("");
    setActiveIndex(0);
  }, []);

  // Debounce the query for relay search.
  React.useEffect(() => {
    const trimmed = query.trim();
    if (trimmed.length < MIN_QUERY_LENGTH) {
      setDebouncedQuery("");
      return;
    }

    const timeout = window.setTimeout(() => {
      setDebouncedQuery(trimmed);
    }, DEBOUNCE_MS);

    return () => window.clearTimeout(timeout);
  }, [query]);

  // Typing a new term is a new search: start over at its first match rather
  // than keeping the cursor from the previous term's result list.
  const setQueryAndRestartNavigation = React.useCallback((next: string) => {
    setQuery(next);
    setActiveIndex(0);
  }, []);

  // Client-side search: instant matches against loaded messages.
  const clientMatchIds = React.useMemo<string[]>(() => {
    const trimmed = query.trim().toLowerCase();
    if (trimmed.length < MIN_QUERY_LENGTH) {
      return [];
    }

    const found: string[] = [];
    for (const message of messages) {
      if (message.body.toLowerCase().includes(trimmed)) {
        found.push(message.id);
      }
    }

    return found;
  }, [messages, query]);

  // Relay-backed search: full history via Postgres FTS.
  const relaySearch = useSearchMessagesQuery(debouncedQuery, {
    channelId: channelId ?? undefined,
    enabled: isOpen && debouncedQuery.length >= MIN_QUERY_LENGTH,
    limit: 100,
  });

  // The client pass reads `query` (every keystroke); the relay pass reads
  // `debouncedQuery`, which trails it by DEBOUNCE_MS. Merging the two
  // generations makes the find bar count, highlight and navigate to messages
  // that do not contain what is in the input — a fast typist gets results for
  // a prefix they have already typed past. Only merge relay hits once the
  // relay is answering the query the reader can actually see.
  const relayHits =
    debouncedQuery.length >= MIN_QUERY_LENGTH && debouncedQuery === query.trim()
      ? (relaySearch.data?.hits ?? NO_RELAY_HITS)
      : NO_RELAY_HITS;

  // Merge: start with client-side matches, then supplement with relay hits.
  // Relay hits may refer to older messages outside the initial cold window;
  // keep them in the match list and ask the route-target splice path to load
  // the active hit so the DOM-based timeline scroll can land it.
  const matchedIds = React.useMemo<string[]>(() => {
    const merged = [...clientMatchIds];
    const seen = new Set(merged);

    for (const hit of relayHits) {
      if (!seen.has(hit.eventId)) {
        merged.push(hit.eventId);
        seen.add(hit.eventId);
      }
    }

    return merged;
  }, [clientMatchIds, relayHits]);

  // Clamp active index when results change.
  React.useEffect(() => {
    setActiveIndex((current) => {
      if (matchedIds.length === 0) return 0;
      return current >= matchedIds.length ? 0 : current;
    });
  }, [matchedIds.length]);

  const activeMatch =
    matchedIds.length > 0 ? { messageId: matchedIds[activeIndex] } : null;

  // A match may live in a thread reply, which the main timeline never renders
  // (BUG-058). Resolve every active match to the surface that can actually
  // show it, once, here — the timeline and the thread panel both read this
  // answer instead of each guessing from the raw id.
  const messageById = React.useMemo(
    () => new Map(messages.map((message) => [message.id, message])),
    [messages],
  );
  const activeReveal = React.useMemo<MessageRevealTarget | null>(
    () =>
      resolveMessageRevealTarget(activeMatch?.messageId ?? null, messageById),
    [activeMatch?.messageId, messageById],
  );

  const relayHitById = React.useMemo(() => {
    const hits = new Map<string, SearchHit>();
    for (const hit of relayHits) {
      hits.set(hit.eventId, hit);
    }
    return hits;
  }, [relayHits]);

  React.useEffect(() => {
    if (!activeMatch) return;
    const hit = relayHitById.get(activeMatch.messageId);
    if (hit) onSearchHit?.(hit);
  }, [activeMatch, onSearchHit, relayHitById]);

  const matchingMessageIds = React.useMemo(() => {
    return new Set(matchedIds);
  }, [matchedIds]);

  const close = React.useCallback(() => {
    reset();
  }, [reset]);

  const goToNext = React.useCallback(() => {
    if (matchedIds.length === 0) return;
    setActiveIndex((current) => (current + 1) % matchedIds.length);
  }, [matchedIds.length]);

  const goToPrevious = React.useCallback(() => {
    if (matchedIds.length === 0) return;
    setActiveIndex((current) =>
      current === 0 ? matchedIds.length - 1 : current - 1,
    );
  }, [matchedIds.length]);

  // Register platform-standard find shortcut (⌘F on macOS, Ctrl+F elsewhere).
  // Read through a ref so the listener is registered once and still sees the
  // current layout.
  const canRenderFindBarRef = React.useRef(canRenderFindBar);
  canRenderFindBarRef.current = canRenderFindBar;
  React.useEffect(() => {
    function handleKeyDown(event: KeyboardEvent) {
      if (
        !hasPrimaryShortcutModifier(event) ||
        event.altKey ||
        event.shiftKey ||
        event.key.toLowerCase() !== "f"
      ) {
        return;
      }

      // BUG-059: in the single-panel layout the main column — and with it the
      // find bar — is not mounted. Claiming the key there suppressed BOTH the
      // (invisible) find bar and the webview's own find, so the press did
      // nothing at all. Leave the event alone when we have nowhere to render.
      if (!canRenderFindBarRef.current) {
        return;
      }

      event.preventDefault();
      setIsOpen(true);
      setFocusRequestId((current) => current + 1);
    }

    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, []);

  // Close find bar when switching channels.
  const prevChannelIdRef = React.useRef(channelId);
  React.useEffect(() => {
    if (prevChannelIdRef.current !== channelId) {
      prevChannelIdRef.current = channelId;
      reset();
    }
  }, [channelId, reset]);

  return React.useMemo(
    () => ({
      activeIndex,
      activeMatch,
      activeReveal,
      close,
      focusRequestId,
      goToNext,
      goToPrevious,
      isOpen,
      /**
       * What the MAIN timeline should scroll to for the active match: the
       * match itself, or — when the match is a thread reply — its thread root,
       * so the timeline points at the conversation while the panel shows the
       * reply. Null while the match is unreachable (for example a relay hit
       * whose event has not been spliced into the window yet), which keeps the
       * timeline from holding a target it can never mount.
       */
      mainTimelineActiveMatchId: activeReveal?.mainTimelineMessageId ?? null,
      matchCount: matchedIds.length,
      matchingMessageIds,
      query,
      setQuery: setQueryAndRestartNavigation,
    }),
    [
      activeIndex,
      activeMatch,
      activeReveal,
      close,
      focusRequestId,
      goToNext,
      goToPrevious,
      isOpen,
      matchedIds.length,
      matchingMessageIds,
      query,
      setQueryAndRestartNavigation,
    ],
  );
}
