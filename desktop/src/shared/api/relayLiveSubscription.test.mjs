import assert from "node:assert/strict";
import test from "node:test";

// Shim `window` for the ready-fallback timer. The real client runs in a Tauri
// WebView where `window` exists; under node:test we wire it to the same
// globals, matching relayStallWatchdog.test.mjs.
if (typeof globalThis.window === "undefined") {
  globalThis.window = {
    setTimeout: (...args) => setTimeout(...args),
    clearTimeout: (id) => clearTimeout(id),
  };
}

const { subscribeCanonical, subscribeWithEdgeSplit } = await import(
  "./relayLiveSubscription.ts"
);

const CHANNEL = "550e8400-e29b-41d4-a716-446655440000";
const KIND_MESSAGE = 9;

function makePort() {
  const sent = [];
  const closed = [];
  return {
    sent,
    closed,
    port: {
      ensureConnected: async () => {},
      subscriptions: new Map(),
      sendReq: async (subId, filter) => sent.push({ subId, filter }),
      closeSubscription: async (subId) => closed.push(subId),
    },
  };
}

function makeEdge({ binding = { communityId: "c" }, subscribe } = {}) {
  const calls = [];
  return {
    calls,
    edge: {
      currentBinding: () => binding,
      subscribe: async (filter, onEvent) => {
        calls.push({ filter, onEvent });
        return subscribe ? subscribe(filter, onEvent) : async () => {};
      },
    },
  };
}

test("an edge-eligible filter splits: kind 9 to the edge, the rest canonical", async () => {
  const { sent, port } = makePort();
  const { calls, edge } = makeEdge();

  const unsubscribe = await subscribeWithEdgeSplit(
    port,
    edge,
    { kinds: [KIND_MESSAGE, 40099], "#h": [CHANNEL], limit: 50 },
    () => {},
  );

  assert.deepEqual(calls[0].filter.kinds, [KIND_MESSAGE]);
  assert.equal(sent.length, 1);
  assert.deepEqual(sent[0].filter.kinds, [40099]);
  await unsubscribe();
});

test("a message-only filter never opens a canonical subscription", async () => {
  const { sent, port } = makePort();
  const { calls, edge } = makeEdge();

  await subscribeWithEdgeSplit(
    port,
    edge,
    { kinds: [KIND_MESSAGE], "#h": [CHANNEL], limit: 50 },
    () => {},
  );

  assert.equal(calls.length, 1);
  assert.equal(sent.length, 0, "canonical must carry no half of this filter");
});

test("no binding keeps the whole filter canonical", async () => {
  const { sent, port } = makePort();
  const { calls, edge } = makeEdge({ binding: null });

  await subscribeWithEdgeSplit(
    port,
    edge,
    { kinds: [KIND_MESSAGE], "#h": [CHANNEL], limit: 50 },
    () => {},
  );

  assert.equal(calls.length, 0);
  assert.deepEqual(sent[0].filter.kinds, [KIND_MESSAGE]);
});

test("a non-message filter is never offered to the edge", async () => {
  const { sent, port } = makePort();
  const { calls, edge } = makeEdge();

  await subscribeWithEdgeSplit(
    port,
    edge,
    { kinds: [20001], limit: 0 },
    () => {},
  );

  assert.equal(calls.length, 0, "presence must not reach the sidecar");
  assert.deepEqual(sent[0].filter.kinds, [20001]);
});

test("an unavailable edge sends the ORIGINAL filter canonically, not the split half", async () => {
  const { sent, port } = makePort();
  const { edge } = makeEdge({ subscribe: () => null });

  await subscribeWithEdgeSplit(
    port,
    edge,
    { kinds: [KIND_MESSAGE, 40099], "#h": [CHANNEL], limit: 50 },
    () => {},
  );

  // A half subscription would silently drop messages — worse than no sidecar.
  assert.deepEqual(sent[0].filter.kinds, [KIND_MESSAGE, 40099]);
});

test("a canonical failure tears the edge half down instead of leaving it alone", async () => {
  const { port } = makePort();
  let edgeClosed = false;
  const { edge } = makeEdge({
    subscribe: () => async () => {
      edgeClosed = true;
    },
  });
  port.sendReq = async () => {
    throw new Error("canonical REQ failed");
  };

  await assert.rejects(
    subscribeWithEdgeSplit(
      port,
      edge,
      { kinds: [KIND_MESSAGE, 40099], "#h": [CHANNEL], limit: 50 },
      () => {},
    ),
    /canonical REQ failed/,
  );
  assert.equal(edgeClosed, true);
});

test("unsubscribing closes both halves", async () => {
  const { closed, port } = makePort();
  let edgeClosed = false;
  const { edge } = makeEdge({
    subscribe: () => async () => {
      edgeClosed = true;
    },
  });

  const unsubscribe = await subscribeWithEdgeSplit(
    port,
    edge,
    { kinds: [KIND_MESSAGE, 40099], "#h": [CHANNEL], limit: 50 },
    () => {},
  );
  await unsubscribe();

  assert.equal(edgeClosed, true);
  assert.equal(closed.length, 1);
});

test("canonical subscribe registers, sends, and cleans up on unsubscribe", async () => {
  const { sent, closed, port } = makePort();

  const unsubscribe = await subscribeCanonical(
    port,
    { kinds: [KIND_MESSAGE], "#h": [CHANNEL], limit: 50 },
    () => {},
  );
  assert.equal(port.subscriptions.size, 1);
  assert.equal(sent[0].subId.startsWith("live-"), true);

  await unsubscribe();
  assert.equal(port.subscriptions.size, 0);
  assert.deepEqual(closed, [sent[0].subId]);
});

test("a failed canonical REQ leaves no orphan subscription behind", async () => {
  const { port } = makePort();
  port.sendReq = async () => {
    throw new Error("send failed");
  };

  await assert.rejects(
    subscribeCanonical(
      port,
      { kinds: [KIND_MESSAGE], "#h": [CHANNEL], limit: 50 },
      () => {},
    ),
    /send failed/,
  );
  assert.equal(port.subscriptions.size, 0);
});
