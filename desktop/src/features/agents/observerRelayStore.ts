import * as React from "react";

import { subscribeToAgentObserverFrames } from "@/shared/api/observerRelay";
import type { RelayEvent, ManagedAgent } from "@/shared/api/types";
import type { ControlResultFrame } from "@/shared/api/types";
import { putAgentSessionConfig } from "@/shared/api/tauri";
import { putManagedAgentRuntimeLifecycle } from "@/shared/api/tauriManagedAgents";
import { getIdentity } from "@/shared/api/tauriIdentity";
import { decryptObserverEvent } from "@/shared/api/tauriObserver";
import {
  parseAgentManagementRequest,
  type AgentManagementRequest,
} from "./agentManagement";
import { normalizePubkey } from "@/shared/lib/pubkey";
import { useQueryClient } from "@tanstack/react-query";
import { agentConfigSurfaceQueryKey } from "@/features/agents/hooks";
import type {
  ConnectionState,
  ObserverEvent,
  TranscriptItem,
} from "./ui/agentSessionTypes";
import {
  type TranscriptState,
  createEmptyTranscriptState,
} from "./ui/agentSessionTranscript";
import {
  CONTROL_RESULT_GAP_STATUS,
  type ObserverGap,
  buildTranscriptStateWithGaps,
  detectRelaySeqGap,
  parseObserverGapFrame,
  processTranscriptEventWithGaps,
  syntheticGapEvent,
} from "./observerGapDetection";
import {
  appendArchivedChannelEvent,
  clearArchivedChannelEvents,
  readArchivedChannelEvents,
} from "./archiveEventWindow";
import {
  compareObserverEvents,
  isObserverEventAfter,
} from "./observerEventOrder";
import {
  type TranscriptWindowIndex,
  collectTouchedItemIds,
  createTranscriptWindowIndex,
  dropTranscriptItems,
  observerEventKey,
  recordTouchedItems,
  seedTranscriptWindowIndex,
  takeEvictedItemIds,
} from "./transcriptWindow";

export const MAX_OBSERVER_EVENTS = 3000;
const MAX_PENDING_UNKNOWN_AGENT_FRAMES = 100;

export type ObserverSnapshot = {
  connectionState: ConnectionState;
  errorMessage: string | null;
  events: ObserverEvent[];
};

const IDLE_SNAPSHOT: ObserverSnapshot = {
  connectionState: "idle",
  errorMessage: null,
  events: [],
};

const EMPTY_EVENTS: ObserverEvent[] = [];
const EMPTY_TRANSCRIPT: TranscriptItem[] = [];

const listeners = new Set<(changedAgentKey: string | null) => void>();
const eventsByAgent = new Map<string, ObserverEvent[]>();
const transcriptByAgent = new Map<string, TranscriptState>();
const snapshotByAgent = new Map<string, ObserverSnapshot>();

// Per-agent map of "which transcript item did each journal event last touch",
// so an event leaving the capped journal can evict exactly its own items
// instead of forcing a full transcript rebuild (BUG-065). See transcriptWindow.
const transcriptWindowByAgent = new Map<string, TranscriptWindowIndex>();

// The channel-scoped archive event journal now lives in `archiveEventWindow`.
// Its contract is unchanged — the live relay path writes to `eventsByAgent`
// (per-agent, capped) and never to the archive, so loading deep history can
// never evict live frames or vice versa. See that module for what it adds.

// Per-agent, per-channel latest-live-session-id.
// Key: `${normalizePubkey(agentPubkey)}:${channelId}`.
// Set when a live relay observer event with a sessionId arrives.
// Cleared in resetAgentObserverStore.
//
// "Latest-live" means: the sessionId that most recently appeared via the
// live relay path (handleRelayObserverEvent). It is NOT derived from
// connectionState or an ever-live Set — an ever-live Set would incorrectly
// mark session A as "current" after session B has started (Thufir Pass 3).
//
// Stored as `{ sessionId, timestamp, seq }` so that late-arriving live frames
// from an older session never regress the latest-live id. We only advance when
// the parsed event sorts strictly AFTER the stored one, using the same
// two-key ordering as `compareObserverEvents`: timestamp first, then seq on a
// tie — so a higher-seq frame at equal timestamp still advances the entry.
type LatestLiveEntry = { sessionId: string; timestamp: string; seq: number };
const latestLiveSessionByAgentChannel = new Map<string, LatestLiveEntry>();

// Highest observer `seq` ingested per agent, and every discontinuity seen.
//
// Observer frames are lossy in two independent places — a 1000-slot broadcast
// inside the harness and the relay hop — and four consumers here used to treat
// them as reliable. `seq` is monotonic, so a hole is detectable; these two maps
// are what turns a detected hole into a fact the rest of the app can read
// instead of a silence it cannot.
const lastSeqByAgent = new Map<string, number>();
const gapsByAgent = new Map<string, ObserverGap[]>();
const EMPTY_GAPS: ObserverGap[] = [];

// Frame loss under sustained load is "structural, not bad luck"
// (observerGapDetection), so this list grew for the whole session and every
// append copied it. Keep the newest window of gap RECORDS...
export const MAX_OBSERVER_GAPS_PER_AGENT = 200;
// ...and keep the one fact a record carries that a caller acts on — "frames
// were lost that nothing could refill" — in a set that is never trimmed. So
// trimming the list can only lose detail, never the verdict: the divergence
// from an uncapped list retains MORE than the list alone, never less.
const unrecoverableGapAgents = new Set<string>();

/**
 * Every observer discontinuity recorded for an agent, oldest first.
 *
 * A caller that needs to know whether this agent's *state* — runtime lifecycle,
 * model switch outcome, session config — can still be trusted should look for
 * any entry with `controlComplete: false`. That is the loud, named state: the
 * harness could not refill the hole, and nothing on this side can either.
 */
export function getAgentObserverGaps(
  agentPubkey: string | null | undefined,
): readonly ObserverGap[] {
  if (!agentPubkey) return EMPTY_GAPS;
  return gapsByAgent.get(normalizePubkey(agentPubkey)) ?? EMPTY_GAPS;
}

/** True when frames were lost that nothing could recover for this agent. */
export function isAgentObserverStateStale(
  agentPubkey: string | null | undefined,
): boolean {
  if (!agentPubkey) return false;
  // Read the sticky set rather than scanning the (now capped) list, so an agent
  // that lost frames stays marked stale even after 200 later gaps push the
  // original record out of the retained window.
  return unrecoverableGapAgents.has(normalizePubkey(agentPubkey));
}

function recordObserverGap(agentPubkey: string, gap: ObserverGap) {
  const key = normalizePubkey(agentPubkey);
  if (!gap.controlComplete) {
    unrecoverableGapAgents.add(key);
  }
  const current = gapsByAgent.get(key) ?? EMPTY_GAPS;
  const overflow = current.length + 1 - MAX_OBSERVER_GAPS_PER_AGENT;
  gapsByAgent.set(
    key,
    overflow > 0 ? [...current.slice(overflow), gap] : [...current, gap],
  );
  if (gap.controlComplete) {
    // The harness refilled the control plane from its own replay ring. Content
    // is still missing — the transcript marker says so — but no waiting caller
    // has been stranded, so do not fabricate a failure for one.
    return;
  }
  console.warn(
    `[GUARDRAIL] observer frames lost for agent ${key} (${gap.source}): ` +
      `fromSeq=${gap.fromSeq} toSeq=${gap.toSeq} unrecoverable=${gap.unrecoverable}`,
  );
  // A `control_result` destroyed in the hole is never re-sent, so a caller
  // awaiting one waits forever. Tell the waiters the reply is gone rather than
  // leaving them hanging on a frame that will not arrive.
  dispatchControlResult(agentPubkey, {
    type: "switch_model",
    status: CONTROL_RESULT_GAP_STATUS,
  });
}

/**
 * Reconcile one arriving frame against the `seq` stream, before it is ingested.
 *
 * Two independent losses are possible and they need different verdicts:
 *
 * - A `seq` jump with no announcement means frames vanished *after* the harness
 *   published them. Nothing on this side of the relay can refill it, so it is
 *   recorded as unrecoverable and injected into the journal as a real
 *   `observer_gap` event — which is what makes the marker survive the store's
 *   full-rebuild path instead of being a transient decoration.
 * - An `observer_gap` frame is the harness's own receipt. It may report the
 *   hole as already refilled from its control replay ring, in which case the
 *   agent's lifecycle, RPC completions and session config are current again.
 *
 * Runs before ingest so the marker sorts ahead of the frame that revealed it.
 * Exported for tests: this is the seam where a dropped frame stops being
 * invisible, and it must be provable without a live relay.
 */
export function trackObserverContinuity(
  agentPubkey: string,
  parsed: ObserverEvent,
) {
  const key = normalizePubkey(agentPubkey);
  const relayGap = detectRelaySeqGap(lastSeqByAgent.get(key), parsed);
  if (relayGap) {
    recordObserverGap(agentPubkey, relayGap);
    appendAgentEvent(agentPubkey, syntheticGapEvent(relayGap, parsed));
  }
  lastSeqByAgent.set(key, Math.max(lastSeqByAgent.get(key) ?? 0, parsed.seq));

  const harnessGap = parseObserverGapFrame(parsed);
  if (harnessGap) {
    recordObserverGap(agentPubkey, harnessGap);
  }
}

function liveSessionKey(agentPubkey: string, channelId: string | null): string {
  return `${normalizePubkey(agentPubkey)}:${channelId ?? ""}`;
}

/** Read the latest-live-session-id for a (agent, channel) pair. */
export function getLatestLiveSessionId(
  agentPubkey: string | null | undefined,
  channelId: string | null | undefined,
): string | null {
  if (!agentPubkey) return null;
  return (
    latestLiveSessionByAgentChannel.get(
      liveSessionKey(agentPubkey, channelId ?? null),
    )?.sessionId ?? null
  );
}

// Per-agent listeners for `control_result` frames. The ModelPicker subscribes
// here to learn the async outcome of a `switch_model` frame (the send is
// fire-and-forget; the harness replies out-of-band over the observer relay).
const controlResultListeners = new Map<
  string,
  Set<(frame: ControlResultFrame) => void>
>();

const agentManagementListeners = new Set<
  (agentPubkey: string, request: AgentManagementRequest) => void
>();

// Normalized pubkeys of agents we are actively managing. Only events whose
// "agent" tag matches an entry here will be decrypted (defense-in-depth).
//
// This set is the *union* of every active subscriber's contribution. Multiple
// callers of `useManagedAgentObserverBridge` (e.g. the channel screen and the
// profile panel) can be mounted at once, each tracking a different agent list.
// We key each subscriber's contribution in `knownAgentsBySubscription` and
// recompute the union, so co-mounted callers no longer clobber each other.
const knownAgentPubkeys = new Set<string>();
const knownAgentsBySubscription = new Map<string, Set<string>>();
const pendingUnknownAgentFrames: RelayEvent[] = [];

// Callback invoked when session_config_captured is received, so React Query
// can invalidate the config-surface query for the affected agent. Wired up
// by useManagedAgentObserverBridge via setSessionConfigCapturedCallback.
let onSessionConfigCaptured: ((pubkey: string) => void) | null = null;

export function setSessionConfigCapturedCallback(
  cb: ((pubkey: string) => void) | null,
) {
  onSessionConfigCaptured = cb;
}

function recomputeKnownAgentPubkeys() {
  knownAgentPubkeys.clear();
  for (const subscriptionAgents of knownAgentsBySubscription.values()) {
    for (const pubkey of subscriptionAgents) {
      knownAgentPubkeys.add(pubkey);
    }
  }
}

function registerKnownAgents(
  subscriptionId: string,
  pubkeys: readonly string[],
) {
  knownAgentsBySubscription.set(
    subscriptionId,
    new Set(pubkeys.map((pubkey) => normalizePubkey(pubkey))),
  );
  recomputeKnownAgentPubkeys();
  if (knownAgentPubkeys.size > 0 && pendingUnknownAgentFrames.length > 0) {
    const pending = pendingUnknownAgentFrames.splice(0);
    for (const event of pending) {
      eventProcessingQueue = eventProcessingQueue.then(() =>
        handleRelayObserverEvent(event, generation),
      );
    }
  }
}

function unregisterKnownAgents(subscriptionId: string) {
  if (knownAgentsBySubscription.delete(subscriptionId)) {
    recomputeKnownAgentPubkeys();
  }
}

let connectionState: ConnectionState = "idle";
let errorMessage: string | null = null;
let unsubscribeRelay: (() => Promise<void>) | null = null;
let startPromise: Promise<void> | null = null;
let eventProcessingQueue: Promise<void> = Promise.resolve();
let generation = 0;
// Count of full transcript rebuilds. The regression for BUG-065 asserts on this
// counter rather than on elapsed time: a timing threshold can be widened until
// it passes, a call count cannot.
let transcriptRebuildCount = 0;

/** Test-only: full transcript rebuilds performed since the last store reset. */
export function _testGetTranscriptRebuildCount(): number {
  return transcriptRebuildCount;
}

/**
 * `changedAgentKey` is the normalized pubkey of the single agent whose journal
 * changed, or `null` when the change is store-wide (connection state, reset, a
 * multi-agent archive page). Subscribers that would otherwise re-scan every
 * agent on every frame use it to do O(1) work instead of O(agents × journal)
 * — see `useActiveAgentTurnsBridge` (BUG-065 rank 2).
 */
function notifyListeners(changedAgentKey: string | null = null) {
  for (const listener of listeners) {
    listener(changedAgentKey);
  }
}

function invalidateSnapshot(key: string) {
  snapshotByAgent.delete(key);
}

function setConnectionState(
  nextState: ConnectionState,
  nextErrorMessage: string | null = errorMessage,
) {
  connectionState = nextState;
  errorMessage = nextErrorMessage;
  snapshotByAgent.clear();
  notifyListeners();
}

function observerTag(event: RelayEvent, tagName: string) {
  return event.tags.find((tag) => tag[0] === tagName)?.[1] ?? null;
}

function appendAgentEvent(agentPubkey: string, event: ObserverEvent) {
  const key = normalizePubkey(agentPubkey);
  const current = eventsByAgent.get(key) ?? [];
  if (
    current.some(
      (existing) =>
        existing.seq === event.seq && existing.timestamp === event.timestamp,
    )
  ) {
    return;
  }

  // The journal is kept sorted, so an in-order frame — every frame, in normal
  // operation — needs one comparison rather than a 3000-element sort whose
  // comparator parses two ISO timestamps per call (BUG-065 rank 3).
  const newest = current.length > 0 ? current[current.length - 1] : null;
  const arrivedInOrder = !newest || compareObserverEvents(newest, event) <= 0;
  const sorted = arrivedInOrder
    ? [...current, event]
    : [...current, event].sort(compareObserverEvents);

  const overflow = sorted.length - MAX_OBSERVER_EVENTS;
  const final = overflow > 0 ? sorted.slice(overflow) : sorted;
  eventsByAgent.set(key, final);

  // Whether the new event landed at the end of the sorted array. If it did
  // (the common case) only this event needs processing, even when the append
  // pushed older events out of the journal — those are handled by evicting the
  // transcript items they own. A genuinely out-of-order arrival is the only
  // case that still needs a full rebuild.
  const eventAtEnd = arrivedInOrder;

  let index = transcriptWindowByAgent.get(key);
  if (!index) {
    index = createTranscriptWindowIndex();
    transcriptWindowByAgent.set(key, index);
  }

  if (eventAtEnd) {
    const previous = transcriptByAgent.get(key) ?? createEmptyTranscriptState();
    const appended = processTranscriptEventWithGaps(previous, event);
    recordTouchedItems(
      index,
      observerEventKey(event),
      collectTouchedItemIds(previous, appended),
    );
    const evicted =
      overflow > 0
        ? takeEvictedItemIds(
            index,
            sorted.slice(0, overflow).map(observerEventKey),
          )
        : null;
    transcriptByAgent.set(
      key,
      evicted ? dropTranscriptItems(appended, evicted) : appended,
    );
  } else {
    // Slow path: out-of-order insertion. Re-derive the whole window, then seed
    // the eviction index conservatively off the newest surviving event so the
    // rebuild itself stays O(events) — see seedTranscriptWindowIndex.
    transcriptRebuildCount += 1;
    const rebuilt = buildTranscriptStateWithGaps(final);
    seedTranscriptWindowIndex(
      index,
      rebuilt,
      observerEventKey(final[final.length - 1]),
    );
    transcriptByAgent.set(key, rebuilt);
  }

  invalidateSnapshot(key);

  notifyListeners(key);
}

/**
 * Compose the map key for the channel-scoped archive transcript.
 * Separates agent identity from channel with `:` — the same delimiter used by
 * liveSessionKey so all composite keys in this module are consistently shaped.
 */
function archiveChannelKey(agentPubkey: string, channelId: string): string {
  return `${normalizePubkey(agentPubkey)}:${channelId}`;
}

/**
 * Read the channel-scoped archive raw events for a given (agent, channel)
 * pair. Returns an empty array when no archive has been loaded yet.
 *
 * Called by `useArchivedChannelEvents` so UI components can reactively
 * subscribe to archive loads and derive transcript state from the combined
 * live + archive raw event window without touching the live-capped per-agent
 * store.
 */
export function getArchivedChannelEvents(
  agentPubkey: string | null | undefined,
  channelId: string | null | undefined,
): ObserverEvent[] {
  if (!agentPubkey || !channelId) return EMPTY_EVENTS;
  return readArchivedChannelEvents(archiveChannelKey(agentPubkey, channelId));
}

export { compareObserverEvents, isObserverEventAfter };

async function handleRelayObserverEvent(
  event: RelayEvent,
  activeGeneration: number,
) {
  const agentPubkey = observerTag(event, "agent");
  const frame = observerTag(event, "frame");
  if (!agentPubkey || frame !== "telemetry") {
    return;
  }

  // Ownership data arrives asynchronously during startup. Buffer raw signed
  // frames until the first trusted-agent set is registered, then re-run this
  // same gate. Once initialized, unknown agents are rejected immediately.
  if (!knownAgentPubkeys.has(normalizePubkey(agentPubkey))) {
    if (knownAgentsBySubscription.size === 0 || knownAgentPubkeys.size === 0) {
      pendingUnknownAgentFrames.push(event);
      if (pendingUnknownAgentFrames.length > MAX_PENDING_UNKNOWN_AGENT_FRAMES) {
        pendingUnknownAgentFrames.shift();
      }
    }
    return;
  }

  // Defense-in-depth: verify the event sender matches the claimed agent pubkey.
  // The relay gates on is_agent_owner, but a compromised relay could misroute.
  if (normalizePubkey(event.pubkey) !== normalizePubkey(agentPubkey)) {
    return;
  }

  try {
    const parsed = (await decryptObserverEvent(event)) as ObserverEvent;
    if (activeGeneration !== generation) {
      return;
    }
    // Track the latest-live-session-id per (agent, channel) on the live path.
    // Only set when the parsed event carries both a sessionId and channelId,
    // so we never attribute a session to the wrong channel.
    if (parsed.sessionId && parsed.channelId) {
      const key = liveSessionKey(agentPubkey, parsed.channelId);
      const stored = latestLiveSessionByAgentChannel.get(key);
      // Advance only when this event sorts strictly AFTER the stored one via
      // isObserverEventAfter (timestamp then seq — same ordering as
      // compareObserverEvents). This prevents late-arriving live frames from
      // older sessions from regressing the latest-live id, while also
      // correctly advancing on a same-timestamp frame with a higher seq.
      if (!stored || isObserverEventAfter(parsed, stored)) {
        latestLiveSessionByAgentChannel.set(key, {
          sessionId: parsed.sessionId,
          timestamp: parsed.timestamp,
          seq: parsed.seq,
        });
      }
    }
    trackObserverContinuity(agentPubkey, parsed);
    appendAgentEvent(agentPubkey, parsed);
    const managementRequest = parseAgentManagementRequest(parsed.payload);
    if (managementRequest) {
      for (const listener of agentManagementListeners) {
        listener(agentPubkey, managementRequest);
      }
    }
    if (parsed.kind === "session_config_captured") {
      void putAgentSessionConfig(agentPubkey, parsed.payload);
      onSessionConfigCaptured?.(agentPubkey);
    } else if (parsed.kind === "control_result") {
      dispatchControlResult(agentPubkey, parsed.payload);
    } else if (parsed.kind === "managed_agent_runtime_lifecycle") {
      void putManagedAgentRuntimeLifecycle(agentPubkey, parsed.payload).catch(
        (error) => {
          console.debug("Late/untracked lifecycle frame dropped:", error);
        },
      );
    }
  } catch (error) {
    if (activeGeneration !== generation) {
      return;
    }
    setConnectionState(
      "error",
      error instanceof Error
        ? `Observer event decrypt failed: ${error.message}`
        : "Observer event decrypt failed.",
    );
  }
}

export function ensureRelayObserverSubscription() {
  if (unsubscribeRelay) {
    return Promise.resolve();
  }
  if (startPromise) {
    return startPromise;
  }

  const activeGeneration = generation;
  setConnectionState("connecting", null);
  startPromise = (async () => {
    const identity = await getIdentity();
    const unsubscribe = await subscribeToAgentObserverFrames(
      identity.pubkey,
      (event) => {
        eventProcessingQueue = eventProcessingQueue
          .then(() => handleRelayObserverEvent(event, activeGeneration))
          .catch((error) => {
            if (activeGeneration !== generation) {
              return;
            }
            setConnectionState(
              "error",
              error instanceof Error
                ? `Observer event handling failed: ${error.message}`
                : "Observer event handling failed.",
            );
          });
      },
    );
    if (activeGeneration !== generation) {
      await unsubscribe();
      return;
    }
    unsubscribeRelay = unsubscribe;
    setConnectionState("open", null);
  })()
    .catch((error) => {
      if (activeGeneration === generation) {
        setConnectionState(
          "error",
          error instanceof Error
            ? error.message
            : "Observer relay subscription failed.",
        );
      }
    })
    .finally(() => {
      if (activeGeneration === generation) {
        startPromise = null;
      }
    });

  return startPromise;
}

/**
 * Subscribe to store changes. The listener receives the normalized pubkey of
 * the one agent whose journal changed, or `null` for a store-wide change.
 * Existing zero-argument listeners are unaffected.
 */
export function subscribeAgentObserverStore(
  listener: (changedAgentKey: string | null) => void,
) {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

function isControlResultFrame(payload: unknown): payload is ControlResultFrame {
  return (
    typeof payload === "object" &&
    payload !== null &&
    typeof (payload as { type?: unknown }).type === "string" &&
    typeof (payload as { status?: unknown }).status === "string"
  );
}

function dispatchControlResult(agentPubkey: string, payload: unknown) {
  if (!isControlResultFrame(payload)) {
    return;
  }
  const subscribers = controlResultListeners.get(normalizePubkey(agentPubkey));
  if (!subscribers) {
    return;
  }
  for (const subscriber of subscribers) {
    subscriber(payload);
  }
}

/**
 * Subscribe to `control_result` frames for a single agent. Returns an
 * unsubscribe function. Used by the ModelPicker to learn the async outcome of
 * a `switch_model` frame.
 */
export function subscribeAgentManagementRequests(
  listener: (agentPubkey: string, request: AgentManagementRequest) => void,
) {
  agentManagementListeners.add(listener);
  return () => {
    agentManagementListeners.delete(listener);
  };
}

export function subscribeControlResults(
  agentPubkey: string,
  listener: (frame: ControlResultFrame) => void,
) {
  const key = normalizePubkey(agentPubkey);
  const subscribers = controlResultListeners.get(key) ?? new Set();
  subscribers.add(listener);
  controlResultListeners.set(key, subscribers);
  return () => {
    const current = controlResultListeners.get(key);
    if (!current) {
      return;
    }
    current.delete(listener);
    if (current.size === 0) {
      controlResultListeners.delete(key);
    }
  };
}

export function getAgentObserverSnapshot(
  agentPubkey?: string | null,
  // `_enabled` previously gated store reads — now only gates the relay
  // subscription in useObserverEvents. Kept for call-site compatibility.
  _enabled?: boolean,
): ObserverSnapshot {
  // `_enabled` gates the live-relay subscription in useObserverEvents, but we
  // always serve stored data when agentPubkey is present — archived frames are
  // ingested into eventsByAgent regardless of live status and must be readable
  // by idle-agent panels showing channel-scoped history.
  if (!agentPubkey) {
    return IDLE_SNAPSHOT;
  }
  const key = normalizePubkey(agentPubkey);
  const cached = snapshotByAgent.get(key);
  if (
    cached &&
    cached.connectionState === connectionState &&
    cached.errorMessage === errorMessage
  ) {
    return cached;
  }
  const snapshot: ObserverSnapshot = {
    connectionState,
    errorMessage,
    events: eventsByAgent.get(key) ?? [],
  };
  snapshotByAgent.set(key, snapshot);
  return snapshot;
}

export function getAgentTranscript(
  agentPubkey?: string | null,
  // `_enabled` previously gated store reads — now only gates the relay
  // subscription in useObserverEvents. Kept for call-site compatibility.
  _enabled?: boolean,
): TranscriptItem[] {
  // Same decoupling as getAgentObserverSnapshot: `_enabled` gates relay
  // subscription, not store reads. Archived items are in transcriptByAgent
  // and must be readable regardless of live status.
  if (!agentPubkey) {
    return EMPTY_TRANSCRIPT;
  }
  const key = normalizePubkey(agentPubkey);
  const state = transcriptByAgent.get(key);
  return state?.items ?? EMPTY_TRANSCRIPT;
}

export function shouldObserveManagedAgents(
  agents: readonly Pick<ManagedAgent, "pubkey">[],
): boolean {
  return agents.length > 0;
}

export function useManagedAgentObserverBridge(
  agents: readonly Pick<ManagedAgent, "pubkey" | "status">[],
) {
  const subscriptionId = React.useId();
  const hasManagedAgent = shouldObserveManagedAgents(agents);

  const agentPubkeys = React.useMemo(
    () => agents.map((agent) => agent.pubkey),
    [agents],
  );

  // Keep this subscriber's slice of the trusted-pubkey set in sync with its
  // own agent list. The store recomputes the union across all subscribers, so
  // a co-mounted caller no longer wipes out this caller's agents.
  React.useEffect(() => {
    registerKnownAgents(subscriptionId, agentPubkeys);
    return () => {
      unregisterKnownAgents(subscriptionId);
    };
  }, [subscriptionId, agentPubkeys]);

  React.useEffect(() => {
    if (!hasManagedAgent) {
      return;
    }
    void ensureRelayObserverSubscription();
  }, [hasManagedAgent]);

  // Wire up config-surface query invalidation when session_config_captured fires.
  const queryClient = useQueryClient();
  React.useEffect(() => {
    setSessionConfigCapturedCallback((pubkey) => {
      void queryClient.invalidateQueries({
        queryKey: agentConfigSurfaceQueryKey(pubkey),
      });
    });
    return () => setSessionConfigCapturedCallback(null);
  }, [queryClient]);
}

/**
 * Ingest a batch of raw archived observer events from the local archive into
 * the store. Applies the same security guards as the live relay path:
 *
 * - Event must have an `agent` tag pointing to a known/trusted pubkey
 *   (registered via `useManagedAgentObserverBridge`).
 * - The event sender (`pubkey`) must match the `agent` tag value.
 * - Event must decrypt successfully via `decryptObserverEvent`.
 *
 * Routes through `appendAgentEvent` so dedup on `(seq, timestamp)` and
 * sort are reused — archived events that are already present (live-delivered)
 * are silently skipped. Failed decryptions are silently dropped (same as
 * live path error handling).
 *
 * Note: events for agents not currently registered in `knownAgentPubkeys`
 * (e.g. an agent that is stopped but has archived history) are dropped.
 * The caller should ensure the agent is registered before calling.
 *
 * `_decryptFn` is only used by tests to inject a mock decryption function.
 * Production callers must always omit it.
 */
export async function ingestArchivedObserverEvents(
  rawEvents: RelayEvent[],
  _decryptFn: (event: RelayEvent) => Promise<unknown> = decryptObserverEvent,
): Promise<void> {
  let archiveChanged = false;
  for (const event of rawEvents) {
    const agentPubkey = observerTag(event, "agent");
    const frame = observerTag(event, "frame");
    if (!agentPubkey || frame !== "telemetry") {
      continue;
    }
    if (!knownAgentPubkeys.has(normalizePubkey(agentPubkey))) {
      continue;
    }
    if (normalizePubkey(event.pubkey) !== normalizePubkey(agentPubkey)) {
      continue;
    }
    try {
      const parsed = (await _decryptFn(event)) as ObserverEvent;
      // Route archived events to the channel-scoped archive window (no cap)
      // rather than the per-agent live-relay store (MAX_OBSERVER_EVENTS cap).
      // Events without a channelId fall through to the live store so they
      // remain visible in the agent's general transcript.
      if (parsed.channelId) {
        const added = appendArchivedChannelEvent(
          archiveChannelKey(agentPubkey, parsed.channelId),
          parsed.channelId,
          parsed,
        );
        if (added) archiveChanged = true;
      } else {
        // Live path already calls notifyListeners() inside appendAgentEvent.
        appendAgentEvent(agentPubkey, parsed);
      }
    } catch {
      // Silently drop decrypt failures — same as live path error handling.
    }
  }
  // Batch-notify once for the whole page of archive events. appendAgentEvent
  // already notifies individually for live/no-channelId events above, so we
  // only need one extra notify here for the archive path.
  if (archiveChanged) {
    notifyListeners();
  }
}

/**
 * E2E-only: inject synthetic observer events directly into the store, bypassing
 * the relay-security knownAgentPubkeys filter. Exercises the real
 * appendAgentEvent → processTranscriptEvent ingestion path so screenshot specs
 * prove the production render, not a stub.
 *
 * Never call this from production code — it is intentionally not re-exported
 * from the public agent feature barrel.
 */
export function injectObserverEventsForE2E(
  agentPubkey: string,
  events: ObserverEvent[],
) {
  for (const event of events) {
    appendAgentEvent(agentPubkey, event);
  }
  notifyListeners();
}

/**
 * Synchronize the observer store with a sorted buffer of events for one agent.
 * Used by test harnesses and replay bridges that already hold decoded frames.
 */
export function syncAgentObserverEvents(
  agentPubkey: string,
  events: ObserverEvent[],
) {
  for (const event of events) {
    appendAgentEvent(agentPubkey, event);
  }
}

export function resetAgentObserverStore() {
  generation += 1;
  const unsubscribe = unsubscribeRelay;
  unsubscribeRelay = null;
  startPromise = null;
  eventProcessingQueue = Promise.resolve();
  eventsByAgent.clear();
  transcriptByAgent.clear();
  transcriptWindowByAgent.clear();
  snapshotByAgent.clear();
  clearArchivedChannelEvents();
  knownAgentPubkeys.clear();
  knownAgentsBySubscription.clear();
  pendingUnknownAgentFrames.length = 0;
  latestLiveSessionByAgentChannel.clear();
  lastSeqByAgent.clear();
  gapsByAgent.clear();
  unrecoverableGapAgents.clear();
  agentManagementListeners.clear();
  transcriptRebuildCount = 0;
  onSessionConfigCaptured = null;
  connectionState = "idle";
  errorMessage = null;
  notifyListeners();
  void unsubscribe?.();
}

/**
 * Test-only: register a set of agent pubkeys as trusted for a given
 * subscription id. Mirrors the effect of mounting `useManagedAgentObserverBridge`
 * in a React tree. Only call from tests — never from production code.
 */
export function _testRegisterKnownAgents(
  subscriptionId: string,
  pubkeys: readonly string[],
): void {
  registerKnownAgents(subscriptionId, pubkeys);
}

/**
 * Test-only: read the raw archived observer events for a (agent, channel) pair.
 * Production callers should use `getArchivedChannelEvents`.
 * Only call from tests — never from production code.
 */
export function _testGetArchivedChannelEvents(
  agentPubkey: string,
  channelId: string,
): ObserverEvent[] {
  return readArchivedChannelEvents(archiveChannelKey(agentPubkey, channelId));
}
