/**
 * Single owner of a hover-opened menu's open state AND its pending hover timer.
 *
 * BUG-051: the community actions menu re-opened itself immediately after a
 * click. Every menu item closed the popover with a bare `setOpen(false)` while
 * the 80ms hover-open timer armed by `onMouseEnter` on the popover content was
 * still live; the orphaned timer fired ~60ms later and re-opened the menu the
 * user had just dismissed. Four call sites each had to remember to clear the
 * timer, and none of them did.
 *
 * The fix is structural rather than clerical: timer ownership moved OUT of the
 * component and into this object, and there is no way to change the open state
 * except through `setOpen`, which cancels the pending timer first. A fifth call
 * site cannot bypass the timer clear because the raw state setter is not what
 * callers hold.
 *
 * Invariant (asserted by `hoverMenuController.test.mjs`): after ANY `setOpen`,
 * `hasPendingTimer()` is false — no scheduled transition can outlive an
 * explicit one.
 */

export const HOVER_MENU_OPEN_DELAY_MS = 80;
export const HOVER_MENU_CLOSE_DELAY_MS = 160;

export type HoverMenuTimers = {
  setTimeout: (callback: () => void, delayMs: number) => number;
  clearTimeout: (handle: number) => void;
};

export type HoverMenuController = {
  /**
   * Apply an open state NOW. Always cancels any scheduled transition first —
   * this is the only entry point that mutates the menu's open state.
   */
  setOpen: (nextOpen: boolean) => void;
  /** Arm a delayed transition, replacing any transition already scheduled. */
  schedule: (nextOpen: boolean) => void;
  /** Cancel a pending transition without touching the open state (unmount). */
  dispose: () => void;
  /** Test/guardrail seam: is a hover transition still armed? */
  hasPendingTimer: () => boolean;
};

const defaultTimers: HoverMenuTimers = {
  setTimeout: (callback, delayMs) => window.setTimeout(callback, delayMs),
  clearTimeout: (handle) => {
    window.clearTimeout(handle);
  },
};

export function createHoverMenuController({
  setOpen,
  timers = defaultTimers,
  openDelayMs = HOVER_MENU_OPEN_DELAY_MS,
  closeDelayMs = HOVER_MENU_CLOSE_DELAY_MS,
}: {
  setOpen: (nextOpen: boolean) => void;
  timers?: HoverMenuTimers;
  openDelayMs?: number;
  closeDelayMs?: number;
}): HoverMenuController {
  let pendingHandle: number | null = null;

  function cancelPending(): void {
    if (pendingHandle !== null) {
      timers.clearTimeout(pendingHandle);
      pendingHandle = null;
    }
  }

  return {
    setOpen(nextOpen: boolean) {
      cancelPending();
      setOpen(nextOpen);
    },
    schedule(nextOpen: boolean) {
      cancelPending();
      pendingHandle = timers.setTimeout(
        () => {
          pendingHandle = null;
          setOpen(nextOpen);
        },
        nextOpen ? openDelayMs : closeDelayMs,
      );
    },
    dispose: cancelPending,
    hasPendingTimer: () => pendingHandle !== null,
  };
}
