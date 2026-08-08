/**
 * One batched delivery-state lookup for every message row on screen.
 *
 * Rows register the event ids they care about; the provider coalesces those
 * registrations and issues ONE `edge_event_delivery_states` call for the whole
 * rendered window. A per-row fetch would put 200 IPC round-trips into a SQLite
 * store behind a single channel switch, and the sidecar refuses batches over
 * `MAX_DELIVERY_STATE_BATCH` (500) with a 400, so the request is chunked rather
 * than trimmed — dropping ids would silently blank badges on the oldest rows.
 *
 * Registration-driven rather than list-driven on purpose. `MessageRow` renders
 * from two independent trees (the channel timeline and the thread panel) and
 * the timeline is virtualized, so "the ids currently on screen" is a fact only
 * the rows themselves hold. It also keeps the wiring out of `ChannelPane` and
 * `ChannelScreen`, which are one and two lines under the repo's file-size
 * ratchet and cannot take new plumbing.
 *
 * The whole edge feature is off by default: with `BUZZ_EDGE_RELAY_URL` unset
 * every command rejects with `EDGE_UNAVAILABLE_MESSAGE`. That is not an error
 * and must not be one on screen — the provider records it, backs the cadence
 * off to `EDGE_UNAVAILABLE_RETRY_INTERVAL_MS`, and publishes an empty lookup so
 * every consumer renders nothing at all. It backs off rather than stopping so a
 * sidecar that starts later is still picked up without an app restart.
 */

import * as React from "react";

import {
  fetchEdgeEventDeliveryStates,
  isEdgeUnavailableError,
  type EdgeDeliveryStateEntry,
  type EdgeDeliveryStateLookup,
} from "@/features/edge-status/api/edgeStatus";
import {
  EDGE_STATUS_POLL_INTERVAL_MS,
  EDGE_UNAVAILABLE_RETRY_INTERVAL_MS,
} from "@/features/edge-status/hooks";

/**
 * `MAX_DELIVERY_STATE_BATCH` in `crates/buzz-edge/src/lib.rs`. Over this the
 * sidecar answers `400 too many event_ids` and the whole batch is lost, so the
 * ceiling is enforced here by splitting rather than by truncating.
 */
export const MAX_DELIVERY_STATE_IDS = 500;

/**
 * How long registrations are pooled before a request goes out.
 *
 * Every row on a freshly mounted timeline registers in the same commit, so any
 * positive delay collapses them into one call. The value is what keeps *scroll*
 * cheap: a virtualized list mounts and unmounts rows continuously, and without
 * a pause each row entering the viewport would re-key the batch.
 */
export const EDGE_DELIVERY_REGISTRATION_DEBOUNCE_MS = 250;

/**
 * Minimum gap between two `[GUARDRAIL]` fault logs from this provider.
 *
 * The badge deliberately has no error UI — it is an aside on someone else's
 * screen and must never replace a message with a banner. That makes a log the
 * ONLY trace a repeated `edge_event_delivery_states` fault leaves anywhere, so
 * it has to exist; and because the poll runs on a timer, it has to be throttled
 * or it becomes a console the operator learns to ignore.
 */
const EDGE_DELIVERY_FAULT_LOG_INTERVAL_MS = 60_000;

/** Shared frozen empty lookup so "no data" never re-renders a consumer. */
const EMPTY_LOOKUP: EdgeDeliveryStateLookup = Object.freeze({});
const EMPTY_IDS: readonly string[] = Object.freeze([]);

type EdgeDeliveryStateContextValue = {
  lookup: EdgeDeliveryStateLookup;
  /** Register interest in an id. Returns the matching unregister. */
  register: (eventId: string) => () => void;
};

/**
 * Default value for trees with no provider (unit tests of a single row, the
 * onboarding shell). Registering is a no-op and the lookup is empty, so a row
 * outside the provider renders exactly what a row on a machine with no sidecar
 * renders: nothing.
 */
const NOOP_UNREGISTER = () => {};
const EdgeDeliveryStateContext =
  React.createContext<EdgeDeliveryStateContextValue>({
    lookup: EMPTY_LOOKUP,
    register: () => NOOP_UNREGISTER,
  });

function chunkIds(ids: readonly string[]): string[][] {
  const chunks: string[][] = [];
  for (let index = 0; index < ids.length; index += MAX_DELIVERY_STATE_IDS) {
    chunks.push(ids.slice(index, index + MAX_DELIVERY_STATE_IDS));
  }
  return chunks;
}

function sameIds(a: readonly string[], b: readonly string[]): boolean {
  if (a.length !== b.length) {
    return false;
  }
  return a.every((value, index) => value === b[index]);
}

function isDocumentHidden(): boolean {
  return (
    typeof document !== "undefined" && document.visibilityState === "hidden"
  );
}

export function EdgeDeliveryStateProvider({
  children,
  debounceMs = EDGE_DELIVERY_REGISTRATION_DEBOUNCE_MS,
  intervalMs = EDGE_STATUS_POLL_INTERVAL_MS,
  unavailableRetryMs = EDGE_UNAVAILABLE_RETRY_INTERVAL_MS,
}: {
  children: React.ReactNode;
  debounceMs?: number;
  intervalMs?: number;
  unavailableRetryMs?: number;
}) {
  // Reference counted: the thread panel and the channel timeline can both have
  // the same event mounted, and the first unmount must not deregister the id
  // out from under the row that is still showing it.
  const refCountsRef = React.useRef<Map<string, number>>(new Map());
  const flushHandleRef = React.useRef<number | null>(null);
  const mountedRef = React.useRef(true);

  const [eventIds, setEventIds] = React.useState<readonly string[]>(EMPTY_IDS);
  const [lookup, setLookup] =
    React.useState<EdgeDeliveryStateLookup>(EMPTY_LOOKUP);
  const [unavailable, setUnavailable] = React.useState(false);
  // `unavailable` is a dependency of the fetch effect so that flipping it tears
  // the fast interval down and re-arms a slow one. These two remember why the
  // effect re-ran: without them the flag flip *itself* fires a second request,
  // and the machine with no sidecar -- every machine today -- would pay two
  // rejected IPC calls per timeline instead of one.
  const previousIdsRef = React.useRef<readonly string[] | null>(null);
  const previousUnavailableRef = React.useRef(false);
  // When the last "sidecar not running" rejection came back. The backoff has to
  // hold across id-set changes, not only across the flag flip: a virtualized
  // scroll re-keys the batch every debounce window, and each re-key used to
  // punch straight through the 120s cadence with another rejected IPC call.
  const lastUnavailableAtRef = React.useRef<number | null>(null);
  const lastFaultLogAtRef = React.useRef(0);

  React.useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
      if (flushHandleRef.current !== null) {
        window.clearTimeout(flushHandleRef.current);
        flushHandleRef.current = null;
      }
    };
  }, []);

  const scheduleFlush = React.useCallback(() => {
    // Row cleanups run AFTER the provider's own cleanup, so an unmounting tree
    // hands `unregister` -> `scheduleFlush` a provider that is already gone.
    // Without this the last teardown leaves one live `setTimeout` behind whose
    // callback can only ever be a no-op.
    if (!mountedRef.current) {
      return;
    }
    if (flushHandleRef.current !== null) {
      return;
    }
    flushHandleRef.current = window.setTimeout(() => {
      flushHandleRef.current = null;
      if (!mountedRef.current) {
        return;
      }
      const next = [...refCountsRef.current.keys()].sort();
      // Identity is the fetch effect's only trigger, so an unchanged set must
      // keep the previous array or scrolling would refetch on every row swap.
      setEventIds((current) => (sameIds(current, next) ? current : next));
    }, debounceMs);
  }, [debounceMs]);

  const register = React.useCallback(
    (eventId: string) => {
      const counts = refCountsRef.current;
      counts.set(eventId, (counts.get(eventId) ?? 0) + 1);
      scheduleFlush();

      return () => {
        const remaining = (counts.get(eventId) ?? 0) - 1;
        if (remaining > 0) {
          counts.set(eventId, remaining);
        } else {
          counts.delete(eventId);
        }
        scheduleFlush();
      };
    },
    [scheduleFlush],
  );

  React.useEffect(() => {
    if (eventIds.length === 0) {
      // Nothing on screen is ours. Publishing the shared empty object rather
      // than `{}` keeps consumers from re-rendering on every empty timeline.
      setLookup((current) =>
        current === EMPTY_LOOKUP ? current : EMPTY_LOOKUP,
      );
      return;
    }

    let cancelled = false;

    async function poll() {
      if (cancelled || !mountedRef.current) {
        return;
      }
      // `allSettled`, never `all`. A 600-id timeline is two chunks, and under
      // `all` one 503 on the second chunk threw away 500 perfectly good entries
      // from the first -- so a single flaky chunk blanked every badge on screen.
      // That is the dangerous direction: a genuinely stuck message showing
      // nothing reads exactly like a message that arrived.
      const results = await Promise.allSettled(
        chunkIds(eventIds).map((chunk) => fetchEdgeEventDeliveryStates(chunk)),
      );
      if (cancelled || !mountedRef.current) {
        return;
      }

      const merged: Record<string, EdgeDeliveryStateEntry> = {};
      const rejections: unknown[] = [];
      for (const result of results) {
        if (result.status === "fulfilled") {
          Object.assign(merged, result.value);
        } else {
          rejections.push(result.reason);
        }
      }

      if (rejections.length === 0) {
        setLookup(merged as EdgeDeliveryStateLookup);
        setUnavailable(false);
        return;
      }

      if (rejections.every((reason) => isEdgeUnavailableError(reason))) {
        // The ordinary case on every machine that never installed the sidecar.
        // Drop any stale badges and slow the timer right down. Partial results
        // are dropped too: if the sidecar went away mid-batch, half a lookup is
        // a half-badged timeline, and "renders nothing at all" is the contract.
        lastUnavailableAtRef.current = Date.now();
        setLookup((current) =>
          current === EMPTY_LOOKUP ? current : EMPTY_LOOKUP,
        );
        setUnavailable(true);
        return;
      }

      // A genuine fault on at least one chunk (503, binding mismatch, SQLite, a
      // shape break). The badge is an aside on someone else's screen, so it must
      // not take the timeline down or replace a message with an error -- but the
      // chunks that DID answer are still published, so one bad chunk costs only
      // its own badges. Keep the fast cadence so recovery is picked up promptly.
      const now = Date.now();
      if (
        now - lastFaultLogAtRef.current >=
        EDGE_DELIVERY_FAULT_LOG_INTERVAL_MS
      ) {
        lastFaultLogAtRef.current = now;
        console.warn(
          `[GUARDRAIL] edge_event_delivery_states failed for ${rejections.length} of ${results.length} chunk(s); badges for those ids are missing`,
          rejections[0],
        );
      }
      setLookup(merged as EdgeDeliveryStateLookup);
      setUnavailable(false);
    }

    const idsChanged = previousIdsRef.current !== eventIds;
    previousIdsRef.current = eventIds;
    const cadenceOnly =
      !idsChanged && previousUnavailableRef.current !== unavailable;
    previousUnavailableRef.current = unavailable;

    // The backoff has to survive id-set churn, not just the flag flip. Scrolling
    // a virtualized timeline re-keys the batch every debounce window, and each
    // re-key firing an immediate poll meant a sidecar-free machine paid roughly
    // four rejected IPC calls per second of scrolling while the cadence timer
    // sat armed at two minutes. Once a rejection has aged past the retry
    // interval a new id set may poll again immediately, so a sidecar that starts
    // later is still picked up promptly and continuous scrolling cannot starve
    // recovery by re-arming the interval.
    const sinceUnavailable =
      lastUnavailableAtRef.current === null
        ? Number.POSITIVE_INFINITY
        : Date.now() - lastUnavailableAtRef.current;
    const backoffHolds = unavailable && sinceUnavailable < unavailableRetryMs;

    if (!cadenceOnly && !backoffHolds) {
      void poll();
    }

    const cadenceMs = unavailable ? unavailableRetryMs : intervalMs;
    const intervalId = window.setInterval(() => {
      if (isDocumentHidden()) {
        return;
      }
      void poll();
    }, cadenceMs);

    return () => {
      cancelled = true;
      window.clearInterval(intervalId);
    };
  }, [eventIds, intervalMs, unavailable, unavailableRetryMs]);

  const value = React.useMemo(() => ({ lookup, register }), [lookup, register]);

  return (
    <EdgeDeliveryStateContext.Provider value={value}>
      {children}
    </EdgeDeliveryStateContext.Provider>
  );
}

/**
 * The delivery state of one event, or `null` when there isn't one.
 *
 * `null` covers three different situations that all mean "say nothing": the
 * sidecar is not running, the event never went through the local outbox (it
 * came from upstream), or this build does not recognise the state the sidecar
 * reported. None of them is something to draw.
 *
 * @param eventId the nostr event id, or `null` to register nothing at all
 */
export function useEdgeDeliveryState(
  eventId: string | null,
): EdgeDeliveryStateEntry | null {
  const { lookup, register } = React.useContext(EdgeDeliveryStateContext);

  React.useEffect(() => {
    if (eventId === null || eventId.length === 0) {
      return;
    }
    return register(eventId);
  }, [eventId, register]);

  if (eventId === null || eventId.length === 0) {
    return null;
  }
  return Object.hasOwn(lookup, eventId) ? lookup[eventId] : null;
}
