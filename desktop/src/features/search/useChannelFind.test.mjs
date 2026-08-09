/**
 * Behaviour tests for the Ctrl+F / ⌘F channel find bar (BUG-055).
 *
 * These mount the REAL `useChannelFind` hook (createRoot + act) against a
 * jsdom window and a stubbed `search_messages` Tauri command, so both the
 * client-side match pass and the relay-backed pass are exercised through the
 * production code path — not a re-implementation of it.
 *
 * Two defects are pinned here:
 *
 *   1. Retyping the query kept the previous match cursor. `activeIndex` was
 *      only ever clamped (`useEffect` on `matchedIds.length`), never reset, so
 *      a fresh search term dropped the reader into the middle of the new
 *      result set — "6 of 9" for a term just typed.
 *
 *   2. Relay hits for a SUPERSEDED query were merged into the match list. The
 *      client pass reads `query` (every keystroke) while the relay pass reads
 *      `debouncedQuery` (300 ms behind), and nothing reconciled the two. For
 *      the ~300 ms after each keystroke the find bar counted, highlighted and
 *      navigated to messages that do not contain what is in the input.
 */

import assert from "node:assert/strict";
import { after, describe, test } from "node:test";

import { JSDOM } from "jsdom";

// ── Environment ──────────────────────────────────────────────────────────────
// jsdom must be installed on globalThis BEFORE React is imported: react-dom
// captures `document` at module scope.

const dom = new JSDOM("<!doctype html><html><body></body></html>", {
  url: "http://localhost",
});

// `useSearchMessagesQuery` sets gcTime to 5 minutes. A test that fails its
// assertion never reaches its own teardown, and react-query's pending garbage
// collection timer would then hold the runner open for those 5 minutes. Track
// every client so teardown is unconditional.
const liveQueryClients = new Set();

after(() => {
  for (const client of liveQueryClients) {
    client.clear();
  }
  liveQueryClients.clear();
  dom.window.close();
});

/** query string → hits the stubbed relay returns for it. */
const relayHitsByQuery = new Map();
/** Every `search_messages` payload the hook actually sent. */
const relayCalls = [];

dom.window.__TAURI_INTERNALS__ = {
  invoke: async (command, args) => {
    if (command !== "search_messages") {
      throw new Error(`unexpected tauri command in find-bar test: ${command}`);
    }
    relayCalls.push(args);
    const hits = relayHitsByQuery.get(args.q) ?? [];
    return { hits, found: hits.length };
  },
};

// The debounce inside useChannelFind uses `window.setTimeout`, which jsdom
// owns and React does not touch (React resolves the bare global, i.e. node's).
// Replacing it here makes the debounce boundary explicit instead of a sleep.
const pendingDebounceTimers = new Map();
let nextDebounceTimerId = 1;
dom.window.setTimeout = (callback) => {
  const id = nextDebounceTimerId++;
  pendingDebounceTimers.set(id, callback);
  return id;
};
dom.window.clearTimeout = (id) => {
  pendingDebounceTimers.delete(id);
};

function flushDebounce() {
  const callbacks = [...pendingDebounceTimers.values()];
  pendingDebounceTimers.clear();
  for (const callback of callbacks) {
    callback();
  }
}

Object.assign(globalThis, {
  document: dom.window.document,
  Event: dom.window.Event,
  HTMLElement: dom.window.HTMLElement,
  KeyboardEvent: dom.window.KeyboardEvent,
  Node: dom.window.Node,
  IS_REACT_ACT_ENVIRONMENT: true,
  window: dom.window,
});

const { QueryClient, QueryClientProvider } = await import(
  "@tanstack/react-query"
);
const React = await import("react");
const { act } = React;
const { createRoot } = await import("react-dom/client");
const { useChannelFind } = await import("./useChannelFind.ts");

// ── Fixtures ─────────────────────────────────────────────────────────────────

const CHANNEL_ID = "11111111-2222-3333-4444-555555555555";

function message(id, body) {
  return {
    id,
    renderKey: id,
    createdAt: 1_700_000_000,
    pubkey: "a".repeat(64),
    signerPubkey: "a".repeat(64),
    author: "tester",
    isAgent: false,
    ownerPubkey: null,
    ownerLabel: null,
    avatarUrl: null,
    role: undefined,
    time: "12:00",
    body,
    parentId: null,
    rootId: null,
    depth: 0,
    accent: false,
    kind: 9,
    tags: [["h", CHANNEL_ID]],
  };
}

function relayHit(eventId, content) {
  return {
    event_id: eventId,
    content,
    kind: 9,
    pubkey: "b".repeat(64),
    channel_id: CHANNEL_ID,
    channel_name: "general",
    created_at: 1_699_000_000,
    score: 1,
  };
}

/** Mount the real hook and expose its latest return value. */
async function mountChannelFind(messages) {
  const container = dom.window.document.createElement("div");
  dom.window.document.body.appendChild(container);

  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  liveQueryClients.add(queryClient);

  const state = { current: null };
  // react-query notifies its observers outside any act() scope, so the render
  // its resolution schedules can land after the pump loop gives up. A probe
  // re-render reads the same cache the observer would and is the only settle
  // signal that does not depend on notification timing.
  let forceRender = () => {};

  function Probe() {
    const [, setTick] = React.useState(0);
    forceRender = () => setTick((tick) => tick + 1);
    state.current = useChannelFind({ channelId: CHANNEL_ID, messages });
    return null;
  }

  const root = createRoot(container);
  await act(async () => {
    root.render(
      React.createElement(
        QueryClientProvider,
        { client: queryClient },
        React.createElement(Probe),
      ),
    );
  });

  return {
    get find() {
      return state.current;
    },
    async openWithShortcut() {
      await act(async () => {
        dom.window.dispatchEvent(
          new dom.window.KeyboardEvent("keydown", {
            bubbles: true,
            ctrlKey: true,
            key: "f",
          }),
        );
      });
    },
    async run(fn) {
      await act(async () => {
        await fn(state.current);
      });
    },
    async settleRelay() {
      await act(async () => {
        flushDebounce();
      });
      // Drain the stubbed invoke promise until the cache is idle...
      for (let turn = 0; turn < 30; turn++) {
        await act(async () => {
          await new Promise((resolve) => setImmediate(resolve));
        });
        if (queryClient.isFetching() === 0 && relayCalls.length > 0) {
          break;
        }
      }
      // ...then take one guaranteed render off the settled cache.
      await act(async () => {
        forceRender();
      });
    },
    async unmount() {
      await act(async () => {
        root.unmount();
      });
      queryClient.clear();
      liveQueryClients.delete(queryClient);
      container.remove();
    },
  };
}

// ── Tests ────────────────────────────────────────────────────────────────────
//
// Serial by construction: the jsdom window, the debounce timer registry and
// the relay stub are process-wide, so two find bars mounted at once would
// clear each other's pending debounce. `concurrency: 1` keeps one harness
// alive at a time, and `t.after` tears it down even when an assertion throws.

describe("useChannelFind", { concurrency: 1 }, () => {
  test("Ctrl+F opens the channel find bar", async (t) => {
    const harness = await mountChannelFind([message("m1", "alpha one")]);
    t.after(() => harness.unmount());

    assert.equal(harness.find.isOpen, false);
    await harness.openWithShortcut();
    assert.equal(harness.find.isOpen, true, "Ctrl+F must open the find bar");
  });

  test("a new query restarts navigation at the first match", async (t) => {
    const harness = await mountChannelFind([
      message("m1", "alpha one"),
      message("m2", "beta one"),
      message("m3", "alpha two"),
      message("m4", "beta two"),
      message("m5", "alpha three"),
      message("m6", "beta three"),
    ]);
    t.after(() => harness.unmount());

    await harness.openWithShortcut();
    await harness.run((find) => find.setQuery("alpha"));
    assert.equal(harness.find.matchCount, 3);

    await harness.run((find) => find.goToNext());
    await harness.run((find) => find.goToNext());
    assert.equal(harness.find.activeIndex, 2);
    assert.equal(harness.find.activeMatch.messageId, "m5");

    // Same match count, different term: the cursor must go home, not linger.
    await harness.run((find) => find.setQuery("beta"));
    assert.equal(harness.find.matchCount, 3);
    assert.equal(
      harness.find.activeIndex,
      0,
      "typing a new search term must reset the match cursor to the first hit",
    );
    assert.equal(
      harness.find.activeMatch.messageId,
      "m2",
      "a new query must land on its FIRST match, not the previous cursor position",
    );
  });

  test("relay hits for a superseded query are not counted as matches", async (t) => {
    relayHitsByQuery.clear();
    relayCalls.length = 0;
    relayHitsByQuery.set("alpha", [
      relayHit("old-1", "alpha from cold history"),
      relayHit("old-2", "alpha again, long ago"),
    ]);

    const harness = await mountChannelFind([
      message("m1", "alpha one"),
      message("m2", "unrelated"),
    ]);
    t.after(() => harness.unmount());

    await harness.openWithShortcut();
    await harness.run((find) => find.setQuery("alpha"));
    await harness.settleRelay();

    assert.equal(
      harness.find.matchCount,
      3,
      "one loaded message plus two relay hits",
    );

    // Keep typing. The client pass re-runs immediately for "alphazzz"; the
    // relay pass still holds results for "alpha" until the debounce fires.
    await harness.run((find) => find.setQuery("alphazzz"));

    assert.equal(
      harness.find.matchCount,
      0,
      "no message contains 'alphazzz' — the find bar must not report matches carried over from the previous query",
    );
    assert.equal(
      harness.find.activeMatch,
      null,
      "there is no active match to navigate to for a term with no results",
    );
  });

  test("matches survive once the relay catches up with the typed query", async (t) => {
    relayHitsByQuery.clear();
    relayCalls.length = 0;
    relayHitsByQuery.set("deploy", [
      relayHit("old-3", "deploy from cold history"),
    ]);

    const harness = await mountChannelFind([message("m1", "deploy one")]);
    t.after(() => harness.unmount());

    await harness.openWithShortcut();
    await harness.run((find) => find.setQuery("deploy"));
    await harness.settleRelay();

    assert.deepEqual(
      relayCalls.map((call) => call.q),
      ["deploy"],
      "the find bar scopes its relay query to the term the reader typed",
    );
    assert.equal(relayCalls[0].channelId, CHANNEL_ID);
    assert.equal(
      harness.find.matchCount,
      2,
      "the generation guard must not suppress relay hits for the current query",
    );
  });
});
