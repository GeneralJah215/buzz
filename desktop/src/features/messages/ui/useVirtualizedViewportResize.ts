import * as React from "react";

/**
 * Sub-pixel noise floor for observed viewport geometry.
 *
 * ResizeObserver reports fractional content-box sizes on fractional device
 * pixel ratios, and a re-delivery whose dimensions round-trip to the same
 * layout is not a reflow the bottom pin has to answer.
 */
const VIEWPORT_SIZE_EPSILON_PX = 0.5;

export type ObservedViewportSize = { width: number; height: number };

export function shouldSettleVirtualizedViewportResize({
  virtualizerAtBottom,
}: {
  virtualizerAtBottom: boolean;
}): boolean {
  return virtualizerAtBottom;
}

/**
 * True when an observed viewport size differs enough from the previous one to
 * be a real reflow. Unknown geometry (no entry, non-finite size) counts as a
 * change: the safe direction is to re-settle, never to skip a needed pin.
 */
export function hasViewportSizeChanged(
  previous: ObservedViewportSize | null,
  next: ObservedViewportSize | null,
): boolean {
  if (!next || !Number.isFinite(next.width) || !Number.isFinite(next.height)) {
    return true;
  }
  if (!previous) return true;
  return (
    Math.abs(next.width - previous.width) > VIEWPORT_SIZE_EPSILON_PX ||
    Math.abs(next.height - previous.height) > VIEWPORT_SIZE_EPSILON_PX
  );
}

function readObservedSize(
  entries: ResizeObserverEntry[] | undefined,
): ObservedViewportSize | null {
  const rect = entries?.[0]?.contentRect;
  if (!rect) return null;
  return { width: rect.width, height: rect.height };
}

/** Re-settles a bottom-pinned virtualized timeline after viewport reflow. */
export function useVirtualizedViewportResize(
  scrollContainerRef: React.RefObject<HTMLDivElement | null>,
  virtualizerAtBottomRef: React.RefObject<boolean>,
  settleAtBottom?: () => void,
) {
  React.useEffect(() => {
    const container = scrollContainerRef.current;
    if (
      !container ||
      !settleAtBottom ||
      typeof ResizeObserver === "undefined"
    ) {
      return;
    }

    // The settle this schedules writes scroll position, which changes the
    // rendered range, which resizes this very element. Doing that synchronously
    // inside the callback puts the write in the same delivery pass at a deeper
    // depth — that is the "ResizeObserver loop completed with undelivered
    // notifications" error. Deferring to a frame moves the write out of the
    // observation pass entirely; it is not a delay knob, and shortening it is
    // not possible because there is no shorter unit than "not this pass".
    let frameId: number | null = null;
    let lastSize: ObservedViewportSize | null = null;

    const observer = new ResizeObserver((entries) => {
      const size = readObservedSize(entries);
      if (!hasViewportSizeChanged(lastSize, size)) return;
      lastSize = size;
      if (
        !shouldSettleVirtualizedViewportResize({
          virtualizerAtBottom: virtualizerAtBottomRef.current,
        })
      ) {
        return;
      }
      if (frameId !== null) cancelAnimationFrame(frameId);
      frameId = requestAnimationFrame(() => {
        frameId = null;
        settleAtBottom();
      });
    });
    observer.observe(container);
    return () => {
      observer.disconnect();
      if (frameId !== null) {
        cancelAnimationFrame(frameId);
        frameId = null;
      }
    };
  }, [scrollContainerRef, settleAtBottom, virtualizerAtBottomRef]);
}
