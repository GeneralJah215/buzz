import type * as React from "react";

import type { EdgeDeliveryState } from "@/features/edge-status/api/edgeStatus";
import {
  deliveryDescription,
  deliveryLabel,
  deliveryTone,
} from "@/features/edge-status/lib/deliveryState";
import { cn } from "@/shared/lib/cn";
import { Badge } from "@/shared/ui/badge";

type DeliveryStateBadgeProps = {
  /**
   * Accepts the raw wire value, not just a known state — a sidecar newer than
   * this build must render a neutral "unknown" badge rather than crash the
   * timeline.
   */
  state: EdgeDeliveryState | string | null | undefined;
  /**
   * Why the event left the exact delivery path, straight from the sidecar's
   * `demotionReason`. It lands in the tooltip: "Sync deferred" with no reason
   * tells the operator where the event went but not whether that is routine or
   * a problem, and the second half is the actionable one.
   */
  demotionReason?: string | null;
  className?: string;
} & Omit<React.HTMLAttributes<HTMLSpanElement>, "children">;

/**
 * The per-message canonical-history label.
 *
 * This badge answers ONE question: did this message reach canonical history?
 * It never answers "was it sent" — by the time an event has a delivery state
 * it is already delivered locally to every client on this machine. Keeping the
 * two axes on separate labels is SPEC-2026-08-05 success criterion (3).
 */
export function DeliveryStateBadge({
  state,
  demotionReason,
  className,
  ...props
}: DeliveryStateBadgeProps) {
  const label = deliveryLabel(state);

  return (
    <Badge
      className={cn("gap-1", className)}
      title={deliveryDescription(state, demotionReason)}
      variant={deliveryTone(state)}
      {...props}
    >
      {label}
    </Badge>
  );
}
