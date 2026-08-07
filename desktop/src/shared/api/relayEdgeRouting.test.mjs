import assert from "node:assert/strict";
import test from "node:test";

import { splitEdgeMessageFilter } from "./relayEdgeRouting.ts";

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
