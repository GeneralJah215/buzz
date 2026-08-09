/**
 * The channel browser's list pipeline and its Enter-key decision, extracted as
 * pure functions so both can be exercised without rendering React.
 *
 * WHY THIS IS A MODULE (BUG-043)
 * ------------------------------
 * `ChannelBrowserDialog` renders its channel list from a `useDeferredValue`
 * copy of the search query, because fuzzy-scoring every channel on every
 * keystroke is the expensive part of typing. The create row, by contrast, is
 * O(1) and reads the query the instant it changes.
 *
 * That is the right split for *rendering* and the wrong split for *deciding*.
 * When Enter arrived while the deferred list still described the previous
 * keystroke, the handler compared a create row computed from "the name you
 * just typed" against a list computed from "the name you typed a moment ago",
 * and the stale list won: typing a brand-new channel name and pressing Enter
 * quickly navigated you into an unrelated channel instead of creating it.
 *
 * The rule this module enforces is that **the immediate query is
 * authoritative for every branch of the decision**. When the rendered list was
 * built from a different query, it is not an answer about the current one, so
 * the list is recomputed synchronously for the authoritative query. That costs
 * one filter+sort on an Enter press — not on every keystroke — so the deferred
 * value keeps doing its job and no affordance lags behind the input.
 *
 * The rejected alternative was to defer the create row too. It also makes the
 * two agree, but it makes them agree on stale data: the create row would show
 * (and Enter would create) a name the user had already finished changing.
 */

import {
  type ChannelSortMode,
  sortChannelsForSidebar,
} from "@/features/sidebar/lib/channelSortPreference";
import type { Channel } from "@/shared/api/types";

import { channelNamesMatch } from "./canonicalChannelName";
import { scoreChannelMatch } from "./channelSearchScore";

export type ChannelBrowserTab = "all" | "joined" | "archived";
export type ChannelBrowserSort = ChannelSortMode | "members";
export type ChannelBrowserTypeFilter = "stream" | "forum";

export type ChannelBrowserListInput = {
  channels: Channel[];
  /** Canonical, lowercased query the returned list describes. */
  query: string;
  channelTypeFilter?: ChannelBrowserTypeFilter;
  activeTab: ChannelBrowserTab;
  sort: ChannelBrowserSort;
};

/**
 * Fuzzy match score per channel id for `query`, so filtering and
 * relevance-ordering share one source of truth. Empty when there is no query.
 */
function scoreChannels(
  channels: Channel[],
  query: string,
): Map<string, number> {
  const scores = new Map<string, number>();
  if (query.length === 0) return scores;
  for (const channel of channels) {
    const score = scoreChannelMatch(channel, query);
    if (score !== null) scores.set(channel.id, score);
  }
  return scores;
}

/**
 * The channel list the browser shows for one exact query, in visual order.
 *
 * Pure and total: given the same inputs it always returns the same order, so
 * the Enter handler can recompute it for the live query whenever the rendered
 * (deferred) list disagrees.
 */
export function selectOrderedChannels({
  channels,
  query,
  channelTypeFilter,
  activeTab,
  sort,
}: ChannelBrowserListInput): Channel[] {
  const matchScoreById = scoreChannels(channels, query);

  const browsable = channels.filter(
    (channel) =>
      channel.channelType !== "dm" &&
      (channel.archivedAt
        ? channel.isMember
        : channel.visibility === "open" || channel.isMember) &&
      (channelTypeFilter ? channel.channelType === channelTypeFilter : true),
  );

  const matchingChannels =
    query.length === 0
      ? browsable
      : browsable.filter((channel) => matchScoreById.has(channel.id));

  const visibleChannels =
    activeTab === "archived"
      ? matchingChannels.filter((channel) => channel.archivedAt !== null)
      : activeTab === "joined"
        ? matchingChannels.filter(
            (channel) => channel.archivedAt === null && channel.isMember,
          )
        : matchingChannels;

  const sorted =
    sort === "members"
      ? [...visibleChannels].sort(
          (a, b) =>
            b.memberCount - a.memberCount ||
            a.name.localeCompare(b.name, undefined, { sensitivity: "base" }),
        )
      : sortChannelsForSidebar(visibleChannels, sort);

  if (query.length === 0) return sorted;

  return sorted.sort(
    (a, b) =>
      (matchScoreById.get(a.id) ?? Number.POSITIVE_INFINITY) -
      (matchScoreById.get(b.id) ?? Number.POSITIVE_INFINITY),
  );
}

/**
 * Whether a channel by exactly this name already exists — if so the browser
 * doesn't offer to create a duplicate, mirroring how you'd never make two
 * "#general"s.
 */
export function hasExactChannelNameMatch({
  channels,
  query,
  channelTypeFilter,
}: {
  channels: Channel[];
  query: string;
  channelTypeFilter?: ChannelBrowserTypeFilter;
}): boolean {
  return channels.some(
    (channel) =>
      channel.channelType !== "dm" &&
      channelNamesMatch(channel.name, query) &&
      (channelTypeFilter ? channel.channelType === channelTypeFilter : true),
  );
}

/** Whether the pinned create row is offered for `query`. */
export function shouldShowCreateRow({
  canCreate,
  channels,
  query,
  channelTypeFilter,
}: {
  canCreate: boolean;
  channels: Channel[];
  query: string;
  channelTypeFilter?: ChannelBrowserTypeFilter;
}): boolean {
  return (
    canCreate &&
    !hasExactChannelNameMatch({ channels, query, channelTypeFilter })
  );
}

export type ChannelBrowserEnterAction =
  | { kind: "none" }
  | { kind: "create"; name: string }
  | { kind: "select"; channel: Channel };

export type ChannelBrowserEnterInput = {
  channels: Channel[];
  /**
   * The canonical query as typed, NOT lowercased — this is the authoritative
   * query, and also the name a create action carries forward.
   */
  query: string;
  /**
   * The lowercased query `renderedChannels` was actually built from. Under
   * `useDeferredValue` this trails `query` by one or more keystrokes.
   */
  renderedQuery: string;
  /** The list currently on screen, built from `renderedQuery`. */
  renderedChannels: Channel[];
  channelTypeFilter?: ChannelBrowserTypeFilter;
  activeTab: ChannelBrowserTab;
  sort: ChannelBrowserSort;
  canCreate: boolean;
  selectedIndex: number | null;
};

/**
 * Decide what Enter does in the channel browser search box.
 *
 * Every branch below reads `query` — the text in the input the user is looking
 * at. `renderedChannels` is used only when it provably describes that same
 * query; otherwise the list is recomputed. A stale list must never be the
 * thing that answers "does a channel by this name exist?", because the answer
 * it gives is about a name the user has already stopped typing (BUG-043).
 */
export function resolveChannelBrowserEnterAction({
  channels,
  query,
  renderedQuery,
  renderedChannels,
  channelTypeFilter,
  activeTab,
  sort,
  canCreate,
  selectedIndex,
}: ChannelBrowserEnterInput): ChannelBrowserEnterAction {
  const authoritativeQuery = query.toLowerCase();

  const channelsForQuery =
    renderedQuery === authoritativeQuery
      ? renderedChannels
      : selectOrderedChannels({
          activeTab,
          channelTypeFilter,
          channels,
          query: authoritativeQuery,
          sort,
        });

  const showCreateRow = shouldShowCreateRow({
    canCreate,
    channelTypeFilter,
    channels,
    query: authoritativeQuery,
  });

  // The create row is pinned at index 0 when present, so channels shift down
  // by one and keyboard order stays identical to visual order.
  const channelNavOffset = showCreateRow ? 1 : 0;
  const isCreateRowSelected = showCreateRow && selectedIndex === 0;

  // If the create row is highlighted — or it's the only actionable item,
  // because nothing matches the query the user actually typed — Enter creates.
  if (showCreateRow && (isCreateRowSelected || channelsForQuery.length === 0)) {
    return { kind: "create", name: query };
  }

  if (channelsForQuery.length > 0) {
    const highlighted =
      selectedIndex === null || isCreateRowSelected
        ? undefined
        : channelsForQuery[selectedIndex - channelNavOffset];
    return { kind: "select", channel: highlighted ?? channelsForQuery[0] };
  }

  return { kind: "none" };
}
