/**
 * BUG-067 — `usePreventSleep` was the fourth zero-argument observer subscriber.
 *
 * It is the one subscriber that genuinely AGGREGATES across agents, so it is
 * deliberately NOT filtered down to a single agent. Two things changed instead:
 *
 *  1. A keyed notification now re-reads only that agent's snapshot, and a key
 *     naming an agent this hook does not track is dropped entirely.
 *  2. `setPreventSleepActive` — a Tauri IPC — was fired every time the newest
 *     event key changed, i.e. every frame for a busy agent. It is now throttled
 *     to `ACTIVITY_REFRESH_INTERVAL_MS`, which is 120x tighter than the one-hour
 *     inactivity cap the Rust side re-arms on each call.
 *
 * The dangerous failure is silence — an inhibitor that never re-arms and lets
 * the machine sleep mid-turn — so every "fires less" assertion is paired with
 * one proving it still fires, for each tracked agent, once the window elapses.
 *
 * Assertions are on IPC call counts and a stubbed clock. Nothing here measures
 * elapsed time.
 */

import assert from "node:assert/strict";
import { afterEach, beforeEach, describe, it } from "node:test";

import { installDOMShim } from "@/shared/testing/reactDomShim.mjs";

installDOMShim();

// localStorage shim — usePreventSleep reads the "buzz-prevent-sleep" preference
// synchronously during its first render.
const storage = new Map();
globalThis.localStorage = {
  getItem: (key) => (storage.has(key) ? storage.get(key) : null),
  setItem: (key, value) => storage.set(key, String(value)),
  removeItem: (key) => storage.delete(key),
  clear: () => storage.clear(),
};
globalThis.window.localStorage = globalThis.localStorage;

/** @type {Array<{ cmd: string, args: unknown }>} */
let ipcCalls = [];

globalThis.__TAURI_INTERNALS__ = {
  invoke: (cmd, args) => {
    ipcCalls.push({ cmd, args });
    return Promise.resolve(null);
  },
  transformCallback: () => Math.random(),
};

// `listen("prevent-sleep-expired")` resolves to an unlisten fn that calls into
// the event plugin's internals on unmount. Without this stub every unmount
// rejects asynchronously after its test has ended.
globalThis.__TAURI_EVENT_PLUGIN_INTERNALS__ = {
  unregisterListener: () => Promise.resolve(),
};

import React from "react";
import { act } from "react";
import { createRoot } from "react-dom/client";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

import { managedAgentsQueryKey } from "@/features/agents/hooks.ts";
import { PreventSleepProvider } from "@/features/agents/usePreventSleep.ts";
import {
  resetAgentObserverStore,
  syncAgentObserverEvents,
} from "@/features/agents/observerRelayStore.ts";

const AGENT_A = "a".repeat(64);
const AGENT_B = "b".repeat(64);
const UNTRACKED_AGENT = "c".repeat(64);
const CHANNEL = "11111111-1111-1111-1111-111111111111";
const EPOCH = Date.UTC(2026, 0, 1, 0, 0, 0);

// Must match ACTIVITY_REFRESH_INTERVAL_MS in usePreventSleep.ts.
const THROTTLE_MS = 30_000;

function turnFrame(seq) {
  return {
    seq,
    timestamp: new Date(EPOCH + seq * 1000).toISOString(),
    kind: "turn_started",
    agentIndex: 0,
    channelId: CHANNEL,
    sessionId: "sess-1",
    turnId: `t${seq}`,
    payload: { channel_id: CHANNEL },
  };
}

/** Every set_prevent_sleep_active call made so far. */
function preventSleepCalls() {
  return ipcCalls.filter((call) => call.cmd === "set_prevent_sleep_active");
}

const realDateNow = Date.now;
let fakeNow = 1_000_000;

function advanceClock(millis) {
  fakeNow += millis;
}

function mountProvider(agents) {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false, refetchInterval: false } },
  });
  queryClient.setQueryData(managedAgentsQueryKey, agents);

  const container = document.createElement("div");
  const root = createRoot(container);
  act(() => {
    root.render(
      React.createElement(
        QueryClientProvider,
        { client: queryClient },
        React.createElement(PreventSleepProvider, null, null),
      ),
    );
  });
  return { unmount: () => act(() => root.unmount()) };
}

/** Push one frame for one agent and let React flush. */
function stream(agentPubkey, seq) {
  act(() => {
    syncAgentObserverEvents(agentPubkey, [turnFrame(seq)]);
  });
}

beforeEach(() => {
  resetAgentObserverStore();
  ipcCalls = [];
  storage.clear();
  storage.set("buzz-prevent-sleep", "true");
  fakeNow = 1_000_000;
  Date.now = () => fakeNow;
});

afterEach(() => {
  Date.now = realDateNow;
});

describe("usePreventSleep throttles its activity IPC", () => {
  it("does not fire once per frame while an agent streams", () => {
    const h = mountProvider([{ pubkey: AGENT_A, status: "running" }]);
    const before = preventSleepCalls().length;

    // 60 frames inside one throttle window. Pre-BUG-067 this was 59 IPC calls
    // (the first frame only seeds the tracker).
    for (let seq = 1; seq <= 60; seq += 1) {
      stream(AGENT_A, seq);
    }

    const fired = preventSleepCalls().length - before;
    assert.equal(
      fired,
      1,
      "an unthrottled activity path fires one IPC per frame",
    );
    h.unmount();
  });

  it("STILL re-arms after the throttle window elapses", () => {
    // The silence guard. A throttle that never reopens would pass the test
    // above and let the machine sleep mid-turn.
    const h = mountProvider([{ pubkey: AGENT_A, status: "running" }]);
    for (let seq = 1; seq <= 5; seq += 1) {
      stream(AGENT_A, seq);
    }
    const after = preventSleepCalls().length;

    advanceClock(THROTTLE_MS + 1);
    stream(AGENT_A, 6);

    assert.equal(
      preventSleepCalls().length,
      after + 1,
      "the inhibitor must re-arm once the window reopens",
    );
    assert.deepEqual(preventSleepCalls().at(-1).args, { active: true });
    h.unmount();
  });
});

describe("usePreventSleep keeps aggregating across every running agent", () => {
  it("STILL sees activity from the second running agent", () => {
    // This is the test that fails if the per-agent narrowing filters out an
    // agent it should have tracked.
    const h = mountProvider([
      { pubkey: AGENT_A, status: "running" },
      { pubkey: AGENT_B, status: "running" },
    ]);

    // Seed both trackers, then burn the window on A.
    stream(AGENT_A, 1);
    stream(AGENT_B, 1);
    stream(AGENT_A, 2);
    const after = preventSleepCalls().length;

    advanceClock(THROTTLE_MS + 1);
    stream(AGENT_B, 2);

    assert.equal(
      preventSleepCalls().length,
      after + 1,
      "agent B's frames must still count as activity",
    );
    h.unmount();
  });

  it("ignores an agent that is not in the running set", () => {
    const h = mountProvider([{ pubkey: AGENT_A, status: "running" }]);
    stream(AGENT_A, 1);
    const after = preventSleepCalls().length;

    advanceClock(THROTTLE_MS + 1);
    stream(UNTRACKED_AGENT, 1);
    stream(UNTRACKED_AGENT, 2);
    stream(UNTRACKED_AGENT, 3);

    assert.equal(
      preventSleepCalls().length,
      after,
      "a stopped or unknown agent's frames must not hold the machine awake",
    );
    h.unmount();
  });

  it("tracks each running agent independently", () => {
    // Narrowing to one agent per wakeup must not let one agent's tracker entry
    // stand in for another's: each agent has to be able to re-arm on its own.
    const h = mountProvider([
      { pubkey: AGENT_A, status: "running" },
      { pubkey: AGENT_B, status: "running" },
    ]);
    stream(AGENT_A, 1);
    stream(AGENT_B, 1);
    const after = preventSleepCalls().length;

    advanceClock(THROTTLE_MS + 1);
    stream(AGENT_B, 2);
    assert.equal(preventSleepCalls().length, after + 1, "B must re-arm");

    advanceClock(THROTTLE_MS + 1);
    stream(AGENT_A, 2);
    assert.equal(preventSleepCalls().length, after + 2, "A must re-arm");

    h.unmount();
  });
});
