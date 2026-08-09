/**
 * BUG-048 guardrail — the mention suggestion list must close SYNCHRONOUSLY the
 * moment its `@` trigger text is gone, AND must still resolve Tab/Enter against
 * the newest typed text while a query is genuinely in flight.
 *
 * ── The defect ────────────────────────────────────────────────────────────────
 * `useMentions.updateMentionQuery` ran detection behind a 120 ms debounce and
 * debounced the CLOSE as well as the OPEN — the same defect `useChannelLinks`
 * carried as BUG-042 and `useEmojiAutocomplete` carried unfixed. Clear an `@`
 * mention from the editor, press Enter inside that window, and `isMentionOpen`
 * was still true; `MessageComposer` ORs it into the editor's
 * `isAutocompleteOpen` ref, so the stale list swallowed the Enter.
 *
 * `flushMentionDebounce` did NOT cover this. When the `@` text is gone,
 * `detectPrefixQuery` finds nothing and the helper returns `null` — not
 * `"no-match"` — so `handleMentionKeyDown` fell straight through to
 * `suggestions[mentionSelectedIndex]` and inserted the stale mention. The flush
 * mitigates a DIFFERENT race (see test 4) and is deliberately kept.
 *
 * ── The fix ───────────────────────────────────────────────────────────────────
 * `updateDebouncedAutocompleteQuery` (see `debouncedAutocompleteQuery.ts`) runs
 * detection synchronously on every call. "No `@query` at the cursor" cancels the
 * pending timer and closes immediately; only the OPEN stays debounced.
 *
 * ── Why these tests are unit-level, not e2e ───────────────────────────────────
 * The defect's signature is "correct state, arriving too late", and an e2e
 * timing test for that can always be made green by waiting longer — commit
 * 7bcfe7e0a widened a post-Enter timeout from 5 s to 10 s and declared the flow
 * correct while it still failed 3/3. Tests 1, 2 and 3 read the real hook's state
 * inside a single synchronous block where no event-loop turn passes and
 * therefore NO pending timer can fire. There is no duration to widen.
 *
 * ── Mutation coverage ─────────────────────────────────────────────────────────
 * Verified by mutation, each killed by an assertion rather than by a syntax
 * error or a neighbouring guard:
 *   1. Re-introducing the defect (deleting the synchronous close from
 *      `updateDebouncedAutocompleteQuery`, leaving only the debounced timer)
 *      fails tests 1 and 3; tests 2 and 4 stay green.
 *   2. Deleting the debounce (opening immediately instead of scheduling) fails
 *      tests 2 and 4, never 1 or 3 — test 2 is the control that stops the "fix"
 *      from degenerating into "delete the debounce".
 *   3. Deleting the `flushMentionDebounce` call from `handleMentionKeyDown`
 *      fails ONLY test 4 — the control that stops the synchronous close from
 *      being mistaken for a replacement for the flush. Note that
 *      `flushMentionDebounce.test.mjs` stays fully green under this mutation:
 *      it tests the helper, never its call site.
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
  // `CommunitiesProvider` reads persisted community state on mount.
  localStorage: dom.window.localStorage,
  IS_REACT_ACT_ENVIRONMENT: true,
});

let React;
let act;
let createRoot;
let QueryClient;
let QueryClientProvider;
let CommunitiesProvider;
let useMentions;

before(async () => {
  React = await import("react");
  act = React.act;
  ({ createRoot } = await import("react-dom/client"));
  ({ QueryClient, QueryClientProvider } = await import(
    "@tanstack/react-query"
  ));
  ({ CommunitiesProvider } = await import(
    "@/features/communities/useCommunities.tsx"
  ));
  ({ useMentions } = await import("./useMentions.ts"));
});

after(() => {
  dom.window.close();
});

// ── Harness ───────────────────────────────────────────────────────────────────

// Passed as `externalMembers` so the candidate list is deterministic and needs
// no relay: every other directory query in the hook is free to fail in Node.
// Two names sharing the `Ali` prefix so test 4 can tell a stale query's top
// suggestion apart from the newest query's top suggestion.
const MEMBERS = [
  {
    pubkey: "a".repeat(64),
    displayName: "Alice Anderson",
    role: "member",
    isAgent: false,
  },
  {
    pubkey: "b".repeat(64),
    displayName: "Alicia Keys",
    role: "member",
    isAgent: false,
  },
];

/**
 * Mount the REAL `useMentions` and expose its live return value. Nothing about
 * the debounce is replicated here: the production hook owns the timer, and
 * these tests observe it.
 */
async function mountMentions(members = MEMBERS) {
  const apiRef = { current: null };

  function Probe() {
    apiRef.current = useMentions("channel-1", members, {});
    return null;
  }

  const client = new QueryClient({
    defaultOptions: { queries: { retry: false, gcTime: 0 } },
  });
  const root = createRoot(dom.window.document.createElement("div"));
  await act(async () => {
    root.render(
      React.createElement(
        QueryClientProvider,
        { client },
        React.createElement(
          CommunitiesProvider,
          null,
          React.createElement(Probe),
        ),
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
      client.clear();
    },
  };
}

/** Let the OPEN debounce (120 ms) elapse and flush the resulting render. */
async function letOpenDebounceLand() {
  await act(async () => {
    await new Promise((resolve) => setTimeout(resolve, 250));
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

test("mention suggestion list closes synchronously when its @trigger text is deleted", async () => {
  const harness = await mountMentions();
  try {
    act(() => {
      harness.api.updateMentionQuery("@Ali", 4);
    });
    await letOpenDebounceLand();
    assert.equal(
      harness.api.isMentionOpen,
      true,
      "precondition: typing @Ali opens the suggestion list",
    );

    // Everything from here to the assertion runs in one synchronous block, so
    // no timer callback can possibly have fired. The close must already have
    // happened by the very next read of the hook's state.
    act(() => {
      harness.api.updateMentionQuery("", 0);
    });

    assert.equal(
      harness.api.isMentionOpen,
      false,
      "isMentionOpen must be false immediately — a debounced close leaves a window in which Enter is stolen from the composer",
    );
    assert.equal(
      harness.api.suggestions.length,
      0,
      "the suggestion list must be emptied immediately, not on the debounce timer",
    );
  } finally {
    await harness.unmount();
  }
});

test("opening the mention suggestion list is still debounced", async () => {
  const harness = await mountMentions();
  try {
    act(() => {
      harness.api.updateMentionQuery("@Ali", 4);
    });

    assert.equal(
      harness.api.isMentionOpen,
      false,
      "the open must not be immediate — the debounce is what keeps a keystroke from re-ranking every candidate and re-rendering the list",
    );

    await letOpenDebounceLand();
    assert.equal(
      harness.api.isMentionOpen,
      true,
      "the list still opens once the debounce elapses",
    );
  } finally {
    await harness.unmount();
  }
});

test("Enter after clearing an @mention falls through to the composer", async () => {
  const harness = await mountMentions();
  try {
    // Editing an existing message that mentions someone: the editor emits the
    // loaded body with the caret at the end of "@Alice Anderson".
    act(() => {
      harness.api.updateMentionQuery("ping @Alice Anderson", 20);
    });
    await letOpenDebounceLand();
    assert.equal(
      harness.api.isMentionOpen,
      true,
      "precondition: the loaded body's @Alice Anderson opens the suggestion list",
    );

    // The user clears the editor and hits Enter immediately — well inside the
    // 120 ms window. Same synchronous block, so no timer has run.
    act(() => {
      harness.api.updateMentionQuery("", 0);
    });

    let preventedDefault = false;
    const result = harness.api.handleMentionKeyDown(
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
      "no mention may be produced by an Enter pressed after the @text was deleted",
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

test("Enter while the @query is still in flight resolves against the newest typed text", async () => {
  // This is why `flushMentionDebounce` survives the synchronous close: the two
  // guards cover different races. The synchronous close handles "the trigger
  // text is GONE"; this handles "the trigger text is NEWER than the open list".
  // A synchronous close cannot help here — detection succeeds, so the debounce
  // is still pending by design, and `suggestions` still describes "Ali".
  const harness = await mountMentions();
  try {
    act(() => {
      harness.api.updateMentionQuery("@Ali", 4);
    });
    await letOpenDebounceLand();
    assert.equal(
      harness.api.suggestions[0]?.displayName,
      "Alice Anderson",
      "precondition: the open list is ranked for @Ali, whose top hit is Alice Anderson",
    );

    // The user finishes typing "@Alicia" and hits Enter inside the 120 ms
    // window, so the list on screen is still the stale @Ali one.
    act(() => {
      harness.api.updateMentionQuery("@Alicia", 7);
    });
    assert.equal(
      harness.api.suggestions[0]?.displayName,
      "Alice Anderson",
      "precondition: the visible list has not caught up yet — the open is debounced",
    );

    // Wrapped in `act` only because the flush closes the dropdown as it
    // resolves; the call itself is synchronous and no timer runs inside it.
    let result;
    act(() => {
      result = harness.api.handleMentionKeyDown(makeEnterEvent(() => {}));
    });

    assert.equal(result.handled, true, "Enter is consumed by the open list");
    assert.equal(
      result.suggestion?.displayName,
      "Alicia Keys",
      "the flush must re-rank against the newest text — inserting the stale @Ali top hit would mention the wrong person",
    );
  } finally {
    await harness.unmount();
  }
});
