import assert from "node:assert/strict";
import test from "node:test";

import {
  mergeThreadDirectoryPage,
  resolveThreadDirectoryTitle,
  sortThreadDirectoryItems,
  threadDirectoryUnreadState,
} from "./threadDirectory.ts";
import { parseThreadDirectoryPage } from "@/shared/api/threadDirectory";
import {
  KIND_THREAD_DIRECTORY_BOUNDS,
  KIND_THREAD_DIRECTORY_ITEM,
} from "@/shared/constants/kinds";

const CHANNEL_ID = "36411e44-0e2d-4cfe-bd6e-567eb169db9f";
const OTHER_CHANNEL_ID = "3c411e44-0e2d-4cfe-bd6e-567eb169db9f";
const ROOT_A = "a".repeat(64);
const ROOT_B = "b".repeat(64);
const PUBKEY = "c".repeat(64);

function itemEvent(overrides = {}) {
  const {
    rootId = ROOT_A,
    channelId = CHANNEL_ID,
    content: contentOverrides,
    ...eventOverrides
  } = overrides;
  // A string override is a raw content body, used to exercise the malformed-JSON
  // path. It must not be spread: spreading "{" yields { 0: "{" }, which merges
  // into a *valid* item and makes the rejection test silently unfalsifiable.
  const content =
    typeof contentOverrides === "string"
      ? contentOverrides
      : JSON.stringify({
          title: "Generated title",
          title_override: null,
          root_author: PUBKEY,
          root_created_at: 100,
          reply_count: 3,
          descendant_count: 3,
          last_reply_at: 200,
          participants: [PUBKEY],
          pinned: false,
          archived: false,
          state_created_at: 0,
          state_event_id: null,
          ...contentOverrides,
        });
  return {
    id: "d".repeat(64),
    pubkey: PUBKEY,
    created_at: 200,
    kind: KIND_THREAD_DIRECTORY_ITEM,
    tags: [
      ["e", rootId],
      ["d", rootId],
      ["h", channelId],
    ],
    content,
    sig: "sig",
    ...eventOverrides,
  };
}

function boundsEvent(overrides = {}) {
  const {
    channelId = CHANNEL_ID,
    state = "active",
    requestCursor = null,
    content: contentOverrides,
    ...eventOverrides
  } = overrides;
  // Same rule as itemEvent: a string override is a raw content body, never spread.
  const content =
    typeof contentOverrides === "string"
      ? contentOverrides
      : JSON.stringify({
          has_more: false,
          next_cursor: null,
          ...contentOverrides,
        });
  return {
    id: "e".repeat(64),
    pubkey: PUBKEY,
    created_at: 200,
    kind: KIND_THREAD_DIRECTORY_BOUNDS,
    tags: [
      ["d", `${channelId}:${state}:${requestCursor ?? "head"}`],
      ["h", channelId],
    ],
    content,
    sig: "sig",
    ...eventOverrides,
  };
}

test("parses a channel-matched item and its single bounds overlay", () => {
  const page = parseThreadDirectoryPage(
    [itemEvent(), boundsEvent()],
    CHANNEL_ID,
    "active",
  );
  assert.equal(page.items.length, 1);
  assert.equal(page.items[0].rootId, ROOT_A);
  assert.equal(page.bounds.hasMore, false);
  assert.equal(page.bounds.nextCursor, null);
});

test("binds the bounds overlay to the exact requested cursor", () => {
  const cursor = "opaque-page-cursor";
  const page = parseThreadDirectoryPage(
    [itemEvent(), boundsEvent({ requestCursor: cursor })],
    CHANNEL_ID,
    "active",
    cursor,
  );
  assert.equal(page.bounds.hasMore, false);

  assert.throws(
    () =>
      parseThreadDirectoryPage(
        [itemEvent(), boundsEvent({ requestCursor: "stale-cursor" })],
        CHANNEL_ID,
        "active",
        cursor,
      ),
    /requested page/i,
  );
});

test("rejects malformed duplicate tags and noncanonical hex identities", () => {
  assert.throws(
    () =>
      parseThreadDirectoryPage(
        [
          itemEvent({
            tags: [
              ["e", ROOT_A],
              ["d", ROOT_A],
              ["h", CHANNEL_ID],
              ["h", CHANNEL_ID, "extra"],
            ],
          }),
          boundsEvent(),
        ],
        CHANNEL_ID,
        "active",
      ),
    /exactly one h tag/i,
  );
  assert.throws(
    () =>
      parseThreadDirectoryPage(
        [itemEvent({ rootId: "not-hex" }), boundsEvent()],
        CHANNEL_ID,
        "active",
      ),
    /root/i,
  );
  assert.throws(
    () =>
      parseThreadDirectoryPage(
        [itemEvent({ content: { root_author: "not-hex" } }), boundsEvent()],
        CHANNEL_ID,
        "active",
      ),
    /root_author/i,
  );
});

for (const [name, events] of [
  ["wrong item kind", [itemEvent({ kind: 9 }), boundsEvent()]],
  [
    "missing root tag",
    [
      itemEvent({
        tags: [
          ["d", ROOT_A],
          ["h", CHANNEL_ID],
        ],
      }),
      boundsEvent(),
    ],
  ],
  [
    "channel mismatch",
    [itemEvent({ channelId: OTHER_CHANNEL_ID }), boundsEvent()],
  ],
  ["malformed item JSON", [itemEvent({ content: "{" }), boundsEvent()]],
  [
    "invalid count",
    [itemEvent({ content: { reply_count: -1 } }), boundsEvent()],
  ],
  [
    "invalid timestamp",
    [itemEvent({ content: { last_reply_at: -1 } }), boundsEvent()],
  ],
  ["mismatched bounds", [itemEvent(), boundsEvent({ state: "archived" })]],
]) {
  test(`rejects ${name}`, () => {
    assert.throws(
      () => parseThreadDirectoryPage(events, CHANNEL_ID, "active"),
      /thread directory/i,
    );
  });
}

test("sorts pinned threads first, then newest activity with root-id tie break", () => {
  const sorted = sortThreadDirectoryItems([
    { rootId: ROOT_B, pinned: false, lastReplyAt: 20, rootCreatedAt: 1 },
    { rootId: ROOT_A, pinned: true, lastReplyAt: 10, rootCreatedAt: 1 },
    {
      rootId: "0".repeat(64),
      pinned: false,
      lastReplyAt: 20,
      rootCreatedAt: 1,
    },
  ]);
  assert.deepEqual(
    sorted.map((item) => item.rootId),
    [ROOT_A, "0".repeat(64), ROOT_B],
  );
});

test("a newer removal tombstone prevents a late page from reviving its root", () => {
  const item = parseThreadDirectoryPage(
    [itemEvent(), boundsEvent()],
    CHANNEL_ID,
    "active",
  ).items[0];
  const merged = mergeThreadDirectoryPage(
    { items: [], removedRootIds: new Set([ROOT_A]) },
    [item],
  );
  assert.deepEqual(merged.items, []);
});

test("uses an explicit title override and exposes only a proven unread count", () => {
  const item = {
    rootId: ROOT_A,
    title: "Generated title",
    titleOverride: "Shared title",
    rootCreatedAt: 100,
    lastReplyAt: 200,
  };
  assert.equal(resolveThreadDirectoryTitle(item), "Shared title");
  assert.deepEqual(
    threadDirectoryUnreadState(item, () => 150, null),
    {
      isUnread: true,
      unreadCount: null,
    },
  );
  assert.deepEqual(
    threadDirectoryUnreadState(item, () => 150, 2),
    {
      isUnread: true,
      unreadCount: 2,
    },
  );
});
