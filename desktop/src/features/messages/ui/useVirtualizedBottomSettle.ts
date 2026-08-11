import * as React from "react";
import type { VListHandle } from "virtua";

/**
 * Distance from the physical floor below which a bottom pin would be a no-op
 * write. Tight enough that a partially revealed trailing row still re-pins,
 * loose enough to absorb sub-pixel layout rounding.
 */
const BOTTOM_EPSILON_PX = 1;

type ScrollerBottomMetrics = {
  scrollHeight: number;
  clientHeight: number;
  scrollTop: number;
};

/**
 * True when the scroller is already at its physical floor, so `scrollToIndex`
 * would write the position it is already in.
 *
 * That write is not free: it re-renders Virtua's range, which resizes the
 * scroller and the inner sizing element, which re-notifies the observers that
 * asked for the pin. Skipping it is what makes the pin converge instead of
 * running every frame for as long as bottom intent is armed.
 *
 * Unknown geometry (a detached node, a non-finite measurement) reports false so
 * the pin still happens; the failure direction must be "scroll needlessly",
 * never "silently stop following the conversation".
 */
export function isVirtualizedScrollerAtBottom(
  metrics: ScrollerBottomMetrics,
): boolean {
  const distance =
    metrics.scrollHeight - metrics.clientHeight - metrics.scrollTop;
  if (!Number.isFinite(distance)) return false;
  return distance <= BOTTOM_EPSILON_PX;
}

const SCROLL_INTENT_KEYS = new Set([
  "ArrowDown",
  "ArrowUp",
  "End",
  "Home",
  "PageDown",
  "PageUp",
  " ",
]);

function isEditableKeyboardTarget(target: EventTarget | null) {
  if (!(target instanceof HTMLElement)) return false;
  if (target.isContentEditable) return true;
  return (
    target.closest("input, textarea, select, [contenteditable='true']") !== null
  );
}

export function useVirtualizedBottomSettle(
  hostRef: React.RefObject<HTMLDivElement | null>,
  listRef: React.RefObject<VListHandle | null>,
  itemsLengthRef: React.RefObject<number>,
) {
  const bottomIntentRef = React.useRef(false);
  const frameRef = React.useRef<number | null>(null);

  const cancelFrame = React.useCallback(() => {
    if (frameRef.current !== null) {
      cancelAnimationFrame(frameRef.current);
      frameRef.current = null;
    }
  }, []);

  const cancel = React.useCallback(() => {
    bottomIntentRef.current = false;
    cancelFrame();
  }, [cancelFrame]);

  const pinToBottom = React.useCallback(() => {
    if (!bottomIntentRef.current) return;
    const scroller = hostRef.current?.firstElementChild;
    const lastIndex = itemsLengthRef.current - 1;
    if (!(scroller instanceof HTMLDivElement) || lastIndex < 0) return;
    // Already on the floor: the write would change nothing visible and would
    // re-trigger the observers that scheduled it.
    if (isVirtualizedScrollerAtBottom(scroller)) return;
    listRef.current?.scrollToIndex(lastIndex, { align: "end" });
  }, [hostRef, itemsLengthRef, listRef]);

  const schedulePinToBottom = React.useCallback(() => {
    if (!bottomIntentRef.current || frameRef.current !== null) return;
    frameRef.current = requestAnimationFrame(() => {
      frameRef.current = null;
      pinToBottom();
    });
  }, [pinToBottom]);

  React.useLayoutEffect(() => {
    const scroller = hostRef.current?.firstElementChild;
    if (!(scroller instanceof HTMLDivElement)) return;
    const retire = () => cancel();
    const retireForPointer = (event: PointerEvent) => {
      // A descendant pointerdown is ordinary row interaction (link, reaction,
      // thread action), not evidence that the reader took scroll ownership.
      // Direct scroller hits cover scrollbar/background drag initiation.
      if (event.target === scroller) cancel();
    };
    const retireForWheel = (event: WheelEvent) => {
      // Ctrl+wheel is browser zoom, not reader navigation. Keep bottom intent
      // armed so the resulting viewport/content reflow can settle at the new
      // physical floor.
      if (!event.ctrlKey) cancel();
    };
    const retireForScrollKey = (event: KeyboardEvent) => {
      if (
        !event.altKey &&
        !event.ctrlKey &&
        !event.metaKey &&
        event.target instanceof Node &&
        scroller.contains(event.target) &&
        !isEditableKeyboardTarget(event.target) &&
        SCROLL_INTENT_KEYS.has(event.key)
      ) {
        cancel();
      }
    };
    scroller.addEventListener("pointerdown", retireForPointer, {
      passive: true,
    });
    scroller.addEventListener("touchmove", retire, { passive: true });
    scroller.addEventListener("wheel", retireForWheel, { passive: true });
    window.addEventListener("keydown", retireForScrollKey, true);
    return () => {
      scroller.removeEventListener("pointerdown", retireForPointer);
      scroller.removeEventListener("touchmove", retire);
      scroller.removeEventListener("wheel", retireForWheel);
      window.removeEventListener("keydown", retireForScrollKey, true);
    };
  }, [cancel, hostRef]);

  React.useLayoutEffect(() => {
    const scroller = hostRef.current?.firstElementChild;
    if (!(scroller instanceof HTMLDivElement)) return;
    const content = scroller.firstElementChild;
    if (
      !(content instanceof HTMLElement) ||
      typeof ResizeObserver === "undefined"
    ) {
      return;
    }
    // Virtua updates this inner element's extent whenever measured row geometry
    // changes. Bottom intent therefore follows the physical floor for as long
    // as it remains active, rather than guessing that layout is done after an
    // arbitrary timeout. Reader input and explicit target/prepend navigation
    // retire the intent through `cancel`.
    //
    // The scroller stays observed alongside the content: a viewport-only change
    // (split panel, window resize, composer growth) moves the physical floor
    // without resizing content, and this observer's gate is bottom *intent*,
    // which is armed in windows where the virtualizer's reported at-bottom flag
    // is transiently false. It no longer free-runs, because `pinToBottom` skips
    // the write once the floor is reached, so the loop terminates at the write
    // rather than at a timer.
    const observer = new ResizeObserver(schedulePinToBottom);
    observer.observe(content);
    observer.observe(scroller);
    return () => observer.disconnect();
  }, [hostRef, schedulePinToBottom]);

  /**
   * Arm bottom intent and pin NOW, deliberately bypassing the frame throttle.
   *
   * Every caller reaches this from a layout effect that has just committed new
   * trailing rows (an arrival, or the split panel taking viewport height). The
   * write has to land before the browser paints that commit; a frame of
   * deferral paints one frame of the new message sitting below the fold and
   * then yanks it up. The throttle exists for observer-driven corrections,
   * which arrive after paint and have nothing to be simultaneous with.
   *
   * This synchronous call is safe against the ResizeObserver feedback loop
   * because it is never invoked from inside an observation pass — the viewport
   * observer defers to a frame before calling it, and `pinToBottom` no-ops when
   * the scroller is already on the floor.
   */
  const settle = React.useCallback(() => {
    bottomIntentRef.current = true;
    cancelFrame();
    pinToBottom();
  }, [cancelFrame, pinToBottom]);

  React.useEffect(() => cancel, [cancel]);
  return { cancel, settle };
}
