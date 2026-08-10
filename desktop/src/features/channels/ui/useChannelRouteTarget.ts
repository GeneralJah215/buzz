import * as React from "react";

import type { MessageRevealTarget } from "@/features/messages/lib/messageRevealTarget";
import { resolveMessageRevealTarget } from "@/features/messages/lib/messageRevealTarget";
import type { TimelineMessage } from "@/features/messages/types";
import { isBroadcastReply } from "@/features/messages/lib/threading";
import type { Channel } from "@/shared/api/types";
import type { PanelValueSetter } from "./useChannelPanelHistoryState";

function getRouteMainTimelineTargetId(
  targetMessageId: string | null,
  targetMessage: TimelineMessage | null,
): string | null {
  if (!targetMessageId) {
    return null;
  }

  if (!targetMessage?.parentId || isBroadcastReply(targetMessage.tags ?? [])) {
    return targetMessageId;
  }

  return targetMessage.rootId ?? targetMessage.parentId;
}

export function useChannelRouteTarget({
  activeChannel,
  activeChannelId,
  closeAgentSession,
  setEditTargetId,
  setExpandedThreadReplyIds,
  setOpenThreadHeadId,
  setProfilePanelPubkey,
  setThreadReplyTargetId,
  setThreadScrollTargetId,
  searchRevealTarget = null,
  targetMessageId,
  timelineMessages,
}: {
  activeChannel: Channel | null;
  activeChannelId: string | null;
  closeAgentSession: () => void;
  setEditTargetId: React.Dispatch<React.SetStateAction<string | null>>;
  setExpandedThreadReplyIds: React.Dispatch<React.SetStateAction<Set<string>>>;
  setOpenThreadHeadId: PanelValueSetter;
  setProfilePanelPubkey: PanelValueSetter;
  setThreadReplyTargetId: React.Dispatch<React.SetStateAction<string | null>>;
  setThreadScrollTargetId: React.Dispatch<React.SetStateAction<string | null>>;
  /**
   * Where the active find-in-channel match can be revealed, already resolved
   * by `useChannelFind`. Only `thread-reply` targets need anything from this
   * hook: the main timeline handles its own rows, and an unreachable match has
   * nowhere to go.
   */
  searchRevealTarget?: MessageRevealTarget | null;
  targetMessageId: string | null;
  timelineMessages: TimelineMessage[];
}) {
  const timelineMessageById = React.useMemo(
    () => new Map(timelineMessages.map((message) => [message.id, message])),
    [timelineMessages],
  );
  const targetTimelineMessage = targetMessageId
    ? (timelineMessageById.get(targetMessageId) ?? null)
    : null;
  const mainTimelineTargetMessageId = getRouteMainTimelineTargetId(
    targetMessageId,
    targetTimelineMessage,
  );
  const handledThreadRouteTargetRef = React.useRef<string | null>(null);

  React.useEffect(() => {
    if (!targetMessageId) {
      handledThreadRouteTargetRef.current = null;
      return;
    }

    const targetKey = `${activeChannelId ?? "none"}:${targetMessageId}`;
    if (handledThreadRouteTargetRef.current !== targetKey) {
      handledThreadRouteTargetRef.current = null;
    }

    if (
      handledThreadRouteTargetRef.current === targetKey ||
      !activeChannel ||
      activeChannel.channelType === "forum"
    ) {
      return;
    }

    const targetMessage = timelineMessageById.get(targetMessageId) ?? null;
    if (!targetMessage) {
      return;
    }

    if (!targetMessage.parentId) {
      closeAgentSession();
      // Root message links should open the reply panel for that root. The
      // timeline scroll/highlight target alone is not enough: root links have
      // no parent/thread metadata, so the reply-only branch below cannot infer
      // a thread head.
      setProfilePanelPubkey(null, { replace: true });
      setEditTargetId(null);
      setOpenThreadHeadId(targetMessage.id, { replace: true });
      setThreadReplyTargetId(targetMessage.id);
      setThreadScrollTargetId(null);
      setExpandedThreadReplyIds(new Set());
      handledThreadRouteTargetRef.current = targetKey;
      return;
    }

    if (isBroadcastReply(targetMessage.tags ?? [])) {
      return;
    }

    const routeTarget = resolveMessageRevealTarget(
      targetMessageId,
      timelineMessageById,
    );
    if (routeTarget?.kind !== "thread-reply") {
      return;
    }

    closeAgentSession();
    // Replace so the deep-link entry itself carries the opened thread —
    // back should leave the deep link, not strip the panel from it.
    setProfilePanelPubkey(null, { replace: true });
    setEditTargetId(null);
    setOpenThreadHeadId(routeTarget.threadHeadId, { replace: true });
    setThreadReplyTargetId(routeTarget.threadHeadId);
    setThreadScrollTargetId(targetMessageId);
    setExpandedThreadReplyIds(new Set(routeTarget.expandedReplyIds));
    handledThreadRouteTargetRef.current = targetKey;
  }, [
    activeChannel,
    activeChannelId,
    closeAgentSession,
    setEditTargetId,
    setExpandedThreadReplyIds,
    setOpenThreadHeadId,
    setProfilePanelPubkey,
    setThreadReplyTargetId,
    setThreadScrollTargetId,
    targetMessageId,
    timelineMessageById,
  ]);

  // Reveal a find-in-channel match that lives in a thread reply (BUG-058).
  //
  // The main timeline filters every reply out of its row set, so `Enter` on a
  // reply match used to scroll to a row that does not exist. The reply DOES
  // have a home — the thread panel, opened on its root — and that is the same
  // composition a deep link to a reply already performs above. Reuse it.
  //
  // Deliberately narrow: only a `thread-reply` match touches panel state.
  //
  //   * A main-timeline match leaves the panel exactly as it is. Opening a
  //     panel on every root match (which is what the deep-link branch does for
  //     root links) would make Next/Previous flap panels open and shut down the
  //     whole result list — worse than the bug being fixed.
  //   * Moving from a reply match to a main-timeline match leaves the panel
  //     open too, so the layout stays put while the reader walks results.
  //   * Moving between replies in different threads re-targets the panel, which
  //     is the only way to show the new match at all.
  //
  // Keyed on the match id so re-entering the same match (a re-render, a churn
  // in `timelineMessages`) does not re-open or re-scroll anything.
  const revealedSearchMatchRef = React.useRef<string | null>(null);
  React.useEffect(() => {
    if (searchRevealTarget?.kind !== "thread-reply") {
      revealedSearchMatchRef.current = null;
      return;
    }

    const matchKey = `${activeChannelId ?? "none"}:${searchRevealTarget.messageId}`;
    if (revealedSearchMatchRef.current === matchKey) {
      return;
    }
    revealedSearchMatchRef.current = matchKey;

    closeAgentSession();
    setProfilePanelPubkey(null, { replace: true });
    setEditTargetId(null);
    setOpenThreadHeadId(searchRevealTarget.threadHeadId, { replace: true });
    setThreadReplyTargetId(searchRevealTarget.threadHeadId);
    setExpandedThreadReplyIds(new Set(searchRevealTarget.expandedReplyIds));
    setThreadScrollTargetId(searchRevealTarget.messageId);
  }, [
    activeChannelId,
    closeAgentSession,
    searchRevealTarget,
    setEditTargetId,
    setExpandedThreadReplyIds,
    setOpenThreadHeadId,
    setProfilePanelPubkey,
    setThreadReplyTargetId,
    setThreadScrollTargetId,
  ]);

  return mainTimelineTargetMessageId;
}
