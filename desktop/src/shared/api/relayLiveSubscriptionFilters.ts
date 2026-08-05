import type { RelaySubscriptionFilter } from "@/shared/api/relayClientShared";
import {
  CHANNEL_EVENT_KINDS,
  KIND_CHANNEL_THREAD_SUMMARY,
  KIND_THREAD_DIRECTORY_ITEM,
} from "@/shared/constants/kinds";

export function channelFilter(channelId: string): RelaySubscriptionFilter {
  return {
    // 39005 rides only this window-store subscription — not
    // CHANNEL_EVENT_KINDS, whose other consumers (unread tracking,
    // timeline-cache merges) must never see summary overlays.
    kinds: [...CHANNEL_EVENT_KINDS, KIND_CHANNEL_THREAD_SUMMARY],
    "#h": [channelId],
    limit: 1000,
    since: Math.floor(Date.now() / 1_000),
  };
}

export function threadDirectoryFilter(
  channelId: string,
): RelaySubscriptionFilter {
  return {
    kinds: [KIND_THREAD_DIRECTORY_ITEM],
    "#h": [channelId],
    limit: 0,
    since: Math.floor(Date.now() / 1_000),
  };
}
