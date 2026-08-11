import * as React from "react";

import type { ActiveTurnSummary } from "@/features/agents/activeAgentTurnsStore";
import { subscribeActiveAgentTurns } from "@/features/agents/activeAgentTurnsStore";
import { isManagedAgentActive } from "@/features/agents/lib/managedAgentControlActions";
import {
  getAgentObserverSnapshot,
  getAgentTranscript,
} from "@/features/agents/observerRelayStore";
import { subscribeAgentObserverStoreForAgent } from "@/features/agents/agentScopedObserverSubscription";
import type {
  ObserverEvent,
  TranscriptItem,
} from "@/features/agents/ui/agentSessionTypes";
import type { ProfileActivityAgent } from "@/features/profile/lib/profileActivityAgent";
import { normalizePubkey } from "@/shared/lib/pubkey";

export type ProfileActivityFeedScope = {
  /** Distinct channel ids to surface in the embed switcher. */
  channelIds: string[];
  /** Whether the observer feed has any events or transcript for this agent. */
  hasFeedContent: boolean;
  /** True while the active-turn store reports live work for this agent. */
  isLive: boolean;
  /** Latest observed activity timestamp, keyed by channel id. */
  latestActivityAtByChannel: Record<string, number>;
  /** Preferred channel scope when no explicit selection exists yet. */
  preferredChannelId: string | null;
};

const cachedScopes = new Map<string, ProfileActivityFeedScope>();

const EMPTY_EVENTS: readonly ObserverEvent[] = [];
const EMPTY_TRANSCRIPT: readonly TranscriptItem[] = [];

/**
 * Last derivation per agent, keyed on the IDENTITY of the three inputs.
 *
 * BUG-067. `stableFeedScope` already guaranteed a stable output reference, so
 * `useSyncExternalStore` never looped — but it bought that stability by running
 * the full `deriveProfileActivityFeedScope` (a scan of the whole event journal
 * plus the whole transcript) on every call just to discover nothing changed.
 * `useSyncExternalStore` calls `getSnapshot` on every render AND on every
 * notification from both stores, so with 29 agents streaming that scan ran
 * continuously.
 *
 * The three inputs are all reference-stable when unchanged — `events` is the
 * store's own array, `transcript` is `TranscriptState.items`, and `activeTurns`
 * comes from `getActiveTurnsForAgent`'s summary cache — so identity comparison
 * is a sound "nothing changed" test and never returns a stale scope.
 */
type FeedScopeDerivation = {
  activeTurns: readonly ActiveTurnSummary[];
  events: readonly ObserverEvent[];
  transcript: readonly TranscriptItem[];
  scope: ProfileActivityFeedScope;
};

const derivationCache = new Map<string, FeedScopeDerivation>();

// Number of times the full `deriveProfileActivityFeedScope` scan actually ran.
// The BUG-067 regression asserts on this counter rather than on elapsed time.
let derivationCount = 0;

/** Test-only: full feed-scope derivations since the last cache reset. */
export function _testGetFeedScopeDerivationCount(): number {
  return derivationCount;
}

function deriveFeedScopeForCacheKey(
  cacheKey: string,
  activeTurns: readonly ActiveTurnSummary[],
  events: readonly ObserverEvent[],
  transcript: readonly TranscriptItem[],
): ProfileActivityFeedScope {
  const cached = derivationCache.get(cacheKey);
  if (
    cached &&
    cached.activeTurns === activeTurns &&
    cached.events === events &&
    cached.transcript === transcript
  ) {
    return cached.scope;
  }

  // stableFeedScope still runs: the derivation can produce a value-equal scope
  // from different input references (e.g. a new event that adds no channel and
  // no newer timestamp), and returning a fresh object then would re-render for
  // nothing.
  derivationCount += 1;
  const scope = stableFeedScope(
    cacheKey,
    deriveProfileActivityFeedScope({ activeTurns, events, transcript }),
  );
  derivationCache.set(cacheKey, { activeTurns, events, transcript, scope });
  return scope;
}

function channelIdsEqual(
  left: readonly string[],
  right: readonly string[],
): boolean {
  if (left.length !== right.length) {
    return false;
  }

  for (let index = 0; index < left.length; index += 1) {
    if (left[index] !== right[index]) {
      return false;
    }
  }

  return true;
}

function scopesEqual(
  left: ProfileActivityFeedScope,
  right: ProfileActivityFeedScope,
): boolean {
  return (
    left.hasFeedContent === right.hasFeedContent &&
    left.isLive === right.isLive &&
    left.preferredChannelId === right.preferredChannelId &&
    latestActivityByChannelEqual(
      left.latestActivityAtByChannel,
      right.latestActivityAtByChannel,
    ) &&
    channelIdsEqual(left.channelIds, right.channelIds)
  );
}

function latestActivityByChannelEqual(
  left: Record<string, number>,
  right: Record<string, number>,
): boolean {
  const leftKeys = Object.keys(left);
  const rightKeys = Object.keys(right);
  if (leftKeys.length !== rightKeys.length) {
    return false;
  }

  for (const key of leftKeys) {
    if (left[key] !== right[key]) {
      return false;
    }
  }

  return true;
}

function stableFeedScope(
  cacheKey: string,
  next: ProfileActivityFeedScope,
): ProfileActivityFeedScope {
  const cached = cachedScopes.get(cacheKey);
  if (cached && scopesEqual(cached, next)) {
    return cached;
  }

  cachedScopes.set(cacheKey, next);
  return next;
}

/** Test-only: drop both memo caches so one test cannot seed another. */
export function _testResetProfileActivityFeedScopeCaches() {
  cachedScopes.clear();
  derivationCache.clear();
  derivationCount = 0;
}

function collectChannelIdsFromFeed(
  events: readonly ObserverEvent[],
  transcript: readonly TranscriptItem[],
): string[] {
  const channelIds = new Set<string>();
  for (const event of events) {
    if (event.channelId) {
      channelIds.add(event.channelId);
    }
  }
  for (const item of transcript) {
    if (item.channelId) {
      channelIds.add(item.channelId);
    }
  }
  return [...channelIds].sort((left, right) => left.localeCompare(right));
}

function deriveLatestChannelId(
  events: readonly ObserverEvent[],
  transcript: readonly TranscriptItem[],
): string | null {
  for (let index = transcript.length - 1; index >= 0; index -= 1) {
    const channelId = transcript[index]?.channelId;
    if (channelId) {
      return channelId;
    }
  }

  for (let index = events.length - 1; index >= 0; index -= 1) {
    const channelId = events[index]?.channelId;
    if (channelId) {
      return channelId;
    }
  }

  return null;
}

function parseTimestampMillis(timestamp: string): number | null {
  const millis = Date.parse(timestamp);
  return Number.isNaN(millis) ? null : millis;
}

function collectLatestActivityAtByChannel({
  activeTurns,
  events,
  transcript,
}: {
  activeTurns: readonly ActiveTurnSummary[];
  events: readonly ObserverEvent[];
  transcript: readonly TranscriptItem[];
}): Record<string, number> {
  const latestActivityAtByChannel: Record<string, number> = {};

  const record = (channelId: string | null | undefined, timestamp: number) => {
    if (!channelId) {
      return;
    }
    const previous = latestActivityAtByChannel[channelId];
    if (previous === undefined || timestamp > previous) {
      latestActivityAtByChannel[channelId] = timestamp;
    }
  };

  for (const turn of activeTurns) {
    record(turn.channelId, turn.anchorAt);
  }

  for (const event of events) {
    const timestamp = parseTimestampMillis(event.timestamp);
    if (timestamp !== null) {
      record(event.channelId, timestamp);
    }
  }

  for (const item of transcript) {
    const timestamp = parseTimestampMillis(item.timestamp);
    if (timestamp !== null) {
      record(item.channelId, timestamp);
    }
  }

  return latestActivityAtByChannel;
}

export function deriveProfileActivityFeedScope({
  activeTurns,
  events,
  transcript,
}: {
  activeTurns: readonly ActiveTurnSummary[];
  events: readonly ObserverEvent[];
  transcript: readonly TranscriptItem[];
}): ProfileActivityFeedScope {
  const hasFeedContent = events.length > 0 || transcript.length > 0;
  const isLive = activeTurns.length > 0;
  const latestActivityAtByChannel = collectLatestActivityAtByChannel({
    activeTurns,
    events,
    transcript,
  });

  if (isLive) {
    const channelIds = [...activeTurns]
      .map((turn) => turn.channelId)
      .sort((left, right) => left.localeCompare(right));

    return {
      channelIds,
      hasFeedContent: true,
      isLive: true,
      latestActivityAtByChannel,
      preferredChannelId: channelIds[0] ?? null,
    };
  }

  const feedChannelIds = collectChannelIdsFromFeed(events, transcript);
  const latestChannelId = deriveLatestChannelId(events, transcript);

  return {
    channelIds: feedChannelIds,
    hasFeedContent,
    isLive: false,
    latestActivityAtByChannel,
    preferredChannelId: latestChannelId,
  };
}

export function useProfileActivityFeedScope(
  activityAgent: ProfileActivityAgent | null,
  activeTurns: readonly ActiveTurnSummary[],
): ProfileActivityFeedScope {
  const agentCacheKey = activityAgent
    ? normalizePubkey(activityAgent.pubkey)
    : "none";
  const hasObserver =
    activityAgent !== null && isManagedAgentActive(activityAgent);

  const getSnapshot = React.useCallback(() => {
    if (!activityAgent || !hasObserver) {
      // Module-level empties, not fresh `[]` literals: a fresh array would miss
      // the identity check in deriveFeedScopeForCacheKey on every single call.
      return deriveFeedScopeForCacheKey(
        agentCacheKey,
        activeTurns,
        EMPTY_EVENTS,
        EMPTY_TRANSCRIPT,
      );
    }

    const { events } = getAgentObserverSnapshot(activityAgent.pubkey, true);
    const transcript = getAgentTranscript(activityAgent.pubkey, true);
    return deriveFeedScopeForCacheKey(
      agentCacheKey,
      activeTurns,
      events,
      transcript,
    );
  }, [activeTurns, activityAgent, agentCacheKey, hasObserver]);

  // BUG-067, two separate defects in the old inline arrow:
  //
  //  1. It was recreated on every render, so `useSyncExternalStore` tore down
  //     and re-established BOTH subscriptions on every render.
  //  2. The observer half ignored the changed-agent key, so all 29 streaming
  //     agents woke this feed on every frame.
  //
  // `deriveProfileActivityFeedScope` reads exactly one agent (`activityAgent`),
  // never an aggregate across agents, so scoping the observer subscription to
  // that agent is correct. The active-turns half stays unfiltered — that store
  // has no per-agent notification key, and `activeTurns` arrives as an argument
  // rather than being read in `getSnapshot`, so filtering it here would be
  // filtering on data this module does not own.
  const agentPubkey = activityAgent?.pubkey ?? null;
  const subscribe = React.useCallback(
    (onStoreChange: () => void) => {
      const unsubscribeObserver = subscribeAgentObserverStoreForAgent(
        agentPubkey,
        onStoreChange,
      );
      const unsubscribeTurns = subscribeActiveAgentTurns(onStoreChange);
      return () => {
        unsubscribeObserver();
        unsubscribeTurns();
      };
    },
    [agentPubkey],
  );

  const snapshot = React.useSyncExternalStore(subscribe, getSnapshot);

  return snapshot;
}
