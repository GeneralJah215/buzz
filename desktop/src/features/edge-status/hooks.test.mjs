/**
 * Lifecycle tests for `useEdgeStatus`, mounting the real hook.
 *
 * The behaviours pinned here are the ones that decide whether an optional,
 * usually-absent sidecar costs the app anything — and whether a sidecar that
 * comes back is ever noticed:
 *   - an absent sidecar drops the fast timer and re-arms a slow one, so a
 *     machine without the feature does almost no repeat IPC, and a restarted
 *     sidecar is picked up without the operator doing anything
 *   - a real fault (503, binding mismatch, SQLite) is an error, not silence
 *   - an explicit `refresh()` polls now, even mid-poll
 *   - unmount clears the timer AND stops any surviving callback doing IPC
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
const {
  useEdgeStatus,
  EDGE_STATUS_POLL_INTERVAL_MS,
  EDGE_UNAVAILABLE_RETRY_INTERVAL_MS,
} = await import("./hooks.ts");

const SUMMARY = {
  pending: 2,
  pendingViaDigest: 4,
  claimed: 0,
  syncedExact: 9,
  syncedViaDigest: 1,
  quarantined: 0,
};
const WAITING = [
  {
    author: "44b8e82baaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    pending: 2,
    ancestorBlocked: 0,
    pendingViaDigest: 4,
    oldestPendingAt: 1_780_000_000,
    oldestClaimableAt: 1_780_000_400,
    oldestAncestorBlockedAt: null,
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

test("an absent sidecar backs off to the slow cadence instead of the fast one", async () => {
  invokeHandler = rejectingHandler("edge sidecar not running");
  await mount();

  assert.equal(latest.unavailable, true);
  assert.equal(latest.error, null, "an absent sidecar is not an error");
  assert.equal(latest.summary, null);
  assert.deepEqual(latest.waitingAuthors, []);

  assert.equal(activeIntervals.size, 1, "exactly one timer, not two");
  assert.equal(
    [...activeIntervals.values()][0].ms,
    EDGE_UNAVAILABLE_RETRY_INTERVAL_MS,
    "the fast timer must be torn down and a slow one armed in its place",
  );
  assert.ok(
    EDGE_UNAVAILABLE_RETRY_INTERVAL_MS > EDGE_STATUS_POLL_INTERVAL_MS * 4,
    "the retry cadence must actually be a backoff",
  );

  const callsAfterGivingUp = invokeCalls.length;
  await act(async () => {});
  assert.equal(
    invokeCalls.length,
    callsAfterGivingUp,
    "re-arming the timer must not immediately re-poll a dead process",
  );
});

/**
 * The point of the backoff. A sidecar restart is the common case; before this,
 * one rejection killed the surface for the rest of the session and nothing
 * ever called refresh().
 */
test("a restarted sidecar is picked up by the slow retry, with no user action", async () => {
  invokeHandler = rejectingHandler("edge sidecar not running");
  await mount();
  assert.equal(latest.unavailable, true);

  invokeHandler = healthyHandler;
  await tickAllIntervals();

  assert.equal(latest.unavailable, false, "recovered on its own");
  assert.deepEqual(latest.summary, SUMMARY);
  assert.equal(activeIntervals.size, 1);
  assert.equal(
    [...activeIntervals.values()][0].ms,
    EDGE_STATUS_POLL_INTERVAL_MS,
    "and the fast cadence comes back",
  );
});

/**
 * Inverted on purpose (was "a community-binding rejection also goes quiet").
 * A binding mismatch is a real fault: the sidecar is right there, answering,
 * and refusing. Reporting it as "no sidecar installed" hid it completely.
 */
test("a community-binding rejection is a real error, not silence", async () => {
  invokeHandler = rejectingHandler(
    "relay returned 421 Misdirected Request: canonical relay/community binding mismatch",
  );
  await mount();

  assert.equal(latest.unavailable, false, "the sidecar is not absent");
  assert.ok(latest.error instanceof Error, "the operator must see this");
  assert.match(latest.error.message, /binding mismatch/);
  assert.equal(
    [...activeIntervals.values()][0].ms,
    EDGE_STATUS_POLL_INTERVAL_MS,
    "a fault does not slow the cadence",
  );
});

test("a 503 from the relay is a real error, not silence", async () => {
  invokeHandler = rejectingHandler("relay returned 503 Service Unavailable");
  await mount();

  assert.equal(latest.unavailable, false);
  assert.ok(latest.error instanceof Error);
  assert.match(latest.error.message, /503/);
  assert.equal(activeIntervals.size, 1);
});

test("stale numbers are dropped when the sidecar disappears mid-session", async () => {
  await mount();
  assert.deepEqual(latest.summary, SUMMARY);

  invokeHandler = rejectingHandler("edge sidecar not running");
  await tickAllIntervals();

  assert.equal(latest.summary, null, "must not keep showing dead counters");
  assert.equal(latest.unavailable, true);
  assert.equal(
    [...activeIntervals.values()][0].ms,
    EDGE_UNAVAILABLE_RETRY_INTERVAL_MS,
  );
});

test("resumes polling on an explicit refresh", async () => {
  invokeHandler = rejectingHandler("edge sidecar not running");
  await mount();
  assert.equal(
    [...activeIntervals.values()][0].ms,
    EDGE_UNAVAILABLE_RETRY_INTERVAL_MS,
  );

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
  assert.equal(
    [...activeIntervals.values()][0].ms,
    EDGE_STATUS_POLL_INTERVAL_MS,
    "back on the fast cadence",
  );
});

test("refresh while already polling does not leak a second timer", async () => {
  await mount();
  await act(async () => {
    latest.refresh();
  });
  assert.equal(activeIntervals.size, 1);
});

/**
 * A refresh that lands while a poll is in flight used to be dropped on the
 * floor by the in-flight latch: the button did nothing, and the numbers on
 * screen were the ones fetched BEFORE the user asked.
 */
test("a refresh during an in-flight poll is honoured, not swallowed", async () => {
  const gate = { resolve: null };
  let served = 0;
  invokeHandler = (command) => {
    served += 1;
    // Hold the very first summary call open until the test releases it.
    if (command === "edge_delivery_summary" && served === 1) {
      return new Promise((resolve) => {
        gate.resolve = () => resolve({ ...SUMMARY });
      });
    }
    return healthyHandler(command);
  };

  await mount();
  assert.equal(latest.summary, null, "the first poll is still in flight");

  await act(async () => {
    latest.refresh();
  });
  const callsBeforeRelease = countCommandCalls("edge_delivery_summary");

  await act(async () => {
    gate.resolve();
  });
  await act(async () => {});

  assert.equal(
    countCommandCalls("edge_delivery_summary"),
    callsBeforeRelease + 1,
    "the queued refresh must run once the in-flight poll settles",
  );
  assert.deepEqual(latest.summary, SUMMARY);
});

test("an interval tick during an in-flight poll is coalesced, not queued", async () => {
  // The opposite of the case above: a periodic tick that lands mid-poll is
  // redundant by definition and must not double the IPC.
  const gate = { resolve: null };
  let served = 0;
  invokeHandler = (command) => {
    served += 1;
    if (command === "edge_delivery_summary" && served === 1) {
      return new Promise((resolve) => {
        gate.resolve = () => resolve({ ...SUMMARY });
      });
    }
    return healthyHandler(command);
  };

  await mount();
  await tickAllIntervals();
  const callsBeforeRelease = countCommandCalls("edge_delivery_summary");

  await act(async () => {
    gate.resolve();
  });
  await act(async () => {});

  assert.equal(
    countCommandCalls("edge_delivery_summary"),
    callsBeforeRelease,
    "no catch-up poll for a tick that was already covered",
  );
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

/**
 * Replaces a test that was provably vacuous: it asserted "no re-render
 * happened after unmount" through a harness that only records state DURING a
 * render, so an unmounted root could never have failed it — it passed with the
 * mounted-ref guard deleted entirely.
 *
 * This asserts something a dead guard genuinely breaks: a surviving callback
 * must not reach the sidecar at all. The stub throws on any call, so the tick
 * either issues zero IPC or the recorded call list grows.
 */
test("a callback that survives unmount issues no IPC", async () => {
  await mount();
  const callbacks = [...activeIntervals.values()].map((entry) => entry.fn);
  const callsBeforeUnmount = invokeCalls.length;

  await unmount();

  invokeHandler = () => {
    throw new Error("no command may be issued from an unmounted hook");
  };
  await act(async () => {
    for (const fn of callbacks) {
      fn();
    }
  });
  await act(async () => {});

  assert.equal(
    invokeCalls.length,
    callsBeforeUnmount,
    "a dead component must not talk to the sidecar",
  );
});

/**
 * The other half: a poll already in flight when the component goes away must
 * not throw on the way out (an unhandled rejection here would surface as a
 * crash in dev and a silent error in prod).
 */
test("a poll still in flight at unmount settles quietly", async () => {
  const gate = { reject: null };
  let served = 0;
  invokeHandler = (command) => {
    served += 1;
    if (command === "edge_delivery_summary" && served === 1) {
      return new Promise((_resolve, reject) => {
        gate.reject = () => reject(new Error("sqlite database is locked"));
      });
    }
    return healthyHandler(command);
  };

  await mount();
  await unmount();

  await act(async () => {
    gate.reject();
  });
  await act(async () => {});
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
