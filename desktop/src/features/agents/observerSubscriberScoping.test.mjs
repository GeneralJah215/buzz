/**
 * BUG-067 — the half of the BUG-065 renderer fix that was deliberately left.
 *
 * Commit 94e82ece1 made `notifyListeners` carry the changed agent key, but four
 * subscribers still registered zero-argument listeners, so 29 streaming agents
 * woke every one of them on every frame. These tests mount the REAL production
 * hooks against the REAL store and assert on call counts and rendered output.
 *
 * The dangerous failure here is SILENCE — a subscriber that filters on the key
 * and gets it wrong stops updating and nothing fails loudly. So every scoping
 * test comes in a pair: one that proves the consumer no longer wakes for
 * another agent, and one that proves it STILL wakes, and still renders new
 * content, for its own agent. A test that only proved "fewer wakeups" would
 * pass a subscriber that never wakes at all.
 *
 * Nothing here asserts on elapsed time.
 */

import assert from "node:assert/strict";
import { beforeEach, describe, it } from "node:test";

import { installDOMShim } from "@/shared/testing/reactDomShim.mjs";

installDOMShim();

import React from "react";
import { act } from "react";
import { createRoot } from "react-dom/client";

import {
  _testGetObserverListenerCount,
  _testGetObserverSubscribeCount,
  _testGetStoreReadCount,
  getLatestLiveSessionId,
  resetAgentObserverStore,
  subscribeAgentObserverStore,
  syncAgentObserverEvents,
} from "@/features/agents/observerRelayStore.ts";
import { subscribeAgentObserverStoreForAgent } from "@/features/agents/agentScopedObserverSubscription.ts";
import {
  useAgentTranscript,
  useObserverEvents,
} from "@/features/agents/ui/useObserverEvents.ts";
import {
  _testGetFeedScopeDerivationCount,
  _testResetProfileActivityFeedScopeCaches,
  useProfileActivityFeedScope,
} from "@/features/profile/lib/profileActivityFeedScope.ts";
import { resetActiveAgentTurnsStore } from "@/features/agents/activeAgentTurnsStore.ts";

const AGENT_A = "a".repeat(64);
const AGENT_B = "b".repeat(64);
const CHANNEL = "11111111-1111-1111-1111-111111111111";
const EPOCH = Date.UTC(2026, 0, 1, 0, 0, 0);

/**
 * One `turn_started` frame. Each distinct turnId produces exactly one
 * transcript item, so "did the consumer see new content" is a direct,
 * readable function of the frames pushed into the journal.
 */
function turnFrame(seq, overrides = {}) {
  return {
    seq,
    timestamp: new Date(EPOCH + seq * 1000).toISOString(),
    kind: "turn_started",
    agentIndex: 0,
    channelId: CHANNEL,
    sessionId: "sess-1",
    turnId: `t${seq}`,
    payload: { channel_id: CHANNEL },
    ...overrides,
  };
}

/**
 * Mount a component and return handles. `renders` counts committed renders of
 * the harness body, which is exactly "how often did this subscriber wake".
 */
function mount(renderBody) {
  const state = { renders: 0, last: null };

  function Harness(props) {
    state.renders += 1;
    state.last = renderBody(props);
    // Rendered through an attribute rather than a text child: the minimal DOM
    // shim has no textContent, and asserting on what actually reached the DOM
    // is the point — a subscriber that silently stops updating shows up here.
    return React.createElement("div", {
      "data-value": JSON.stringify(state.last),
    });
  }

  const container = document.createElement("div");
  const root = createRoot(container);
  const rerender = (props = {}) => {
    act(() => {
      root.render(React.createElement(Harness, props));
    });
  };
  rerender();
  return {
    state,
    container,
    rerender,
    /** What the DOM actually shows, parsed back from the rendered attribute. */
    rendered: () =>
      JSON.parse(container.children[0].getAttribute("data-value")),
    unmount: () => act(() => root.unmount()),
  };
}

/** Push frames into the store inside act() so React flushes synchronously. */
function stream(agentPubkey, seqs, overrides = {}) {
  act(() => {
    syncAgentObserverEvents(
      agentPubkey,
      seqs.map((seq) => turnFrame(seq, overrides)),
    );
  });
}

beforeEach(() => {
  resetAgentObserverStore();
  resetActiveAgentTurnsStore();
  _testResetProfileActivityFeedScopeCaches();
});

describe("subscribeAgentObserverStoreForAgent (BUG-067 filter contract)", () => {
  it("drops a keyed notification naming a different agent", () => {
    const seen = [];
    const unsubscribe = subscribeAgentObserverStoreForAgent(AGENT_A, () =>
      seen.push("woke"),
    );
    syncAgentObserverEvents(AGENT_B, [turnFrame(1)]);
    assert.deepEqual(seen, []);
    unsubscribe();
  });

  it("wakes for its own agent", () => {
    const seen = [];
    const unsubscribe = subscribeAgentObserverStoreForAgent(AGENT_A, () =>
      seen.push("woke"),
    );
    syncAgentObserverEvents(AGENT_A, [turnFrame(1)]);
    assert.deepEqual(seen, ["woke"]);
    unsubscribe();
  });

  it("always wakes for a store-wide (null key) change", () => {
    // This is the safety asymmetry the whole filter rests on: every mutation
    // whose blast radius exceeds one agent is emitted with a null key.
    const seen = [];
    const unsubscribe = subscribeAgentObserverStoreForAgent(AGENT_A, () =>
      seen.push("woke"),
    );
    resetAgentObserverStore();
    assert.deepEqual(seen, ["woke"]);
    unsubscribe();
  });

  it("keeps waking for everything when there is no agent to scope to", () => {
    const seen = [];
    const unsubscribe = subscribeAgentObserverStoreForAgent(null, () =>
      seen.push("woke"),
    );
    syncAgentObserverEvents(AGENT_B, [turnFrame(1)]);
    assert.deepEqual(seen, ["woke"]);
    unsubscribe();
  });

  it("normalizes the scoped pubkey before comparing", () => {
    const seen = [];
    const unsubscribe = subscribeAgentObserverStoreForAgent(
      AGENT_A.toUpperCase(),
      () => seen.push("woke"),
    );
    syncAgentObserverEvents(AGENT_A, [turnFrame(1)]);
    assert.deepEqual(seen, ["woke"]);
    unsubscribe();
  });
});

describe("useObserverEvents wakes only for its own agent", () => {
  it("is not woken at all when a different agent streams", () => {
    // Asserting on renders alone would be VACUOUS here: React bails inside
    // useSyncExternalStore when the snapshot is unchanged, so a fully
    // unfiltered subscriber also renders zero times. The store-read counter is
    // what actually distinguishes "never notified" from "notified and bailed",
    // and the per-frame getSnapshot call is the cost the fan-out imposed.
    const h = mount(() => useObserverEvents(true, AGENT_A).events.length);
    const beforeRenders = h.state.renders;
    const beforeReads = _testGetStoreReadCount();

    stream(AGENT_B, [1, 2, 3, 4, 5]);

    assert.equal(
      _testGetStoreReadCount(),
      beforeReads,
      "agent B's frames must not make agent A's panel re-read the store",
    );
    assert.equal(h.state.renders, beforeRenders);
    h.unmount();
  });

  it("STILL re-renders, with the new events, for its own agent", () => {
    // The dangerous failure is silence. This is the half that catches it.
    const h = mount(() => useObserverEvents(true, AGENT_A).events.length);
    const before = h.state.renders;

    stream(AGENT_A, [1, 2, 3]);

    assert.ok(
      h.state.renders > before,
      "agent A's own frames must still wake its panel",
    );
    assert.equal(h.state.last, 3);
    assert.equal(h.rendered(), 3);
    h.unmount();
  });

  it("re-renders on a store-wide change", () => {
    const h = mount(() => useObserverEvents(true, AGENT_A).connectionState);
    stream(AGENT_A, [1]);
    const before = h.state.renders;

    act(() => {
      resetAgentObserverStore();
    });

    assert.ok(h.state.renders > before);
    assert.equal(h.state.last, "idle");
    h.unmount();
  });
});

describe("useAgentTranscript wakes only for its own agent", () => {
  it("is not woken at all when a different agent streams", () => {
    const h = mount(() => useAgentTranscript(true, AGENT_A).length);
    const beforeRenders = h.state.renders;
    const beforeReads = _testGetStoreReadCount();

    stream(AGENT_B, [1, 2, 3]);

    assert.equal(_testGetStoreReadCount(), beforeReads);
    assert.equal(h.state.renders, beforeRenders);
    h.unmount();
  });

  it("STILL grows the rendered transcript for its own agent", () => {
    const h = mount(() => useAgentTranscript(true, AGENT_A).length);

    stream(AGENT_A, [1]);
    assert.equal(h.state.last, 1);
    stream(AGENT_A, [2]);
    assert.equal(h.state.last, 2);
    stream(AGENT_A, [3]);
    assert.equal(h.state.last, 3);
    assert.equal(h.rendered(), 3);

    h.unmount();
  });
});

describe("latest-live-session-id source is agent scoped", () => {
  // AgentSessionTranscriptList subscribes through the same helper to read
  // getLatestLiveSessionId. Its per-(agent, channel) entry can only be advanced
  // by appendAgentEvent for that agent, which is why the scoping is sound.
  it("another agent's frames cannot change this agent's latest live session", () => {
    syncAgentObserverEvents(AGENT_B, [turnFrame(1, { sessionId: "sess-b" })]);
    assert.equal(getLatestLiveSessionId(AGENT_A, CHANNEL), null);
  });
});

describe("subscribe callbacks are stable across renders", () => {
  /**
   * Mount, then render twice more to let React settle its passive effects, and
   * only then take the baseline. The baseline is asserted to be non-zero first
   * — otherwise "the subscribe count never moved" would also pass for a hook
   * that never subscribed at all.
   */
  function settledSubscribeBaseline(h) {
    h.rerender({ warm: 1 });
    h.rerender({ warm: 2 });
    const baseline = _testGetObserverSubscribeCount();
    assert.ok(baseline > 0, "the hook must actually subscribe to the store");
    return baseline;
  }

  it("useObserverEvents subscribes no more across ten further renders", () => {
    const h = mount(() => useObserverEvents(true, AGENT_A).events.length);
    const baseline = settledSubscribeBaseline(h);

    for (let i = 0; i < 10; i += 1) {
      h.rerender({ tick: i });
    }

    assert.equal(
      _testGetObserverSubscribeCount(),
      baseline,
      "a subscribe callback recreated per render resubscribes on every render",
    );
    h.unmount();
    assert.equal(_testGetObserverListenerCount(), 0);
  });

  it("useProfileActivityFeedScope subscribes no more across ten further renders", () => {
    // Before BUG-067 this hook passed an inline arrow to useSyncExternalStore,
    // so React tore down and re-established BOTH subscriptions every render.
    const agent = { pubkey: AGENT_A, name: "A", status: "running" };
    const turns = [];
    const h = mount(() => useProfileActivityFeedScope(agent, turns).isLive);
    const baseline = settledSubscribeBaseline(h);

    for (let i = 0; i < 10; i += 1) {
      h.rerender({ tick: i });
    }

    assert.equal(_testGetObserverSubscribeCount(), baseline);
    h.unmount();
    assert.equal(_testGetObserverListenerCount(), 0);
  });
});

describe("useProfileActivityFeedScope derivation is memoized and scoped", () => {
  it("is not woken at all when a different agent streams", () => {
    // The read counter tests the FILTER; the derivation counter tests the MEMO.
    // Both are needed: the memo alone would still leave this hook woken 29
    // times a frame, and the filter alone would still re-derive the whole
    // journal on every one of its own frames.
    const agent = { pubkey: AGENT_A, name: "A", status: "running" };
    const turns = [];
    const h = mount(() => useProfileActivityFeedScope(agent, turns).channelIds);
    const beforeDerivations = _testGetFeedScopeDerivationCount();
    const beforeReads = _testGetStoreReadCount();

    stream(AGENT_B, [1, 2, 3, 4, 5]);

    assert.equal(
      _testGetStoreReadCount(),
      beforeReads,
      "another agent's frames must not wake the profile feed at all",
    );
    assert.equal(
      _testGetFeedScopeDerivationCount(),
      beforeDerivations,
      "another agent's frames must not run the full feed-scope derivation",
    );
    h.unmount();
  });

  it("does not re-derive on a render that changed no store input", () => {
    const agent = { pubkey: AGENT_A, name: "A", status: "running" };
    const turns = [];
    const h = mount(() => useProfileActivityFeedScope(agent, turns).channelIds);
    stream(AGENT_A, [1]);
    const before = _testGetFeedScopeDerivationCount();

    for (let i = 0; i < 5; i += 1) {
      h.rerender({ tick: i });
    }

    assert.equal(_testGetFeedScopeDerivationCount(), before);
    h.unmount();
  });

  it("STILL picks up its own agent's channel, and returns a stable reference", () => {
    const agent = { pubkey: AGENT_A, name: "A", status: "running" };
    const turns = [];
    const scopes = [];
    const h = mount(() => {
      const scope = useProfileActivityFeedScope(agent, turns);
      scopes.push(scope);
      return scope.channelIds;
    });

    assert.deepEqual(h.state.last, []);
    stream(AGENT_A, [1]);
    assert.deepEqual(
      h.state.last,
      [CHANNEL],
      "the feed must still learn its own agent's channel",
    );
    assert.deepEqual(h.rendered(), [CHANNEL]);

    // getSnapshot must return a STABLE reference when nothing changed, or
    // useSyncExternalStore re-renders (and can loop) forever.
    const settled = scopes[scopes.length - 1];
    h.rerender({ tick: 1 });
    h.rerender({ tick: 2 });
    assert.equal(scopes[scopes.length - 1], settled);

    h.unmount();
  });

  it("keeps the same scope object when a frame changes nothing it reports", () => {
    // The memo only covers identical INPUT references. A frame the feed does
    // not care about still produces a new events array, so the value-equality
    // collapse in stableFeedScope is what stops a pointless re-render — and
    // what guarantees getSnapshot stays stable across a real store change.
    // A channel-less frame is exactly that: it adds no channel, no preferred
    // channel, and no per-channel activity timestamp.
    const agent = { pubkey: AGENT_A, name: "A", status: "running" };
    const turns = [];
    const h = mount(() => useProfileActivityFeedScope(agent, turns));

    stream(AGENT_A, [1], { channelId: null });
    const settled = h.state.last;
    const rendersAfterFirst = h.state.renders;

    stream(AGENT_A, [2], { channelId: null });
    stream(AGENT_A, [3], { channelId: null });

    assert.equal(
      h.state.last,
      settled,
      "a value-equal scope must keep its object identity",
    );
    assert.equal(h.state.renders, rendersAfterFirst);
    h.unmount();
  });
});

describe("the store still fans out to unfiltered listeners", () => {
  it("a zero-argument listener is unaffected by the scoped helper", () => {
    let woke = 0;
    const unsubscribe = subscribeAgentObserverStore(() => {
      woke += 1;
    });
    syncAgentObserverEvents(AGENT_A, [turnFrame(1)]);
    syncAgentObserverEvents(AGENT_B, [turnFrame(1)]);
    assert.equal(woke, 2);
    unsubscribe();
  });
});
