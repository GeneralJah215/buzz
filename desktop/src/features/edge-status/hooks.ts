/**
 * Edge-status polling hook.
 *
 * Deliberately NOT a TanStack Query hook: the edge sidecar is optional and off
 * by default, so the common case is "every command rejects, forever". A query
 * with `refetchInterval` would keep firing IPC at a process that does not
 * exist for the whole life of the app. This hook instead treats the first
 * "sidecar not running" rejection as terminal for the session, tears its timer
 * down, and stays quiet until something explicitly calls `refresh()`.
 *
 * Three behaviours the tests pin:
 *   - the interval is cleared on unmount (no IPC from a dead component)
 *   - the interval is cleared, not merely skipped, once the sidecar is absent
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

export type EdgeStatus = {
  /** `null` until the first successful poll, and whenever unavailable. */
  summary: EdgeDeliverySummary | null;
  waitingAuthors: EdgeWaitingAuthor[];
  /** The sidecar is not running (or not bound). Render nothing, not an error. */
  unavailable: boolean;
  /** A genuine fault — malformed response or an unexpected rejection. */
  error: Error | null;
  isLoading: boolean;
  /** Resume polling and fetch immediately, even after going quiet. */
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
}): EdgeStatus {
  const enabled = options?.enabled ?? true;
  const intervalMs = options?.intervalMs ?? EDGE_STATUS_POLL_INTERVAL_MS;

  const [summary, setSummary] = React.useState<EdgeDeliverySummary | null>(
    null,
  );
  const [waitingAuthors, setWaitingAuthors] =
    React.useState<EdgeWaitingAuthor[]>(NO_WAITING_AUTHORS);
  const [unavailable, setUnavailable] = React.useState(false);
  const [error, setError] = React.useState<Error | null>(null);
  const [isLoading, setIsLoading] = React.useState(false);
  // `paused` is a dependency of the polling effect, so flipping it true tears
  // the interval down rather than leaving a timer that no-ops forever.
  const [paused, setPaused] = React.useState(false);
  const [refreshToken, setRefreshToken] = React.useState(0);

  const mountedRef = React.useRef(true);
  const inFlightRef = React.useRef(false);

  React.useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
    };
  }, []);

  const load = React.useEffectEvent(async () => {
    if (inFlightRef.current) {
      return;
    }
    inFlightRef.current = true;
    setIsLoading(true);

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
        // any stale numbers, surface no error, and stop the timer.
        setSummary(null);
        setWaitingAuthors(NO_WAITING_AUTHORS);
        setError(null);
        setUnavailable(true);
        setPaused(true);
        return;
      }
      setError(caught instanceof Error ? caught : new Error(String(caught)));
    } finally {
      inFlightRef.current = false;
      if (mountedRef.current) {
        setIsLoading(false);
      }
    }
  });

  React.useEffect(() => {
    // `refreshToken` is a deliberate re-run trigger, not a value this effect
    // reads: bumping it is how refresh() restarts polling after going quiet.
    void refreshToken;

    if (!enabled || paused) {
      return;
    }

    // Never poll a hidden window; the visibility listener catches us up.
    if (!isDocumentHidden()) {
      void load();
    }

    const intervalId = window.setInterval(() => {
      if (isDocumentHidden()) {
        return;
      }
      void load();
    }, intervalMs);

    const hasDocument = typeof document !== "undefined";
    function handleVisibilityChange() {
      if (!isDocumentHidden()) {
        void load();
      }
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
  }, [enabled, intervalMs, paused, refreshToken]);

  const refresh = React.useCallback(() => {
    // Unpausing (or bumping the token when already live) re-runs the polling
    // effect, which loads immediately and restarts the interval.
    setPaused(false);
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
