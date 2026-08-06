import type {
  RelaySubscription,
  RelaySubscriptionFilter,
} from "@/shared/api/relayClientShared";
import { clearClosedRetry } from "@/shared/api/relayClosedRecovery";
import { splitEdgeMessageFilter } from "@/shared/api/relayEdgeRouting";
import type { RelayEdgeClient } from "@/shared/api/relayEdgeSession";
import type { RelayEvent } from "@/shared/api/types";

/** Wait this long for the relay's first frame before reporting ready. */
const SUBSCRIPTION_READY_FALLBACK_MS = 250;

/**
 * The canonical client's internals this module needs. Passing them explicitly
 * keeps the live-subscription lifecycle testable and keeps `RelayClient` from
 * growing a second copy of it for the edge lane.
 */
export type LiveSubscriptionPort = {
  ensureConnected: () => Promise<void>;
  subscriptions: Map<string, RelaySubscription>;
  sendReq: (subId: string, filter: RelaySubscriptionFilter) => Promise<void>;
  closeSubscription: (subId: string) => Promise<void>;
};

/**
 * Open one live subscription on the canonical relay.
 *
 * Extracted verbatim from `RelayClient.subscribe` so the edge split below has
 * a single canonical path to fall back to.
 */
export async function subscribeCanonical(
  port: LiveSubscriptionPort,
  filter: RelaySubscriptionFilter,
  onEvent: (event: RelayEvent) => void,
): Promise<() => Promise<void>> {
  await port.ensureConnected();

  const subId = `live-${crypto.randomUUID()}`;
  let resolveReady = () => {
    return;
  };
  const ready = new Promise<void>((resolve) => {
    resolveReady = () => {
      window.clearTimeout(fallbackTimeout);
      resolve();
    };
  });
  const fallbackTimeout = window.setTimeout(
    () => resolveReady(),
    SUBSCRIPTION_READY_FALLBACK_MS,
  );

  port.subscriptions.set(subId, {
    mode: "live",
    filter,
    onEvent,
    resolveReady,
  });

  try {
    await port.sendReq(subId, filter);
  } catch (error) {
    window.clearTimeout(fallbackTimeout);
    port.subscriptions.delete(subId);
    throw error;
  }
  await ready;

  return async () => {
    const active = port.subscriptions.get(subId);
    if (active?.mode !== "live") return;
    port.subscriptions.delete(subId);
    clearClosedRetry(active);
    await port.closeSubscription(subId);
  };
}

/**
 * Open a live subscription, routing its kind-9 half through the loopback
 * sidecar when one is bound and the filter is edge-eligible
 * (SPEC-2026-08-05 §15).
 *
 * The two halves are disjoint by construction: `splitEdgeMessageFilter` gives
 * kind 9 to the edge and every other kind to canonical, so no event can arrive
 * twice and the caller needs no de-duplication.
 *
 * If the edge half cannot be opened, the ORIGINAL unsplit filter goes to
 * canonical. A half-subscribed channel would silently lose messages, which is
 * worse than not using the sidecar at all.
 */
export async function subscribeWithEdgeSplit(
  port: LiveSubscriptionPort,
  edge: RelayEdgeClient,
  filter: RelaySubscriptionFilter,
  onEvent: (event: RelayEvent) => void,
): Promise<() => Promise<void>> {
  const split = edge.currentBinding() ? splitEdgeMessageFilter(filter) : null;
  const edgeUnsubscribe = split
    ? await edge.subscribe(split.edge, onEvent)
    : null;
  if (!split || !edgeUnsubscribe) {
    return subscribeCanonical(port, filter, onEvent);
  }

  let canonicalUnsubscribe: (() => Promise<void>) | null = null;
  if (split.canonical) {
    try {
      canonicalUnsubscribe = await subscribeCanonical(
        port,
        split.canonical,
        onEvent,
      );
    } catch (error) {
      // Never leave the edge half running alone: the caller asked for both.
      await edgeUnsubscribe();
      throw error;
    }
  }

  return async () => {
    await edgeUnsubscribe();
    await canonicalUnsubscribe?.();
  };
}
