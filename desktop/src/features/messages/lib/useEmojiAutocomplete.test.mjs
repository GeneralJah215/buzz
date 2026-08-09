/**
 * BUG-048 guardrail — the emoji suggestion list must close SYNCHRONOUSLY the
 * moment its `:shortcode` trigger text is gone.
 *
 * ── The defect ────────────────────────────────────────────────────────────────
 * `useEmojiAutocomplete.updateEmojiQuery` ran detection behind a 120 ms debounce
 * and debounced the CLOSE as well as the OPEN — byte-for-byte the same defect
 * `useChannelLinks` carried as BUG-042. Edit a message containing a
 * `:shortcode:`, clear the editor, press Enter inside that window, and
 * `isEmojiAutocompleteOpen` was still true. `MessageComposer` ORs it into the
 * editor's `isAutocompleteOpen` ref, so the stale list swallowed the Enter and
 * inserted an emoji instead of submitting. Typing a replacement kept resetting
 * the timer, so the stale list never closed at all.
 *
 * ── The fix ───────────────────────────────────────────────────────────────────
 * `updateDebouncedAutocompleteQuery` (see `debouncedAutocompleteQuery.ts`) runs
 * detection synchronously on every call. "No `:query` at the cursor" cancels the
 * pending timer and closes immediately; only the OPEN stays debounced, which is
 * the only side that costs anything (an emoji-mart search plus a dropdown
 * re-render).
 *
 * ── Why these tests are unit-level, not e2e ───────────────────────────────────
 * The defect's signature is "correct state, arriving too late", and an e2e
 * timing test for that can always be made green by waiting longer — commit
 * 7bcfe7e0a widened a post-Enter timeout from 5 s to 10 s and declared the flow
 * correct while it still failed 3/3. Tests 1 and 3 below read the hook's state
 * directly inside a single synchronous block where no event-loop turn passes and
 * therefore NO pending timer can fire. There is no duration to widen: the close
 * either already happened or the test fails.
 *
 * ── Mutation coverage ─────────────────────────────────────────────────────────
 * Verified by mutation, each killed by an assertion rather than by a syntax
 * error or a neighbouring guard:
 *   1. Re-introducing the defect (deleting the synchronous close from
 *      `updateDebouncedAutocompleteQuery`, leaving only the debounced timer)
 *      fails tests 1 and 3 while test 2 stays green.
 *   2. Deleting the debounce (opening immediately instead of scheduling) fails
 *      ONLY test 2 — the control that stops the "fix" from degenerating into
 *      "delete the debounce".
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
let useEmojiAutocomplete;

before(async () => {
  React = await import("react");
  act = React.act;
  ({ createRoot } = await import("react-dom/client"));
  ({ useEmojiAutocomplete } = await import("./useEmojiAutocomplete.ts"));
});

after(() => {
  dom.window.close();
});

// ── Harness ───────────────────────────────────────────────────────────────────

// A custom emoji, not a standard one: the test loader stubs `emoji-mart`'s
// SearchIndex to return nothing, so the custom list is the deterministic source
// of suggestions here. `isEmojiAutocompleteOpen` needs a non-empty list.
const CUSTOM_EMOJI = [
  { shortcode: "partyblob", url: "https://cdn.example.test/partyblob.gif" },
  { shortcode: "partyparrot", url: "https://cdn.example.test/parrot.gif" },
];

/**
 * Mount the REAL `useEmojiAutocomplete` and expose its live return value.
 * Nothing about the debounce is replicated here: the production hook owns the
 * timer, and these tests observe it.
 */
async function mountEmojiAutocomplete(customEmoji = CUSTOM_EMOJI) {
  const apiRef = { current: null };

  function Probe() {
    apiRef.current = useEmojiAutocomplete(customEmoji);
    return null;
  }

  const root = createRoot(dom.window.document.createElement("div"));
  await act(async () => {
    root.render(React.createElement(Probe));
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

/**
 * Drain the promise chain the suggestion effect runs (emoji-mart search →
 * setSuggestions) WITHOUT advancing wall-clock time, so the 120 ms debounce
 * cannot have elapsed. Deliberately not a `setTimeout`: there is no duration
 * here for a future maintainer to widen.
 */
async function flushMicrotasks() {
  await act(async () => {
    for (let i = 0; i < 25; i += 1) {
      await Promise.resolve();
    }
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

test("emoji suggestion list closes synchronously when its :shortcode text is deleted", async () => {
  const harness = await mountEmojiAutocomplete();
  try {
    act(() => {
      harness.api.updateEmojiQuery(":party", 6);
    });
    await letOpenDebounceLand();
    assert.equal(
      harness.api.isEmojiAutocompleteOpen,
      true,
      "precondition: typing :party opens the suggestion list",
    );

    // Everything from here to the assertion runs in one synchronous block, so
    // no timer callback can possibly have fired. The close must already have
    // happened by the very next read of the hook's state.
    act(() => {
      harness.api.updateEmojiQuery("", 0);
    });

    assert.equal(
      harness.api.isEmojiAutocompleteOpen,
      false,
      "isEmojiAutocompleteOpen must be false immediately — a debounced close leaves a window in which Enter is stolen from the composer",
    );
    assert.equal(
      harness.api.emojiSuggestions.length,
      0,
      "the suggestion list must be emptied immediately, not on the debounce timer",
    );
  } finally {
    await harness.unmount();
  }
});

test("opening the emoji suggestion list is still debounced", async () => {
  const harness = await mountEmojiAutocomplete();
  try {
    act(() => {
      harness.api.updateEmojiQuery(":party", 6);
    });

    // Drain every microtask the open path would need. If the open were not
    // debounced, the query would already be set and the search already
    // resolved, so suggestions would be populated by now.
    await flushMicrotasks();

    assert.equal(
      harness.api.emojiSuggestions.length,
      0,
      "the open must not be immediate — the debounce is what keeps a keystroke from re-running the emoji search and re-rendering the list",
    );
    assert.equal(
      harness.api.isEmojiAutocompleteOpen,
      false,
      "the list must not be open before its debounce has elapsed",
    );

    await letOpenDebounceLand();
    assert.equal(
      harness.api.isEmojiAutocompleteOpen,
      true,
      "the list still opens once the debounce elapses",
    );
  } finally {
    await harness.unmount();
  }
});

test("Enter after clearing a :shortcode falls through to the composer", async () => {
  const harness = await mountEmojiAutocomplete();
  try {
    // Editing an existing message whose body contains a shortcode: the editor
    // emits the loaded body with the caret at the end of ":partyblob".
    act(() => {
      harness.api.updateEmojiQuery("ship it :partyblob", 18);
    });
    await letOpenDebounceLand();
    assert.equal(
      harness.api.isEmojiAutocompleteOpen,
      true,
      "precondition: the loaded body's :partyblob opens the suggestion list",
    );

    // The user clears the editor and hits Enter immediately — well inside the
    // 120 ms window. Same synchronous block, so no timer has run.
    act(() => {
      harness.api.updateEmojiQuery("", 0);
    });

    let preventedDefault = false;
    const result = harness.api.handleEmojiKeyDown(
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
      "no emoji may be produced by an Enter pressed after the :shortcode was deleted",
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
