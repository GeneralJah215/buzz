import assert from "node:assert/strict";
import test from "node:test";

import {
  mergeRelayEvents,
  splitEdgeMessageFilter,
} from "./relayEdgeRouting.ts";

const channel = "550e8400-e29b-41d4-a716-446655440000";

test("splits kind 9 from a mixed UUID-channel filter", () => {
  const split = splitEdgeMessageFilter({
    kinds: [9, 7, 40099],
    "#h": [channel],
    limit: 50,
  });
  assert.deepEqual(split?.edge, {
    kinds: [9],
    "#h": [channel],
    limit: 50,
  });
  assert.deepEqual(split?.canonical?.kinds, [7, 40099]);
});

test("keeps wildcard and non-channel filters canonical-only", () => {
  assert.equal(splitEdgeMessageFilter({ "#h": [channel] }), null);
  assert.equal(splitEdgeMessageFilter({ kinds: [9] }), null);
  assert.equal(
    splitEdgeMessageFilter({ kinds: [9], "#h": ["not-a-uuid"] }),
    null,
  );
});

test("message-only filters have no canonical half", () => {
  const split = splitEdgeMessageFilter({ kinds: [9], "#h": [channel] });
  assert.equal(split?.canonical, null);
});

test("merge de-duplicates by event id and restores descending time order", () => {
  const old = { id: "old", created_at: 1 };
  const fresh = { id: "fresh", created_at: 3 };
  assert.deepEqual(
    mergeRelayEvents([old, fresh], [old, { id: "middle", created_at: 2 }]).map(
      (event) => event.id,
    ),
    ["fresh", "middle", "old"],
  );
});
