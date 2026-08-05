import assert from "node:assert/strict";
import test from "node:test";

import {
  channelFilter,
  threadDirectoryFilter,
} from "./relayLiveSubscriptionFilters.ts";

test("thread directory live filter stays isolated and channel-scoped", () => {
  const before = Math.floor(Date.now() / 1_000);
  const filter = threadDirectoryFilter("channel-a");
  const after = Math.floor(Date.now() / 1_000);

  assert.deepEqual(filter.kinds, [39007]);
  assert.deepEqual(filter["#h"], ["channel-a"]);
  assert.equal(filter.limit, 0);
  assert.ok(filter.since >= before && filter.since <= after);
});

test("channel live filter retains timeline rows and summary overlays", () => {
  const filter = channelFilter("channel-b");

  assert.ok(filter.kinds.includes(39005));
  assert.ok(!filter.kinds.includes(39007));
  assert.deepEqual(filter["#h"], ["channel-b"]);
  assert.equal(filter.limit, 1000);
});
