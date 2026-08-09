import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

/**
 * Structural tripwire for BUG-051.
 *
 * The behavioural guarantee — "an explicit open/close cancels the pending
 * hover timer" — is proved by `lib/hoverMenuController.test.mjs`. That proof is
 * worth nothing if a call site closes the menu WITHOUT going through the
 * controller, which is exactly how BUG-051 happened: four menu items each wrote
 * `setDropdownOpen(false)` directly and left the 80ms hover-open timer armed.
 *
 * So this scan enforces the other half: inside `CommunitySwitcher.tsx` the only
 * way to change the menu's open state is `menu.setOpen`, and the component owns
 * no timers of its own. A fifth menu item that reaches for the raw React setter
 * or re-grows a `window.setTimeout` fails here, at unit speed, with no reliance
 * on pointer timing.
 */

const SOURCE = fs.readFileSync(
  path.resolve(
    path.dirname(fileURLToPath(import.meta.url)),
    "CommunitySwitcher.tsx",
  ),
  "utf8",
);

test("the raw open-state setter is never called directly", () => {
  const directCalls = SOURCE.match(/setDropdownOpenState\s*\(/g) ?? [];
  assert.deepEqual(
    directCalls,
    [],
    "close the menu with `menu.setOpen(false)` — a bare state write leaves the hover timer armed (BUG-051)",
  );
});

test("no legacy `setDropdownOpen` call site survives", () => {
  assert.equal(
    /(?<!State)\bsetDropdownOpen\s*\(/.test(SOURCE),
    false,
    "`setDropdownOpen(...)` was the bypass that caused BUG-051",
  );
});

test("the component owns no hover timer of its own", () => {
  assert.equal(
    /\b(?:window\.)?(?:set|clear)Timeout\s*\(/.test(SOURCE),
    false,
    "timer ownership lives in createHoverMenuController, not in the component",
  );
});

test("the menu is wired to the hover controller", () => {
  assert.match(
    SOURCE,
    /createHoverMenuController\(\{\s*setOpen:\s*setDropdownOpenState/,
    "the controller must be the sole writer of the open state",
  );
  const openChangeHandlers = SOURCE.match(/onOpenChange=\{menu\.setOpen\}/g);
  assert.equal(
    openChangeHandlers?.length,
    2,
    "both the popover and the dropdown must close through the controller",
  );
});

test("every menu action closes through the controller", () => {
  const controllerCloses = SOURCE.match(/menu\.setOpen\(false\)/g) ?? [];
  assert.ok(
    controllerCloses.length >= 5,
    `expected every menu action to close via menu.setOpen(false); found ${controllerCloses.length}`,
  );
});
