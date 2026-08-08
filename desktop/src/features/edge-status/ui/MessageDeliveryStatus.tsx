/**
 * The timeline's slot for the canonical-history label.
 *
 * This is the placement policy, kept out of `MessageRow` so the rules live next
 * to the states they are about. Four gates, each for a different reason:
 *
 *   1. **Own messages only.** The badge answers "did MY message get out?" —
 *      the question the operator can act on. Every other identity's queue is
 *      reported in Local sync settings, where the waiting-for-author and
 *      ancestor-blocked distinctions have room to be explained; a badge on
 *      someone else's row could only ever say "not yet" with no next step.
 *   2. **Not while the row is optimistic.** A `pending` row's `id` is a local
 *      `optimistic-<uuid>` key, not a nostr id — asking the sidecar about it
 *      would be a guaranteed miss. The existing "Sending…" text owns that
 *      window, and it is a different axis: local acceptance, not upstream
 *      convergence.
 *   3. **Only when the sidecar knows the event.** No entry means either no
 *      sidecar or an event that never went through the local outbox. Both
 *      render nothing.
 *   4. **Only when it has NOT reached canonical history.** This is the "quiet"
 *      rule. On a healthy machine nearly every row is `syncedExact`, and a
 *      badge on every message is a badge the operator stops reading — so the
 *      converged states say nothing and the badge means "this one has not
 *      landed yet".
 *
 * Gate 4 is the one place this file could mislead, so: it is NOT collapsing the
 * two delivery axes. Silence here never means "sent". Every event with a state
 * at all is already delivered locally; silence means the second axis reached a
 * terminal good value, and the label that does appear names which of the
 * not-yet states the row is in, never a merged word.
 */

import { useEdgeDeliveryState } from "@/features/edge-status/EdgeDeliveryStateProvider";
import { isSyncedToCanonicalHistory } from "@/features/edge-status/lib/deliveryState";
import { DeliveryStateBadge } from "@/features/edge-status/ui/DeliveryStateBadge";

export type MessageDeliveryStatusProps = {
  /** The nostr event id of the row. */
  eventId: string;
  /** True when the row's author is the current user. */
  isOwnMessage: boolean;
  /** True while the optimistic row is still waiting for its send to ack. */
  isPending?: boolean;
  className?: string;
  /**
   * Wrap the badge in a div with these classes. Only for callers that need a
   * line of their own (grouped continuation rows). The wrapper is part of this
   * component precisely so it is never emitted empty — a caller that rendered
   * its own container around a badge that decided to say nothing would leave a
   * margin behind on every row on every machine without a sidecar.
   */
  containerClassName?: string;
};

export function MessageDeliveryStatus({
  eventId,
  isOwnMessage,
  isPending = false,
  className,
  containerClassName,
}: MessageDeliveryStatusProps) {
  const tracked = isOwnMessage && !isPending && eventId.length > 0;
  const entry = useEdgeDeliveryState(tracked ? eventId : null);

  if (entry === null || isSyncedToCanonicalHistory(entry.state)) {
    return null;
  }

  const badge = (
    <DeliveryStateBadge
      carriedByDigest={entry.carriedByDigest}
      className={className}
      data-testid="message-delivery-state"
      demotionReason={entry.demotionReason}
      state={entry.state}
    />
  );

  return containerClassName === undefined ? (
    badge
  ) : (
    <div className={containerClassName}>{badge}</div>
  );
}
