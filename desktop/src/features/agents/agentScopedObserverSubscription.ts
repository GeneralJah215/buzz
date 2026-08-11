import * as React from "react";

import { subscribeAgentObserverStore } from "@/features/agents/observerRelayStore";
import { normalizePubkey } from "@/shared/lib/pubkey";

/**
 * Observer-store subscription for a consumer that reads exactly ONE agent.
 *
 * BUG-067. `notifyListeners` carries the normalized pubkey of the single agent
 * whose journal changed (commit 94e82ece1), but every React consumer still
 * registered a zero-argument listener, so 29 streaming agents woke every
 * consumer on every frame. This narrows the wakeups for the consumers that are
 * genuinely single-agent scoped.
 *
 * ── The correctness rule this encodes ────────────────────────────────────────
 * A `null` changedAgentKey means "store-wide change" — connection state, a
 * reset, or an archive page that may span several agents. It ALWAYS wakes the
 * consumer. Only a non-null key that names a DIFFERENT agent is filtered out.
 *
 * That asymmetry is the whole safety argument: the dangerous failure mode of a
 * filtered subscriber is silence (a transcript that stops growing), and every
 * store mutation whose blast radius is wider than one agent is emitted with a
 * null key. So the filter can only ever drop a notification about an agent this
 * consumer does not read.
 *
 * Do NOT use this for a consumer that aggregates across agents — see
 * `usePreventSleep`, which narrows the WORK it does per wakeup instead.
 */
export function subscribeAgentObserverStoreForAgent(
  agentPubkey: string | null | undefined,
  onStoreChange: () => void,
): () => void {
  if (!agentPubkey) {
    // No agent to scope to: the consumer reads whatever the store-wide getters
    // return, so it must keep seeing every notification.
    return subscribeAgentObserverStore(onStoreChange);
  }
  const scopedKey = normalizePubkey(agentPubkey);
  return subscribeAgentObserverStore((changedAgentKey) => {
    if (changedAgentKey !== null && changedAgentKey !== scopedKey) {
      return;
    }
    onStoreChange();
  });
}

/**
 * `useSyncExternalStore`-ready subscribe callback scoped to one agent.
 *
 * The returned function's identity only changes when `agentPubkey` does, which
 * matters as much as the filtering: `useSyncExternalStore` tears down and
 * re-establishes the subscription every time the subscribe reference changes,
 * so an inline arrow would unsubscribe/resubscribe on every render.
 */
export function useAgentScopedObserverSubscribe(
  agentPubkey: string | null | undefined,
): (onStoreChange: () => void) => () => void {
  return React.useCallback(
    (onStoreChange: () => void) =>
      subscribeAgentObserverStoreForAgent(agentPubkey, onStoreChange),
    [agentPubkey],
  );
}
