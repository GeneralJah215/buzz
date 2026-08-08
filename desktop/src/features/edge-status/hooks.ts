/**
 * Edge-status polling hook.
 *
 * Deliberately NOT a TanStack Query hook: the edge sidecar is optional and off
 * by default, so the common case is "every command rejects, forever". A query
 * with `refetchInterval` would keep firing IPC at a process that does not
 * exist for the whole life of the app.
 *
 * What this hook does instead is *back off*, not give up. An absent sidecar
 * drops the cadence from seconds to minutes; it does not tear the surface down
 * for the session. A restarted sidecar is the common case, and permanent
 * silence after one blip would mean the operator sees "nothing is stuck"
 * forever while events pile up.
 *
 * Four behaviours the tests pin:
 *   - the interval is cleared on unmount, and no IPC is issued after it
 *   - an absent sidecar drops to the slow retry cadence and recovers on its own
 *   - a genuine fault (503, binding mismatch, SQLite error) surfaces as an
 *     error and keeps the fast cadence
 *   - no poll fires while the window is hidden; becoming visible polls once
 */

import * as React from "react";

import {
  fetchEdgeDeliverySummary,
  fetchEdgeWaitingAuthors,
  isEdgeUnavailableError,
  type EdgeDeliverySummary,
  type EdgeWaitingAuthor,
} from "@/features/edge-status/api/edgeStatus";

/**
 * Modest cadence. The counters move on human timescales (a drain runs after a
 * reconnect), and every tick is two IPC round-trips into a SQLite store.
 */
export const EDGE_STATUS_POLL_INTERVAL_MS = 15_000;

/**
 * Cadence once the sidecar is known absent. Two minutes is cheap enough to run
 * all day on a machine that never installed the feature (30 IPC pairs an hour)
 * and quick enough that a sidecar restart is picked up without the operator
 * doing anything.
 */
export const EDGE_UNAVAILABLE_RETRY_INTERVAL_MS = 120_000;

export type EdgeStatus = {
  /** `null` until the first successful poll, and whenever unavailable. */
  summary: EdgeDeliverySummary | null;
  waitingAuthors: EdgeWaitingAuthor[];
  /** The sidecar is not running. Render nothing, not an error. */
  unavailable: boolean;
  /** A genuine fault — malformed response or an unexpected rejection. */
  error: Error | null;
  isLoading: boolean;
  /** Fetch immediately and return to the fast cadence. */
  refresh: () => void;
};

const NO_WAITING_AUTHORS: EdgeWaitingAuthor[] = [];

function isDocumentHidden(): boolean {
  return (
    typeof document !== "undefined" && document.visibilityState === "hidden"
  );
}

export function useEdgeStatus(options?: {
  enabled?: boolean;
  intervalMs?: number;
  unavailableRetryMs?: number;
}): EdgeStatus {
  const enabled = options?.enabled ?? true;
  const intervalMs = options?.intervalMs ?? EDGE_STATUS_POLL_INTERVAL_MS;
  const unavailableRetryMs =
    options?.unavailableRetryMs ?? EDGE_UNAVAILABLE_RETRY_INTERVAL_MS;

  const [summary, setSummary] = React.useState<EdgeDeliverySummary | null>(
    null,
  );
  const [waitingAuthors, setWaitingAuthors] =
    React.useState<EdgeWaitingAuthor[]>(NO_WAITING_AUTHORS);
  // `unavailable` is a dependency of the polling effect, so flipping it tears
  // the fast interval down and re-arms a slow one rather than leaving a timer
  // that hammers a process which is not there.
  const [unavailable, setUnavailable] = React.useState(false);
  const [error, setError] = React.useState<Error | null>(null);
  const [isLoading, setIsLoading] = React.useState(false);
  const [refreshToken, setRefreshToken] = React.useState(0);

  const mountedRef = React.useRef(true);
  const inFlightRef = React.useRef(false);
  // Set when a poll was asked for while another was in flight and must not be
  // dropped (an explicit refresh()). Interval ticks never set it: coalescing a
  // periodic tick into the run already happening is the correct behaviour.
  const queuedRef = React.useRef(false);
  const refreshRequestedRef = React.useRef(false);
  const previousUnavailableRef = React.useRef<boolean | null>(null);

  React.useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
    };
  }, []);

  const load = React.useEffectEvent(async (queueIfBusy: boolean) => {
    // No IPC from a dead component: a timer callback that outlived teardown
    // must not reach the sidecar at all, let alone write state back.
    if (!mountedRef.current) {
      return;
    }
    if (inFlightRef.current) {
      queuedRef.current = queuedRef.current || queueIfBusy;
      return;
    }

    async function attempt() {
      try {
        const [nextSummary, nextWaitingAuthors] = await Promise.all([
          fetchEdgeDeliverySummary(),
          fetchEdgeWaitingAuthors(),
        ]);
        if (!mountedRef.current) {
          return;
        }
        setSummary(nextSummary);
        setWaitingAuthors(nextWaitingAuthors);
        setUnavailable(false);
        setError(null);
      } catch (caught) {
        if (!mountedRef.current) {
          return;
        }
        if (isEdgeUnavailableError(caught)) {
          // Expected on every machine that never installed the sidecar. Drop
          // any stale numbers, surface no error, and slow the timer down.
          setSummary(null);
          setWaitingAuthors(NO_WAITING_AUTHORS);
          setError(null);
          setUnavailable(true);
          return;
        }
        setError(caught instanceof Error ? caught : new Error(String(caught)));
      }
    }

    inFlightRef.current = true;
    setIsLoading(true);
    try {
      do {
        queuedRef.current = false;
        await attempt();
      } while (queuedRef.current && mountedRef.current);
    } finally {
      queuedRef.current = false;
      inFlightRef.current = false;
      if (mountedRef.current) {
        setIsLoading(false);
      }
    }
  });

  React.useEffect(() => {
    // `refreshToken` is a deliberate re-run trigger, not a value this effect
    // reads: bumping it is how refresh() polls now and restarts the cadence.
    void refreshToken;

    const wasRefresh = refreshRequestedRef.current;
    refreshRequestedRef.current = false;
    // Did this run exist only because the cadence changed? If so the poll that
    // changed it has just finished, and polling again on arrival would double
    // every availability transition.
    const cadenceOnly =
      previousUnavailableRef.current !== null &&
      previousUnavailableRef.current !== unavailable;
    previousUnavailableRef.current = unavailable;

    if (!enabled) {
      return;
    }

    const cadenceMs = unavailable ? unavailableRetryMs : intervalMs;

    if (!isDocumentHidden() && (wasRefresh || !cadenceOnly)) {
      void load(wasRefresh);
    }

    const intervalId = window.setInterval(() => {
      if (isDocumentHidden()) {
        return;
      }
      void load(false);
    }, cadenceMs);

    const hasDocument = typeof document !== "undefined";
    function handleVisibilityChange() {
      if (isDocumentHidden()) {
        return;
      }
      // While unavailable the slow timer owns recovery; polling on every
      // window focus would undo the backoff on a machine with no sidecar.
      if (unavailable) {
        return;
      }
      void load(false);
    }
    if (hasDocument) {
      document.addEventListener("visibilitychange", handleVisibilityChange);
    }

    return () => {
      window.clearInterval(intervalId);
      if (hasDocument) {
        document.removeEventListener(
          "visibilitychange",
          handleVisibilityChange,
        );
      }
    };
  }, [enabled, intervalMs, unavailableRetryMs, unavailable, refreshToken]);

  const refresh = React.useCallback(() => {
    // Clearing `unavailable` returns the effect to the fast cadence; the ref
    // tells it this run was asked for, so it polls immediately and is queued
    // rather than dropped if a poll happens to be in flight.
    refreshRequestedRef.current = true;
    setUnavailable(false);
    setRefreshToken((token) => token + 1);
  }, []);

  return {
    summary,
    waitingAuthors,
    unavailable,
    error,
    isLoading,
    refresh,
  };
}
