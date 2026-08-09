import assert from "node:assert/strict";
import test from "node:test";

import {
  HOVER_MENU_CLOSE_DELAY_MS,
  HOVER_MENU_OPEN_DELAY_MS,
  createHoverMenuController,
} from "./hoverMenuController.ts";

/**
 * Deterministic stand-in for window timers. Nothing here waits on wall clock:
 * the BUG-051 guardrails must fail on the CONTROL FLOW (an orphaned timer),
 * never on a race that a longer timeout could paper over.
 */
function makeTimers() {
  const scheduled = new Map();
  const cleared = [];
  let nextHandle = 1;
  return {
    cleared,
    scheduled,
    timers: {
      setTimeout(callback, delayMs) {
        const handle = nextHandle++;
        scheduled.set(handle, { callback, delayMs });
        return handle;
      },
      clearTimeout(handle) {
        cleared.push(handle);
        scheduled.delete(handle);
      },
    },
    /** Fire every timer that is still armed. */
    flush() {
      for (const [handle, entry] of [...scheduled]) {
        scheduled.delete(handle);
        entry.callback();
      }
    },
  };
}

function makeController(overrides = {}) {
  const harness = makeTimers();
  const opened = [];
  const controller = createHoverMenuController({
    setOpen: (nextOpen) => opened.push(nextOpen),
    timers: harness.timers,
    ...overrides,
  });
  return { ...harness, controller, opened };
}

test("schedule arms the hover-open timer at the open delay", () => {
  const { controller, scheduled, opened } = makeController();
  controller.schedule(true);
  assert.equal(controller.hasPendingTimer(), true);
  assert.deepEqual(
    [...scheduled.values()].map((entry) => entry.delayMs),
    [HOVER_MENU_OPEN_DELAY_MS],
  );
  assert.deepEqual(opened, []);
});

test("schedule arms the hover-close timer at the close delay", () => {
  const { controller, scheduled } = makeController();
  controller.schedule(false);
  assert.deepEqual(
    [...scheduled.values()].map((entry) => entry.delayMs),
    [HOVER_MENU_CLOSE_DELAY_MS],
  );
});

test("a fired hover timer applies the open state and disarms itself", () => {
  const { controller, flush, opened } = makeController();
  controller.schedule(true);
  flush();
  assert.deepEqual(opened, [true]);
  assert.equal(controller.hasPendingTimer(), false);
});

// BUG-051 GUARDRAIL. Moving the pointer onto a menu item arms the 80ms
// hover-open timer; clicking the item closes the menu. If the close does not
// cancel that timer, it fires ~60ms later and re-opens the menu the user just
// dismissed — and, because the pointer is still inside, it never closes again.
// This asserts the CLEAR, not a delay: no amount of waiting can make it pass.
test("setOpen(false) clears an armed hover-open timer (BUG-051)", () => {
  const { controller, cleared, flush, opened, scheduled } = makeController();
  controller.schedule(true);
  const armedHandle = [...scheduled.keys()][0];

  controller.setOpen(false);

  assert.deepEqual(
    cleared,
    [armedHandle],
    "the close must clear the exact timer handle the hover armed",
  );
  assert.equal(
    controller.hasPendingTimer(),
    false,
    "no hover transition may outlive an explicit setOpen",
  );
  assert.equal(scheduled.size, 0);

  flush();
  assert.deepEqual(
    opened,
    [false],
    "the orphaned timer must not re-open the menu after the click closed it",
  );
});

test("setOpen(true) also clears an armed hover-close timer", () => {
  const { controller, cleared, flush, opened, scheduled } = makeController();
  controller.schedule(false);
  const armedHandle = [...scheduled.keys()][0];

  controller.setOpen(true);

  assert.deepEqual(cleared, [armedHandle]);
  flush();
  assert.deepEqual(
    opened,
    [true],
    "a pending close must not slam the menu shut right after an explicit open",
  );
});

test("every setOpen leaves no pending timer, whatever preceded it", () => {
  const { controller } = makeController();
  for (const scheduleNext of [true, false]) {
    for (const openNext of [true, false]) {
      controller.schedule(scheduleNext);
      controller.setOpen(openNext);
      assert.equal(
        controller.hasPendingTimer(),
        false,
        `setOpen(${openNext}) after schedule(${scheduleNext}) left a live timer`,
      );
    }
  }
});

test("schedule replaces a pending transition instead of stacking one", () => {
  const { controller, cleared, flush, opened, scheduled } = makeController();
  controller.schedule(true);
  const first = [...scheduled.keys()][0];
  controller.schedule(false);

  assert.deepEqual(cleared, [first]);
  assert.equal(scheduled.size, 1);
  flush();
  assert.deepEqual(opened, [false]);
});

test("dispose cancels a pending transition without changing open state", () => {
  const { controller, cleared, flush, opened, scheduled } = makeController();
  controller.schedule(true);
  const armedHandle = [...scheduled.keys()][0];

  controller.dispose();

  assert.deepEqual(cleared, [armedHandle]);
  assert.equal(controller.hasPendingTimer(), false);
  flush();
  assert.deepEqual(opened, [], "unmount must not push state after teardown");
});

test("dispose on an idle controller is a no-op", () => {
  const { controller, cleared, opened } = makeController();
  controller.dispose();
  controller.dispose();
  assert.deepEqual(cleared, []);
  assert.deepEqual(opened, []);
});
