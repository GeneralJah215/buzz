/**
 * The timeline half of SPEC-2026-08-05 acceptance item 18, asserted end to end
 * through the real IPC boundary: the real provider, the real placement rules in
 * `MessageDeliveryStatus`, the real `DeliveryStateBadge`, and the real
 * `invokeTauri` -> `@tauri-apps/api` -> `__TAURI_INTERNALS__.invoke` path. The
 * only fake is the Rust process at the far end of that call, which is the one
 * thing a node test cannot have.
 *
 * Four things are pinned, each of which has a way of quietly rotting:
 *
 *   1. **Off by default renders NOTHING.** `BUZZ_EDGE_RELAY_URL` is unset on
 *      every machine today, and then every command rejects with the exact
 *      `edge sidecar not running` sentinel. The assertion is on
 *      `container.innerHTML` being empty -- not on a stubbed hook returning an
 *      empty object, which would pass just as happily if the component grew an
 *      "unavailable" banner.
 *   2. **One request for N rows.** A 200-message timeline must issue ONE
 *      `edge_event_delivery_states` call carrying 200 ids, not 200 calls. The
 *      count comes from the recorded IPC log, so a regression to per-row
 *      fetching fails here rather than in a profiler six months later.
 *   3. **The badge is for stuck messages.** Present on a quarantined own
 *      message, absent on a synced one, absent on someone else's, absent while
 *      the row is still optimistic.
 *   4. **Over 500 ids are chunked, not truncated.** The sidecar answers 400 to
 *      an oversized batch, so a single 600-id call would blank every badge on
 *      screen -- and would do it only on the largest timelines.
 */

import assert from "node:assert/strict";
import { afterEach, beforeEach, test } from "node:test";

import { JSDOM } from "jsdom";

const dom = new JSDOM("<!doctype html><html><body></body></html>", {
  pretendToBeVisual: true,
  url: "http://localhost",
});

// Intervals are recorded rather than run: the provider arms one for its poll
// cadence and a real timer would make the request counts nondeterministic.
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

// Timeouts run on demand so the registration debounce can be flushed
// deterministically -- and so a test can prove that N registrations produced
// exactly ONE scheduled flush.
const pendingTimeouts = new Map();
let nextTimeoutId = 1;
dom.window.setTimeout = (fn) => {
  const id = nextTimeoutId++;
  pendingTimeouts.set(id, fn);
  return id;
};
dom.window.clearTimeout = (id) => {
  pendingTimeouts.delete(id);
};

// ── Tauri IPC stub, at the real boundary ─────────────────────────────────────

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
const { EdgeDeliveryStateProvider, MAX_DELIVERY_STATE_IDS } = await import(
  "./EdgeDeliveryStateProvider.tsx"
);
const { MessageDeliveryStatus } = await import(
  "./ui/MessageDeliveryStatus.tsx"
);
const { deliveryLabel } = await import("./lib/deliveryState.ts");

/** The exact string the Rust side returns when no sidecar is reachable. */
const UNAVAILABLE = "edge sidecar not running";

function eventId(index) {
  return index.toString(16).padStart(64, "0");
}

/** Answers delivery-state lookups from a map; rejects anything else loudly. */
function statesHandler(statesById) {
  return (command, args) => {
    if (command !== "edge_event_delivery_states") {
      throw new Error(`unexpected command ${command}`);
    }
    return args.eventIds
      .filter((id) => id in statesById)
      .map((id) => ({
        eventId: id,
        state: statesById[id].state,
        carriedByDigest: statesById[id].carriedByDigest ?? false,
        demotionReason: statesById[id].demotionReason ?? null,
      }));
  };
}

let root = null;
let container = null;

async function flushDebounce() {
  const callbacks = [...pendingTimeouts.values()];
  pendingTimeouts.clear();
  await act(async () => {
    for (const callback of callbacks) {
      callback();
    }
  });
}

function rowTree(rows) {
  return React.createElement(
    EdgeDeliveryStateProvider,
    null,
    rows.map((row) =>
      React.createElement(MessageDeliveryStatus, {
        eventId: row.eventId,
        isOwnMessage: row.isOwnMessage ?? true,
        isPending: row.isPending ?? false,
        key: row.eventId,
      }),
    ),
  );
}

/** Mount rows, let the debounce fire, and let the resulting fetch settle. */
async function renderRows(rows) {
  container = dom.window.document.createElement("div");
  dom.window.document.body.appendChild(container);
  root = createRoot(container);
  await act(async () => {
    root.render(rowTree(rows));
  });
  await flushDebounce();
  // The fetch is kicked off from an effect that runs after the debounce commit.
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
  });
}

/** Swap the rendered window, as a virtualized scroll step does. */
async function rerenderRows(rows) {
  await act(async () => {
    root.render(rowTree(rows));
  });
  await flushDebounce();
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
  });
}

/** Runs `body` with `console.warn` captured, and hands back what it logged. */
async function captureWarnings(body) {
  const warnings = [];
  const original = console.warn;
  console.warn = (...args) => warnings.push(args);
  try {
    await body();
  } finally {
    console.warn = original;
  }
  return warnings;
}

beforeEach(() => {
  invokeCalls.length = 0;
  activeIntervals.clear();
  pendingTimeouts.clear();
  invokeHandler = () => {
    throw new Error("no invoke handler installed");
  };
});

afterEach(async () => {
  if (root) {
    const current = root;
    await act(async () => {
      current.unmount();
    });
    root = null;
  }
  container?.remove();
  container = null;
});

test("a machine with no edge sidecar renders nothing at all", async () => {
  invokeHandler = () => {
    throw new Error(UNAVAILABLE);
  };

  await renderRows([
    { eventId: eventId(1) },
    { eventId: eventId(2) },
    { eventId: eventId(3) },
  ]);

  // Real rendered output, not a mocked lookup: no badge, no empty state, no
  // "unavailable" note, no wrapper element left behind to shift the layout.
  assert.equal(container.innerHTML, "");
  assert.equal(container.textContent, "");
  assert.equal(container.childElementCount, 0);
});

test("an absent sidecar drops to the slow cadence instead of hammering IPC", async () => {
  invokeHandler = () => {
    throw new Error(UNAVAILABLE);
  };

  await renderRows([{ eventId: eventId(1) }]);

  assert.equal(invokeCalls.length, 1);
  const cadences = [...activeIntervals.values()].map((entry) => entry.ms);
  assert.ok(
    cadences.every((ms) => ms >= 120_000),
    `expected the unavailable retry cadence, got ${cadences.join(", ")}`,
  );
});

test("a timeline of 200 messages issues ONE delivery-state request", async () => {
  const ids = Array.from({ length: 200 }, (_, index) => eventId(index + 1));
  invokeHandler = statesHandler({});

  await renderRows(ids.map((id) => ({ eventId: id })));

  const stateCalls = invokeCalls.filter(
    (call) => call.command === "edge_event_delivery_states",
  );
  assert.equal(
    stateCalls.length,
    1,
    `expected one batched call, got ${stateCalls.length}`,
  );
  assert.equal(stateCalls[0].args.eventIds.length, 200);
  assert.deepEqual([...stateCalls[0].args.eventIds].sort(), [...ids].sort());
});

test("more ids than the sidecar's batch limit are chunked, never truncated", async () => {
  const total = MAX_DELIVERY_STATE_IDS + 100;
  const ids = Array.from({ length: total }, (_, index) => eventId(index + 1));
  invokeHandler = statesHandler({});

  await renderRows(ids.map((id) => ({ eventId: id })));

  const stateCalls = invokeCalls.filter(
    (call) => call.command === "edge_event_delivery_states",
  );
  assert.equal(stateCalls.length, 2);
  for (const call of stateCalls) {
    assert.ok(
      call.args.eventIds.length <= MAX_DELIVERY_STATE_IDS,
      `a chunk of ${call.args.eventIds.length} would be refused with 400`,
    );
  }
  const requested = stateCalls.flatMap((call) => call.args.eventIds);
  assert.equal(
    new Set(requested).size,
    total,
    "every id must still be asked about",
  );
});

test("a stuck own message is badged and a synced one is not", async () => {
  const stuck = eventId(1);
  const syncedExact = eventId(2);
  const syncedViaDigest = eventId(3);
  invokeHandler = statesHandler({
    [stuck]: { state: "quarantined", demotionReason: "rejected upstream" },
    [syncedExact]: { state: "syncedExact" },
    [syncedViaDigest]: { state: "syncedViaDigest" },
  });

  await renderRows([
    { eventId: stuck },
    { eventId: syncedExact },
    { eventId: syncedViaDigest },
  ]);

  const badges = [
    ...container.querySelectorAll('[data-testid="message-delivery-state"]'),
  ];
  assert.equal(badges.length, 1, "only the stuck message should carry a badge");
  assert.equal(badges[0].textContent, deliveryLabel("quarantined"));
  // The demotion reason is the actionable half and must reach the tooltip.
  assert.match(badges[0].getAttribute("title"), /rejected upstream/);
});

/**
 * BUG-023, through the same seam the operator sees: sidecar reply -> parser ->
 * lookup entry -> `MessageDeliveryStatus` -> badge. Both rows are
 * `quarantined`, so the label alone cannot separate them; only the flag can,
 * and it has to survive every hop. The quarantine list already draws this
 * distinction, and the badge disagreeing with it about one row is the bug.
 */
test("a quarantined row the digest is carrying is badged differently", async () => {
  const stuck = eventId(1);
  const carried = eventId(2);
  invokeHandler = statesHandler({
    [stuck]: { state: "quarantined", demotionReason: null },
    [carried]: {
      state: "quarantined",
      carriedByDigest: true,
      demotionReason: "permanently rejected upstream",
    },
  });

  await renderRows([{ eventId: stuck }, { eventId: carried }]);

  const badges = [
    ...container.querySelectorAll('[data-testid="message-delivery-state"]'),
  ];
  assert.equal(badges.length, 2, "neither row has reached canonical history");
  assert.equal(badges[0].textContent, deliveryLabel("quarantined"));
  assert.equal(badges[1].textContent, deliveryLabel("quarantined", true));
  assert.notEqual(
    badges[1].textContent,
    badges[0].textContent,
    "the flag never reached the badge: one row, two surfaces, two answers",
  );
  assert.match(
    badges[1].getAttribute("title"),
    /nothing to retry/i,
    "and the hover copy has to say there is nothing to do",
  );
});

test("every not-yet-synced state is badged with its own label", async () => {
  const pending = eventId(1);
  const viaDigest = eventId(2);
  const claimed = eventId(3);
  invokeHandler = statesHandler({
    [pending]: { state: "pending" },
    [viaDigest]: { state: "pendingViaDigest" },
    [claimed]: { state: "claimed" },
  });

  await renderRows([
    { eventId: pending },
    { eventId: viaDigest },
    { eventId: claimed },
  ]);

  const labels = [
    ...container.querySelectorAll('[data-testid="message-delivery-state"]'),
  ].map((node) => node.textContent);
  assert.deepEqual(labels, [
    deliveryLabel("pending"),
    deliveryLabel("pendingViaDigest"),
    deliveryLabel("claimed"),
  ]);
  // The three are distinct strings. `pendingViaDigest` has no author coming for
  // it and `pending` does; rendering them the same word would leave the
  // operator with a queue nothing on screen explains.
  assert.equal(new Set(labels).size, 3);
  // And none of them collapses the two axes into "sent"/"delivered" alone.
  assert.equal(labels[0], "Delivered locally");
});

test("other people's messages and un-acked sends are never asked about", async () => {
  const mine = eventId(1);
  const theirs = eventId(2);
  const optimistic = eventId(3);
  invokeHandler = statesHandler({
    [mine]: { state: "pending" },
    [theirs]: { state: "pending" },
    [optimistic]: { state: "pending" },
  });

  await renderRows([
    { eventId: mine, isOwnMessage: true },
    { eventId: theirs, isOwnMessage: false },
    { eventId: optimistic, isOwnMessage: true, isPending: true },
  ]);

  const stateCalls = invokeCalls.filter(
    (call) => call.command === "edge_event_delivery_states",
  );
  assert.deepEqual(stateCalls[0].args.eventIds, [mine]);
  const badges = [
    ...container.querySelectorAll('[data-testid="message-delivery-state"]'),
  ];
  assert.equal(badges.length, 1);
});

test("one failed chunk does not discard the chunks that answered", async () => {
  // 501 ids is two chunks. Under `Promise.all` a 503 on the second one threw
  // away 500 good entries from the first, so a single flaky chunk blanked every
  // badge on a long timeline -- and a stuck message showing nothing reads
  // exactly like a message that arrived.
  const total = MAX_DELIVERY_STATE_IDS + 1;
  const ids = Array.from({ length: total }, (_, index) => eventId(index + 1));
  const lastId = ids[total - 1];
  invokeHandler = (command, args) => {
    if (command !== "edge_event_delivery_states") {
      throw new Error(`unexpected command ${command}`);
    }
    if (args.eventIds.includes(lastId)) {
      throw new Error("relay returned 503 Service Unavailable");
    }
    return args.eventIds.map((id) => ({
      eventId: id,
      state: "quarantined",
      carriedByDigest: false,
      demotionReason: null,
    }));
  };

  const warnings = await captureWarnings(() =>
    renderRows(ids.map((id) => ({ eventId: id }))),
  );

  const badges = [
    ...container.querySelectorAll('[data-testid="message-delivery-state"]'),
  ];
  assert.equal(
    badges.length,
    MAX_DELIVERY_STATE_IDS,
    `every id the sidecar DID answer for must still be badged, got ${badges.length}`,
  );
  // A genuine fault keeps the fast cadence so recovery is picked up promptly.
  const cadences = [...activeIntervals.values()].map((entry) => entry.ms);
  assert.ok(
    cadences.every((ms) => ms < 120_000),
    `a genuine fault must not be treated as 'no sidecar', got ${cadences.join(", ")}`,
  );
  // The badge has no error UI by design, so the log is the ONLY trace this
  // fault leaves anywhere. Silence here is how a repeated fault stays invisible.
  const guardrail = warnings.find(
    (args) =>
      typeof args[0] === "string" &&
      args[0].startsWith("[GUARDRAIL]") &&
      args[0].includes("edge_event_delivery_states"),
  );
  assert.ok(guardrail, "a genuine delivery-state fault must be logged");
  assert.match(String(guardrail[1]), /503 Service Unavailable/);
});

test("a whole batch of sentinel rejections is still the benign case", async () => {
  // Two chunks, both "not running". That is not a fault and must render
  // nothing at all, exactly as the single-chunk case does.
  const total = MAX_DELIVERY_STATE_IDS + 1;
  const ids = Array.from({ length: total }, (_, index) => eventId(index + 1));
  invokeHandler = () => {
    throw new Error(UNAVAILABLE);
  };

  const warnings = await captureWarnings(() =>
    renderRows(ids.map((id) => ({ eventId: id }))),
  );

  assert.equal(container.innerHTML, "");
  assert.equal(warnings.length, 0, "an absent sidecar is not a fault to log");
  const cadences = [...activeIntervals.values()].map((entry) => entry.ms);
  assert.ok(cadences.every((ms) => ms >= 120_000));
});

test("an absent sidecar's backoff survives id-set churn", async () => {
  // The cadence guard used to cover only the `unavailable` flip. Every id-set
  // change still forced an immediate poll, so scrolling a virtualized timeline
  // on a sidecar-free machine paid a rejected IPC call per debounce window
  // while the retry timer sat armed at two minutes.
  invokeHandler = () => {
    throw new Error(UNAVAILABLE);
  };

  await renderRows([{ eventId: eventId(1) }]);
  assert.equal(invokeCalls.length, 1);

  for (let step = 2; step <= 11; step += 1) {
    await rerenderRows([{ eventId: eventId(step) }, { eventId: eventId(1) }]);
  }

  assert.equal(
    invokeCalls.length,
    1,
    `ten scroll steps must not punch through the backoff, got ${invokeCalls.length} calls`,
  );
});

test("unmounting schedules no flush the provider will never run", async () => {
  invokeHandler = () => {
    throw new Error(UNAVAILABLE);
  };
  await renderRows([{ eventId: eventId(1) }, { eventId: eventId(2) }]);
  assert.equal(pendingTimeouts.size, 0, "the debounce should have flushed");

  const current = root;
  root = null;
  await act(async () => {
    current.unmount();
  });

  // Row cleanups run after the provider's own, so `unregister -> scheduleFlush`
  // used to arm one timer per teardown whose callback could only be a no-op.
  assert.equal(
    pendingTimeouts.size,
    0,
    "a torn-down provider must not leave a timer behind",
  );
});

test("a state this build predates renders no badge rather than a broken one", async () => {
  const unknown = eventId(1);
  invokeHandler = () => [
    {
      eventId: unknown,
      state: "teleported",
      carriedByDigest: false,
      demotionReason: null,
    },
  ];

  await renderRows([{ eventId: unknown }]);

  assert.equal(container.innerHTML, "");
});
