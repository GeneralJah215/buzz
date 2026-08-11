import * as React from "react";

import { useStableArrayShallow } from "@/shared/hooks/useStableReference";
import {
  mergeObserverEventWindows,
  scopeByChannel,
} from "./agentSessionPanelLayout";
import { buildTranscriptState } from "./agentSessionTranscript";
import type { ObserverEvent, TranscriptItem } from "./agentSessionTypes";

// Number of full live+archive transcript rebuilds a session panel performed.
// BUG-067's regression asserts on this counter, never on elapsed time — a
// timing threshold can be widened until it passes, a call count cannot.
let panelTranscriptBuildCount = 0;

/** Test-only: panel transcript rebuilds since the last reset. */
export function _testGetPanelTranscriptBuildCount(): number {
  return panelTranscriptBuildCount;
}

/** Test-only: zero the panel transcript rebuild counter. */
export function _testResetPanelTranscriptBuildCount(): void {
  panelTranscriptBuildCount = 0;
}

/**
 * Derive a session panel's channel-scoped transcript from the agent's live
 * event window plus this channel's archive pages.
 *
 * ── Why this is a hook and not three inline useMemos ─────────────────────────
 * BUG-067. `getAgentObserverSnapshot` hands back a new events array on EVERY
 * frame the agent streams — including a frame belonging to a channel this panel
 * is not showing, because the live journal is per-agent, not per-channel.
 * `scopeByChannel` then allocated a fresh, element-identical array, which broke
 * the merge's `useMemo`, which broke the rebuild's `useMemo`. The result was a
 * full O(live + archive) `buildTranscriptState` on every frame of every channel
 * the agent was talking in — the third of BUG-065's three per-frame rebuilds.
 *
 * `useStableArrayShallow` fixes it by VALUE, not by filtering: identical
 * contents keep the previous reference, so the merge and the rebuild are
 * skipped. When the panel's own channel genuinely gains a frame the contents
 * differ, the reference changes, and the rebuild runs exactly as before. There
 * is no path here that can make a displayed transcript go stale.
 *
 * Lives in its own module so the regression can mount it without importing the
 * panel's `.tsx` (which reads `import.meta.env`, unavailable under node).
 */
export function useSessionPanelTranscript(
  events: readonly ObserverEvent[],
  channelId: string | null | undefined,
  archivedChannelEvents: readonly ObserverEvent[],
): {
  combinedEvents: ObserverEvent[];
  derivedTranscript: TranscriptItem[];
} {
  const scopedLiveEvents = useStableArrayShallow(
    React.useMemo(() => scopeByChannel(events, channelId), [channelId, events]),
  );

  // Combined raw window: live (scoped) + archive merged by (seq, timestamp),
  // sorted ascending. Single source for both the transcript and the raw rail.
  const combinedEvents = React.useMemo(
    () => mergeObserverEventWindows(scopedLiveEvents, archivedChannelEvents),
    [scopedLiveEvents, archivedChannelEvents],
  );

  const derivedTranscript = React.useMemo(() => {
    panelTranscriptBuildCount += 1;
    return buildTranscriptState(combinedEvents).items;
  }, [combinedEvents]);

  return { combinedEvents, derivedTranscript };
}
