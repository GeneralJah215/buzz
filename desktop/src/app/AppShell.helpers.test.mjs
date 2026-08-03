import assert from "node:assert/strict";
import test from "node:test";

import {
  getThreadReadAtWithChannelFallback,
  shouldBounceForChannelNotification,
} from "./AppShell.helpers.ts";

test("shouldBounceForChannelNotification_allowsTopLevelChannelMessages", () => {
  assert.equal(shouldBounceForChannelNotification([["h", "channel"]]), true);
});

test("shouldBounceForChannelNotification_suppressesThreadReplies", () => {
  assert.equal(
    shouldBounceForChannelNotification([
      ["h", "channel"],
      ["e", "root", "", "reply"],
    ]),
    false,
  );
});

test("shouldBounceForChannelNotification_allowsBroadcastReplies", () => {
  assert.equal(
    shouldBounceForChannelNotification([
      ["h", "channel"],
      ["e", "root", "", "reply"],
      ["broadcast", "1"],
    ]),
    true,
  );
});

test("getThreadReadAtWithChannelFallback uses the explicit directory channel", () => {
  const ownCalls = [];
  const channelCalls = [];
  const options = {
    rootId: "root",
    channelId: "directory-channel",
    getOwnReadAt(contextId) {
      ownCalls.push(contextId);
      return 100;
    },
    getChannelReadAt(channelId) {
      channelCalls.push(channelId);
      return 150;
    },
  };
  assert.equal(getThreadReadAtWithChannelFallback(options), 150);
  assert.deepEqual(ownCalls, ["thread:root"]);
  assert.deepEqual(channelCalls, ["directory-channel"]);
});

test("getThreadReadAtWithChannelFallback handles either or neither marker", () => {
  const value = (threadReadAt, channelReadAt, channelId = "channel") =>
    getThreadReadAtWithChannelFallback({
      rootId: "root",
      channelId,
      getOwnReadAt: () => threadReadAt,
      getChannelReadAt: () => channelReadAt,
    });

  assert.equal(value(100, null), 100);
  assert.equal(value(null, 150), 150);
  assert.equal(value(null, null), null);
  assert.equal(value(100, 150, null), 100);
});
