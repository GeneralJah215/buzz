import * as React from "react";
import { useManagedAgentsQuery } from "@/features/agents/hooks";
import {
  getAgentObserverSnapshot,
  subscribeAgentObserverStore,
} from "@/features/agents/observerRelayStore";
import { createPreventSleepActivityTracker } from "@/features/agents/preventSleepActivity";
import { setPreventSleepActive } from "@/shared/api/tauri";
import { normalizePubkey } from "@/shared/lib/pubkey";
import { listen } from "@tauri-apps/api/event";

// Intentionally not scoped per-pubkey — multi-user desktop is rare and the
// setting applies to the machine's sleep behavior regardless of account.
const STORAGE_KEY = "buzz-prevent-sleep";

/**
 * Minimum gap between two activity-driven `setPreventSleepActive(true)` calls.
 *
 * BUG-067. That call is a Tauri IPC, and it fired whenever the newest observer
 * event key changed — which for a busy agent is every frame. All the Rust side
 * does with a repeat call is re-arm the inactivity cap timer, and that cap is
 * one hour (`INACTIVITY_CAP_SECONDS`), so refreshing it at most every 30s is
 * 120x more often than the cap needs and cannot let the assertion lapse while
 * an agent is genuinely working.
 *
 * The `expired` recovery path deliberately bypasses this throttle: coming back
 * from an expired assertion must re-acquire immediately, not up to 30s late.
 */
const ACTIVITY_REFRESH_INTERVAL_MS = 30_000;

function readPreference(): boolean {
  return window.localStorage.getItem(STORAGE_KEY) === "true";
}

function writePreference(enabled: boolean) {
  window.localStorage.setItem(STORAGE_KEY, String(enabled));
}

interface PreventSleepValue {
  enabled: boolean;
  setEnabled: (value: boolean) => void;
  active: boolean;
  hasRunningAgents: boolean;
  expired: boolean;
  clearExpired: () => void;
}

const PreventSleepContext = React.createContext<PreventSleepValue | null>(null);

export function PreventSleepProvider({
  children,
}: {
  children: React.ReactNode;
}) {
  const value = usePreventSleepInternal();
  return React.createElement(PreventSleepContext.Provider, { value }, children);
}

export function usePreventSleepContext(): PreventSleepValue {
  const ctx = React.useContext(PreventSleepContext);
  if (!ctx) {
    throw new Error(
      "usePreventSleepContext must be used within a PreventSleepProvider",
    );
  }
  return ctx;
}

function usePreventSleepInternal() {
  const [enabled, setEnabledState] = React.useState(readPreference);
  const { data: agents } = useManagedAgentsQuery();

  // Only local "running" agents need sleep prevention. Remote "deployed"
  // agents run on provider infrastructure and are unaffected by local sleep.
  const runningAgentPubkeys = React.useMemo(
    () =>
      (agents ?? [])
        .filter((agent) => agent.status === "running")
        .map((agent) => normalizePubkey(agent.pubkey))
        .sort(),
    [agents],
  );

  const runningAgentPubkeyKey = runningAgentPubkeys.join(",");
  // Observer ingestion is owner-global (useAgentObserverIngestion in
  // AppShell); this hook only reads observer snapshots for activity tracking.

  const hasRunningAgents = runningAgentPubkeys.length > 0;

  const [expired, setExpired] = React.useState(false);

  const active = enabled && hasRunningAgents && !expired;

  const setEnabled = React.useCallback((value: boolean) => {
    writePreference(value);
    setEnabledState(value);
  }, []);

  React.useEffect(() => {
    void setPreventSleepActive(active);
  }, [active]);
  React.useEffect(() => {
    const unlisten = listen("prevent-sleep-expired", () => {
      setExpired(true);
    });
    return () => {
      void unlisten.then((fn) => fn());
    };
  }, []);

  // Timestamp of the last activity-driven IPC. A ref, not effect-local state,
  // so re-running the effect (an agent starts, `expired` flips) cannot reset
  // the throttle window and let a burst of IPC through.
  const lastActivityIpcAtRef = React.useRef(0);

  React.useEffect(() => {
    if (!enabled || !runningAgentPubkeyKey) return;

    const observedPubkeys = runningAgentPubkeyKey.split(",");
    // Both sides are already normalized: runningAgentPubkeys maps through
    // normalizePubkey, and the store's changedAgentKey is normalized too.
    const observedPubkeySet = new Set(observedPubkeys);
    const tracker = createPreventSleepActivityTracker();

    // BUG-067 — this consumer AGGREGATES across every running agent, so it is
    // deliberately NOT filtered down to a single agent. What the changed-agent
    // key buys here is the size of each wakeup: re-reading one agent's snapshot
    // instead of all 29. A `null` key (reset, connection state, archive page)
    // still re-reads everything, and a key naming an agent we don't track is
    // dropped because its events can never move this tracker.
    const observeActivity = (changedAgentKey: string | null = null) => {
      let observed: readonly string[];
      if (changedAgentKey === null) {
        observed = observedPubkeys;
      } else if (observedPubkeySet.has(changedAgentKey)) {
        observed = [changedAgentKey];
      } else {
        return;
      }

      const hasNewActivity = tracker.observe(
        observed.map((pubkey) => ({
          pubkey,
          events: getAgentObserverSnapshot(pubkey, true).events,
        })),
      );
      if (!hasNewActivity) return;

      if (expired) {
        setExpired(false);
        lastActivityIpcAtRef.current = Date.now();
        void setPreventSleepActive(true);
        return;
      }

      const now = Date.now();
      if (now - lastActivityIpcAtRef.current < ACTIVITY_REFRESH_INTERVAL_MS) {
        return;
      }
      lastActivityIpcAtRef.current = now;
      void setPreventSleepActive(true);
    };

    observeActivity();
    return subscribeAgentObserverStore(observeActivity);
  }, [enabled, expired, runningAgentPubkeyKey]);

  return {
    enabled,
    setEnabled,
    active,
    hasRunningAgents,
    expired,
    clearExpired: () => setExpired(false),
  };
}
