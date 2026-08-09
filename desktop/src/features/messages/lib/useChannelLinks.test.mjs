/**
 * BUG-042 guardrail — the channel-link suggestion list must close
 * SYNCHRONOUSLY the moment its `#` trigger text is gone.
 *
 * ── The defect ────────────────────────────────────────────────────────────────
 * `useChannelLinks.updateChannelQuery` ran detection behind a 120 ms debounce
 * and debounced the CLOSE as well as the OPEN. Edit a message containing a
 * `#channel`, clear the editor, press Enter inside that window, and
 * `isChannelOpen` was still true — `MessageComposer` feeds it straight into the
 * editor's `isAutocompleteOpen` ref, so the suggestion list swallowed the Enter
 * and inserted a channel chip instead of saving the edit. Typing a replacement
 * kept resetting the timer, so the stale list never closed at all.
 *
 * ── The fix ───────────────────────────────────────────────────────────────────
 * Detection now runs synchronously on every call. A "no query here" result
 * closes the list immediately (and cancels any pending timer); only the OPEN
 * state update stays behind the debounce, which is the only part that costs
 * anything (it re-filters the channel list and re-renders the dropdown).
 *
 * ── Why these tests are unit-level, not e2e ───────────────────────────────────
 * This defect's whole signature is "correct state, arriving too late". An e2e
 * timing test for it can always be made green by waiting longer — commit
 * 7bcfe7e0a did exactly that, widening a post-Enter timeout from 5 s to 10 s
 * and declaring the flow correct while it still failed 3/3. These assertions
 * read the hook's state directly, inside a single synchronous block where no
 * event-loop turn passes and therefore NO pending timer can fire. There is no
 * duration to widen: the close either already happened or the test fails.
 *
 * ── Mutation coverage ─────────────────────────────────────────────────────────
 * Deleting the synchronous-close branch from `updateChannelQuery` (restoring
 * the pre-fix behaviour) fails tests 1 and 3. Deleting the debounce outright
 * fails test 2. Both were verified by mutation before commit.
 */

import assert from "node:assert/strict";
import { after, before, test } from "node:test";

import { JSDOM } from "jsdom";

// react-dom/client needs a DOM at call time. Install it in the module body,
// then pull React in dynamically inside `before()` so nothing React-shaped is
// evaluated against a bare Node global object.
const dom = new JSDOM("<!doctype html><html><body></body></html>", {
  url: "http://localhost",
});

Object.assign(globalThis, {
  document: dom.window.document,
  Element: dom.window.Element,
  HTMLElement: dom.window.HTMLElement,
  Node: dom.window.Node,
  window: dom.window,
  IS_REACT_ACT_ENVIRONMENT: true,
});

let React;
let act;
let createRoot;
let ChannelNavigationProvider;
let useChannelLinks;

before(async () => {
  React = await import("react");
  act = React.act;
  ({ createRoot } = await import("react-dom/client"));
  ({ ChannelNavigationProvider } = await import(
    "@/shared/context/ChannelNavigationContext"
  ));
  ({ useChannelLinks } = await import("./useChannelLinks.ts"));
});

after(() => {
  dom.window.close();
});

// ── Harness ───────────────────────────────────────────────────────────────────

const CHANNELS = [
  { id: "c-general", name: "general", channelType: "stream" },
  { id: "c-random", name: "random", channelType: "stream" },
  { id: "d-alice", name: "alice", channelType: "dm" },
];

/**
 * Mount the REAL `useChannelLinks` under a real ChannelNavigationProvider and
 * expose its live return value. Nothing about the debounce is replicated here:
 * the production hook owns the timer, and these tests observe it.
 */
async function mountChannelLinks(channels = CHANNELS) {
  const apiRef = { current: null };

  function Probe() {
    apiRef.current = useChannelLinks();
    return null;
  }

  const root = createRoot(dom.window.document.createElement("div"));
  await act(async () => {
    root.render(
      React.createElement(
        ChannelNavigationProvider,
        { channels },
        React.createElement(Probe),
      ),
    );
  });

  return {
    get api() {
      return apiRef.current;
    },
    unmount: async () => {
      await act(async () => {
        root.unmount();
      });
    },
  };
}

/** Let the OPEN debounce (120 ms) elapse and flush the resulting render. */
async function letOpenDebounceLand() {
  await act(async () => {
    await new Promise((resolve) => setTimeout(resolve, 200));
  });
}

/** A plain-Enter keydown, shaped like the React synthetic event the hook reads. */
function makeEnterEvent(onPreventDefault) {
  return {
    key: "Enter",
    altKey: false,
    ctrlKey: false,
    metaKey: false,
    shiftKey: false,
    preventDefault: onPreventDefault,
  };
}

// ── Tests ─────────────────────────────────────────────────────────────────────

test("channel suggestion list closes synchronously when its #trigger text is deleted", async () => {
  const harness = await mountChannelLinks();
  try {
    act(() => {
      harness.api.updateChannelQuery("#gen", 4);
    });
    await letOpenDebounceLand();
    assert.equal(
      harness.api.isChannelOpen,
      true,
      "precondition: typing #gen opens the suggestion list",
    );

    // Everything from here to the assertion runs in one synchronous block, so
    // no timer callback can possibly have fired. The close must already have
    // happened by the very next read of the hook's state.
    act(() => {
      harness.api.updateChannelQuery("", 0);
    });

    assert.equal(
      harness.api.isChannelOpen,
      false,
      "isChannelOpen must be false immediately — a debounced close leaves a window in which Enter is stolen from the composer",
    );
    assert.equal(
      harness.api.channelQuery,
      null,
      "channelQuery must be cleared immediately, not on the debounce timer",
    );
  } finally {
    await harness.unmount();
  }
});

test("opening the channel suggestion list is still debounced", async () => {
  const harness = await mountChannelLinks();
  try {
    act(() => {
      harness.api.updateChannelQuery("#gen", 4);
    });

    assert.equal(
      harness.api.isChannelOpen,
      false,
      "the open must not be immediate — the debounce is what keeps a keystroke from re-filtering and re-rendering the channel list",
    );

    await letOpenDebounceLand();
    assert.equal(
      harness.api.isChannelOpen,
      true,
      "the list still opens once the debounce elapses",
    );
  } finally {
    await harness.unmount();
  }
});

test("Enter after clearing a #channel body falls through to the composer", async () => {
  const harness = await mountChannelLinks();
  try {
    // Editing an existing message whose body contains a channel link: the
    // editor emits the loaded body with the caret at the end of "#general".
    act(() => {
      harness.api.updateChannelQuery("see #general", 12);
    });
    await letOpenDebounceLand();
    assert.equal(
      harness.api.isChannelOpen,
      true,
      "precondition: the loaded body's #general opens the suggestion list",
    );

    // The user clears the editor and hits Enter immediately — well inside the
    // 120 ms window. Same synchronous block, so no timer has run.
    act(() => {
      harness.api.updateChannelQuery("", 0);
    });

    let preventedDefault = false;
    const result = harness.api.handleChannelKeyDown(
      makeEnterEvent(() => {
        preventedDefault = true;
      }),
    );

    assert.equal(
      result.handled,
      false,
      "Enter must reach the composer's submit path, not the suggestion list",
    );
    assert.equal(
      result.suggestion,
      undefined,
      "no channel chip may be produced by an Enter pressed after the #text was deleted",
    );
    assert.equal(
      preventedDefault,
      false,
      "the suggestion list must not preventDefault the user's submit key",
    );
  } finally {
    await harness.unmount();
  }
});
