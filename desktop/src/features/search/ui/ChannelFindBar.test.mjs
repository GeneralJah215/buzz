/**
 * Find bar mount + focus guardrails (BUG-059).
 *
 * The bar used to focus its input on mount only. Pressing ⌘F/Ctrl+F again
 * while it was already open therefore did nothing at all — no focus, no
 * selection — which is the one thing every other find bar in existence does.
 * `focusRequestId` is the trigger; these tests pin that it is honoured after
 * mount, not just at it.
 */

import assert from "node:assert/strict";
import { after, describe, test } from "node:test";

import { JSDOM } from "jsdom";

const dom = new JSDOM("<!doctype html><html><body></body></html>", {
  url: "http://localhost",
});

after(() => dom.window.close());

Object.assign(globalThis, {
  document: dom.window.document,
  Event: dom.window.Event,
  HTMLElement: dom.window.HTMLElement,
  Node: dom.window.Node,
  IS_REACT_ACT_ENVIRONMENT: true,
  window: dom.window,
});

const React = await import("react");
const { act } = React;
const { createRoot } = await import("react-dom/client");
const { ChannelFindBar, ChannelFindBarSlot } = await import(
  "./ChannelFindBar.tsx"
);

function noop() {}

/**
 * Compare focus as a boolean, never as two DOM nodes: assert.equal on a
 * mismatch would deep-diff a whole element tree and turn a one-line failure
 * into a two-minute one.
 */
function isFocused(element) {
  return dom.window.document.activeElement === element;
}

async function mount(element) {
  const container = dom.window.document.createElement("div");
  dom.window.document.body.appendChild(container);
  const root = createRoot(container);
  await act(async () => {
    root.render(element);
  });

  return {
    container,
    async render(next) {
      await act(async () => {
        root.render(next);
      });
    },
    async unmount() {
      await act(async () => {
        root.unmount();
      });
      container.remove();
    },
  };
}

function bar(focusRequestId) {
  return React.createElement(ChannelFindBar, {
    focusRequestId,
    matchCount: 3,
    matchIndex: 0,
    onClose: noop,
    onNext: noop,
    onPrevious: noop,
    onQueryChange: noop,
    query: "deploy",
  });
}

describe("ChannelFindBar", { concurrency: 1 }, () => {
  test("focuses and selects its input on open", async (t) => {
    const harness = await mount(bar(1));
    t.after(() => harness.unmount());

    const input = harness.container.querySelector("input");
    assert.equal(isFocused(input), true);
    assert.equal(input.selectionStart, 0);
    assert.equal(input.selectionEnd, "deploy".length);
  });

  test("re-focuses and re-selects when the find shortcut is pressed again", async (t) => {
    const harness = await mount(bar(1));
    t.after(() => harness.unmount());

    const input = harness.container.querySelector("input");
    // Simulate the reader having moved on: focus elsewhere, cursor collapsed.
    await act(async () => {
      input.blur();
      input.setSelectionRange(6, 6);
    });
    assert.equal(isFocused(input), false);

    // A second ⌘F/Ctrl+F bumps focusRequestId without remounting the bar.
    await harness.render(bar(2));

    assert.equal(
      isFocused(input),
      true,
      "pressing find again must put the caret back in the find input",
    );
    assert.equal(
      input.selectionEnd - input.selectionStart,
      "deploy".length,
      "and select the existing query, so typing replaces it",
    );
  });

  test("an unrelated re-render does not steal focus back", async (t) => {
    const harness = await mount(bar(1));
    t.after(() => harness.unmount());

    const input = harness.container.querySelector("input");
    await act(async () => {
      input.blur();
    });

    await harness.render(bar(1));

    assert.equal(
      isFocused(input),
      false,
      "focus belongs to the reader between find presses",
    );
  });
});

describe("ChannelFindBarSlot", { concurrency: 1 }, () => {
  const closedFind = {
    activeIndex: 0,
    close: noop,
    focusRequestId: 0,
    goToNext: noop,
    goToPrevious: noop,
    isOpen: false,
    matchCount: 0,
    query: "",
    setQuery: noop,
  };

  test("renders nothing while the find state is closed", async (t) => {
    const harness = await mount(
      React.createElement(ChannelFindBarSlot, { find: closedFind }),
    );
    t.after(() => harness.unmount());

    assert.equal(
      harness.container.querySelector('[data-testid="channel-find-bar"]'),
      null,
    );
  });

  test("renders the bar once the find state is open", async (t) => {
    const harness = await mount(
      React.createElement(ChannelFindBarSlot, {
        find: { ...closedFind, isOpen: true, matchCount: 3, query: "deploy" },
      }),
    );
    t.after(() => harness.unmount());

    assert.ok(
      harness.container.querySelector('[data-testid="channel-find-bar"]'),
      "the shortcut only claims the key when this slot can paint",
    );
  });
});
