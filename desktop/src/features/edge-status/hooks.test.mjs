/**
 * Lifecycle tests for `useEdgeStatus`, mounting the real hook.
 *
 * The behaviours pinned here are the ones that decide whether an optional,
 * usually-absent sidecar costs the app anything:
 *   - the interval is CLEARED (not merely skipped) after the sidecar reports
 *     it is not running, so a machine without the feature does zero repeat IPC
 *   - an explicit `refresh()` brings polling back
 *   - unmount clears the timer
 *   - a hidden window never polls, and becoming visible catches up once
 *
 * `window.setInterval` / `window.clearInterval` are replaced with a recording
 * fake so "the timer was torn down" is asserted directly rather than inferred.
 */

import assert from "node:assert/strict";
import { afterEach, beforeEach, test } from "node:test";

import { JSDOM } from "jsdom";

const dom = new JSDOM("<!doctype html><html><body></body></html>", {
  pretendToBeVisual: true,
  url: "http://localhost",
});

// ── Recording interval fake ──────────────────────────────────────────────────

const activeIntervals = new Map();
let nextIntervalId = 1;

dom.window.setInterval = (fn, ms) => {
  const id = nextIntervalId++;
  activeIntervals.set(id, { fn, ms });
  return id;
};
dom.window.clearInterval = (id) => {
  activeIntervals.delete(id);
};

// ── Controllable visibility ──────────────────────────────────────────────────

let documentHidden = false;
Object.defineProperty(dom.window.document, "visibilityState", {
  configurable: true,
  get: () => (documentHidden ? "hidden" : "visible"),
});

function setHidden(hidden) {
  documentHidden = hidden;
  dom.window.document.dispatchEvent(
    new dom.window.Event("visibilitychange", { bubbles: false }),
  );
}

// ── Tauri IPC stub ───────────────────────────────────────────────────────────

let invokeHandler = () => {
  throw new Error("no invoke handler installed");
};
const invokeCalls = [];

dom.window.__TAURI_INTERNALS__ = {
  invoke: async (command, args) => {
    invokeCalls.push({ command, args });
    return invokeHandler(command, args);
  },
};

Object.assign(globalThis, {
  document: dom.window.document,
  Event: dom.window.Event,
  HTMLElement: dom.window.HTMLElement,
  IS_REACT_ACT_ENVIRONMENT: true,
  window: dom.window,
});

const React = (await import("react")).default;
const { act } = await import("react");
const { createRoot } = await import("react-dom/client");
const { useEdgeStatus, EDGE_STATUS_POLL_INTERVAL_MS } = await import(
  "./hooks.ts"
);

const SUMMARY = {
  pending: 2,
  claimed: 0,
  deliveredExact: 9,
  deliveredViaDigest: 1,
  quarantined: 0,
};
const WAITING = [
  {
    author: "44b8e82baaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    pending: 2,
    oldestPendingAt: 1_780_000_000,
  },
];

function healthyHandler(command) {
  if (command === "edge_delivery_summary") return { ...SUMMARY };
  if (command === "edge_waiting_authors") return WAITING.map((w) => ({ ...w }));
  throw new Error(`unexpected command ${command}`);
}

function rejectingHandler(message) {
  return () => {
    throw new Error(message);
  };
}

// ── Mount harness ────────────────────────────────────────────────────────────

let latest = null;
let root = null;
let container = null;

function Probe(props) {
  latest = useEdgeStatus(props.options);
  return null;
}

async function mount(options) {
  container = dom.window.document.createElement("div");
  dom.window.document.body.appendChild(container);
  root = createRoot(container);
  await act(async () => {
    root.render(React.createElement(Probe, { options }));
  });
}

async function unmount() {
  if (!root) return;
  const current = root;
  root = null;
  await act(async () => {
    current.unmount();
  });
  container?.remove();
  container = null;
}

async function tickAllIntervals() {
  const callbacks = [...activeIntervals.values()].map((entry) => entry.fn);
  await act(async () => {
    for (const fn of callbacks) {
      fn();
    }
  });
}

function countCommandCalls(command) {
  return invokeCalls.filter((call) => call.command === command).length;
}

beforeEach(() => {
  activeIntervals.clear();
  invokeCalls.length = 0;
  documentHidden = false;
  latest = null;
  invokeHandler = healthyHandler;
});

afterEach(async () => {
  await unmount();
});

// ── Happy path ───────────────────────────────────────────────────────────────

test("loads on mount and arms a single interval", async () => {
  await mount();

  assert.deepEqual(latest.summary, SUMMARY);
  assert.deepEqual(latest.waitingAuthors, WAITING);
  assert.equal(latest.unavailable, false);
  assert.equal(latest.error, null);
  assert.equal(activeIntervals.size, 1, "exactly one poll timer");
  assert.equal(
    [...activeIntervals.values()][0].ms,
    EDGE_STATUS_POLL_INTERVAL_MS,
  );
});

test("each tick re-reads both commands", async () => {
  await mount();
  const before = countCommandCalls("edge_delivery_summary");

  await tickAllIntervals();

  assert.equal(countCommandCalls("edge_delivery_summary"), before + 1);
  assert.equal(countCommandCalls("edge_waiting_authors"), before + 1);
});

// ── Sidecar not running ──────────────────────────────────────────────────────

test("stops polling after a sidecar-not-running rejection", async () => {
  invokeHandler = rejectingHandler("edge sidecar not running");
  await mount();

  assert.equal(latest.unavailable, true);
  assert.equal(latest.error, null, "an absent sidecar is not an error");
  assert.equal(latest.summary, null);
  assert.deepEqual(latest.waitingAuthors, []);
  assert.equal(
    activeIntervals.size,
    0,
    "the timer must be cleared, not left ticking against a dead process",
  );

  const callsAfterGivingUp = invokeCalls.length;
  await tickAllIntervals();
  assert.equal(
    invokeCalls.length,
    callsAfterGivingUp,
    "no further IPC once the sidecar is known absent",
  );
});

test("a community-binding rejection also goes quiet", async () => {
  invokeHandler = rejectingHandler("community binding does not hold");
  await mount();

  assert.equal(latest.unavailable, true);
  assert.equal(latest.error, null);
  assert.equal(activeIntervals.size, 0);
});

test("stale numbers are dropped when the sidecar disappears mid-session", async () => {
  await mount();
  assert.deepEqual(latest.summary, SUMMARY);

  invokeHandler = rejectingHandler("edge sidecar not running");
  await tickAllIntervals();

  assert.equal(latest.summary, null, "must not keep showing dead counters");
  assert.equal(latest.unavailable, true);
  assert.equal(activeIntervals.size, 0);
});

test("resumes polling on an explicit refresh", async () => {
  invokeHandler = rejectingHandler("edge sidecar not running");
  await mount();
  assert.equal(activeIntervals.size, 0);

  const callsWhileQuiet = invokeCalls.length;
  invokeHandler = healthyHandler;

  await act(async () => {
    latest.refresh();
  });

  assert.ok(
    invokeCalls.length > callsWhileQuiet,
    "refresh must fetch immediately",
  );
  assert.equal(latest.unavailable, false);
  assert.deepEqual(latest.summary, SUMMARY);
  assert.equal(activeIntervals.size, 1, "the interval must be re-armed");
});

test("refresh while already polling does not leak a second timer", async () => {
  await mount();
  await act(async () => {
    latest.refresh();
  });
  assert.equal(activeIntervals.size, 1);
});

// ── Genuine faults ───────────────────────────────────────────────────────────

test("a malformed response surfaces an error and keeps polling", async () => {
  invokeHandler = (command) =>
    command === "edge_delivery_summary" ? "garbage" : [];
  await mount();

  assert.ok(latest.error instanceof Error);
  assert.equal(latest.unavailable, false, "a bug is not an absent sidecar");
  assert.equal(
    activeIntervals.size,
    1,
    "a transient fault must not disarm polling",
  );
});

test("an unexpected rejection surfaces an error and keeps polling", async () => {
  invokeHandler = rejectingHandler("sqlite database is locked");
  await mount();

  assert.ok(latest.error instanceof Error);
  assert.match(latest.error.message, /sqlite/);
  assert.equal(latest.unavailable, false);
  assert.equal(activeIntervals.size, 1);
});

// ── Unmount ──────────────────────────────────────────────────────────────────

test("clears its timer on unmount", async () => {
  await mount();
  assert.equal(activeIntervals.size, 1);

  await unmount();

  assert.equal(activeIntervals.size, 0, "no timer may outlive the component");
});

test("a tick that survives unmount cannot write state back", async () => {
  await mount();
  const callbacks = [...activeIntervals.values()].map((entry) => entry.fn);
  const frozen = latest;

  invokeHandler = rejectingHandler("sqlite database is locked");
  await unmount();

  // Even if a stray reference to the tick survived teardown, the mounted-ref
  // guard must swallow the result rather than setState on a dead component.
  await act(async () => {
    for (const fn of callbacks) {
      fn();
    }
  });
  await act(async () => {});

  assert.equal(latest, frozen, "no re-render happened after unmount");
  assert.deepEqual(latest.summary, SUMMARY);
  assert.equal(latest.error, null);
});

// ── Visibility ───────────────────────────────────────────────────────────────

test("does not poll while the window is hidden", async () => {
  documentHidden = true;
  await mount();

  assert.equal(invokeCalls.length, 0, "no load on a hidden mount");
  assert.equal(
    activeIntervals.size,
    1,
    "the timer still exists, it just idles",
  );

  await tickAllIntervals();
  assert.equal(invokeCalls.length, 0, "ticks are skipped while hidden");
});

test("catches up once the window becomes visible again", async () => {
  documentHidden = true;
  await mount();
  assert.equal(invokeCalls.length, 0);

  await act(async () => {
    setHidden(false);
  });

  assert.equal(countCommandCalls("edge_delivery_summary"), 1);
  assert.deepEqual(latest.summary, SUMMARY);
});

// ── Disabled ─────────────────────────────────────────────────────────────────

test("enabled:false neither loads nor arms a timer", async () => {
  await mount({ enabled: false });

  assert.equal(invokeCalls.length, 0);
  assert.equal(activeIntervals.size, 0);
  assert.equal(latest.summary, null);
});
