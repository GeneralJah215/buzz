import assert from "node:assert/strict";
import test from "node:test";
import { QueryClient } from "@tanstack/react-query";

import {
  chooseThreadDirectoryProjection,
  discardPreviousThreadDirectoryScope,
  isThreadDirectoryItemInState,
  mergeThreadDirectoryLiveProjection,
  reconcileThreadDirectoryItems,
  resolveThreadDirectoryTitle,
  rollbackThreadDirectoryOptimisticProjection,
  sortThreadDirectoryItems,
  threadDirectoryProjection,
  threadDirectoryLiveQueryKey,
  threadDirectoryQueryKey,
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
  assert.throws(
    () =>
      parseThreadDirectoryPage(
        [itemEvent({ rootId: "A".repeat(64) }), boundsEvent()],
        CHANNEL_ID,
        "active",
      ),
    /root/i,
  );
  assert.throws(
    () =>
      parseThreadDirectoryPage(
        [itemEvent({ content: { participants: ["not-hex"] } }), boundsEvent()],
        CHANNEL_ID,
        "active",
      ),
    /participants/i,
  );
  assert.throws(
    () =>
      parseThreadDirectoryPage(
        [itemEvent({ content: { state_event_id: "not-hex" } }), boundsEvent()],
        CHANNEL_ID,
        "active",
      ),
    /state event id/i,
  );
  assert.throws(
    () =>
      parseThreadDirectoryPage(
        [
          itemEvent({
            tags: [
              ["e", ROOT_A],
              ["d", ROOT_A],
              ["h", CHANNEL_ID],
              ["x", "unexpected"],
            ],
          }),
          boundsEvent(),
        ],
        CHANNEL_ID,
        "active",
      ),
    /canonical tags/i,
  );
});

test("rejects inconsistent state timestamp and event-id pairs", () => {
  assert.throws(
    () =>
      parseThreadDirectoryPage(
        [
          itemEvent({ content: { state_event_id: "f".repeat(64) } }),
          boundsEvent(),
        ],
        CHANNEL_ID,
        "active",
      ),
    /state revision/i,
  );
  assert.throws(
    () =>
      parseThreadDirectoryPage(
        [itemEvent({ content: { state_created_at: 1 } }), boundsEvent()],
        CHANNEL_ID,
        "active",
      ),
    /state revision/i,
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

test("a newer live tombstone prevents a late page from reviving its root", () => {
  const item = parseThreadDirectoryPage(
    [itemEvent(), boundsEvent()],
    CHANNEL_ID,
    "active",
  ).items[0];
  const tombstone = {
    ...item,
    present: false,
    projectionCreatedAt: 300,
    projectionEventId: "f".repeat(64),
  };
  const live = mergeThreadDirectoryLiveProjection(
    { nextOrder: 1, byRootId: new Map() },
    tombstone,
    "live",
  );
  const merged = reconcileThreadDirectoryItems(
    [
      {
        items: [item],
        bounds: {
          channelId: CHANNEL_ID,
          state: "active",
          hasMore: false,
          nextCursor: null,
        },
        requestOrder: 1,
      },
    ],
    live.byRootId,
    "active",
    300,
  );
  assert.deepEqual(merged, []);
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
  assert.deepEqual(
    threadDirectoryUnreadState(item, () => 250, 2),
    {
      isUnread: false,
      unreadCount: null,
    },
  );
  assert.deepEqual(
    threadDirectoryUnreadState(item, () => 150, 0),
    {
      isUnread: true,
      unreadCount: null,
    },
  );
});

test("present defaults true, false is retained, and non-boolean is rejected", () => {
  const defaultPresent = parseThreadDirectoryPage(
    [itemEvent(), boundsEvent()],
    CHANNEL_ID,
    "active",
  ).items[0];
  assert.equal(defaultPresent.present, true);

  const removed = parseThreadDirectoryPage(
    [itemEvent({ content: { present: false } }), boundsEvent()],
    CHANNEL_ID,
    "active",
  ).items[0];
  assert.equal(removed.present, false);

  assert.throws(
    () =>
      parseThreadDirectoryPage(
        [itemEvent({ content: { present: "false" } }), boundsEvent()],
        CHANNEL_ID,
        "active",
      ),
    /present/i,
  );
});

test("active membership enforces persistence, threshold, cutoff, and zero-descendant exclusion", () => {
  const cutoff = 1_000;
  const base = {
    present: true,
    archived: false,
    pinned: false,
    titleOverride: null,
    descendantCount: 3,
    lastReplyAt: cutoff,
    rootCreatedAt: 1,
  };
  assert.equal(
    isThreadDirectoryItemInState(base, "active", cutoff + 30 * 86_400),
    true,
  );
  assert.equal(
    isThreadDirectoryItemInState(
      { ...base, lastReplyAt: cutoff - 1 },
      "active",
      cutoff + 30 * 86_400,
    ),
    false,
  );
  assert.equal(
    isThreadDirectoryItemInState(
      { ...base, descendantCount: 2 },
      "active",
      cutoff,
    ),
    false,
  );
  assert.equal(
    isThreadDirectoryItemInState(
      { ...base, descendantCount: 2, titleOverride: "Persistent" },
      "active",
      cutoff + 40 * 86_400,
    ),
    true,
  );
  assert.equal(
    isThreadDirectoryItemInState(
      { ...base, descendantCount: 0, pinned: true },
      "active",
      cutoff,
    ),
    false,
  );
  assert.equal(
    isThreadDirectoryItemInState(
      { ...base, descendantCount: 0, archived: true },
      "archived",
      cutoff,
    ),
    false,
  );
  assert.equal(
    isThreadDirectoryItemInState(
      { ...base, present: false, pinned: true, titleOverride: "Still gone" },
      "active",
      cutoff,
    ),
    false,
  );
});

test("reconciliation deduplicates across pages and applies one global sort", () => {
  const itemA = parseThreadDirectoryPage(
    [itemEvent(), boundsEvent()],
    CHANNEL_ID,
    "active",
  ).items[0];
  const itemB = {
    ...itemA,
    rootId: ROOT_B,
    pinned: true,
    projectionEventId: "1".repeat(64),
  };
  const bounds = {
    channelId: CHANNEL_ID,
    state: "active",
    hasMore: false,
    nextCursor: null,
  };
  const items = reconcileThreadDirectoryItems(
    [
      { items: [itemA], bounds, requestOrder: 1 },
      { items: [itemA, itemB], bounds, requestOrder: 2 },
    ],
    new Map(),
    "active",
    10_000_000,
  );
  assert.deepEqual(
    items.map((item) => item.rootId),
    [ROOT_B, ROOT_A],
  );
});

test("causal fence protects newer live state and lets a later page supersede it", () => {
  const pageItem = parseThreadDirectoryPage(
    [itemEvent(), boundsEvent()],
    CHANNEL_ID,
    "active",
  ).items[0];
  const liveItem = {
    ...pageItem,
    titleOverride: "Live title",
    pinned: true,
    replyCount: 2,
  };
  const liveProjection = threadDirectoryProjection(liveItem, "live", 2);
  const stalePageProjection = threadDirectoryProjection(pageItem, "page", 1);
  assert.equal(
    chooseThreadDirectoryProjection(stalePageProjection, liveProjection).item
      .titleOverride,
    "Live title",
  );

  const freshPageItem = { ...pageItem, titleOverride: "Relay title" };
  const freshPageProjection = threadDirectoryProjection(
    freshPageItem,
    "page",
    3,
  );
  assert.equal(
    chooseThreadDirectoryProjection(liveProjection, freshPageProjection).item
      .titleOverride,
    "Relay title",
  );
});

test("stale live false loses to a newer page and equal-revision false clears", () => {
  const item = parseThreadDirectoryPage(
    [itemEvent(), boundsEvent()],
    CHANNEL_ID,
    "active",
  ).items[0];
  const current = threadDirectoryProjection(item, "page", 1);
  const staleFalse = threadDirectoryProjection(
    {
      ...item,
      present: false,
      projectionCreatedAt: item.projectionCreatedAt - 1,
    },
    "live",
    2,
  );
  assert.equal(
    chooseThreadDirectoryProjection(current, staleFalse).item.present,
    true,
  );

  const equalFalse = threadDirectoryProjection(
    { ...item, present: false },
    "page",
    1,
  );
  assert.equal(
    chooseThreadDirectoryProjection(current, equalFalse).item.present,
    false,
  );
});

test("scope disposal removes only obsolete exact page and live caches", () => {
  const client = new QueryClient();
  const activeKey = threadDirectoryQueryKey(
    "community",
    "wss://relay",
    PUBKEY,
    CHANNEL_ID,
    "active",
  );
  const liveKey = threadDirectoryLiveQueryKey(
    "community",
    "wss://relay",
    PUBKEY,
    CHANNEL_ID,
  );
  const siblingKey = threadDirectoryQueryKey(
    "community",
    "wss://relay",
    PUBKEY,
    OTHER_CHANNEL_ID,
    "active",
  );
  const archivedKey = threadDirectoryQueryKey(
    "community",
    "wss://relay",
    PUBKEY,
    CHANNEL_ID,
    "archived",
  );
  client.setQueryData(activeKey, "active");
  client.setQueryData(liveKey, "live");
  client.setQueryData(siblingKey, "sibling");

  discardPreviousThreadDirectoryScope(
    { client, queryKey: activeKey, liveQueryKey: liveKey },
    { client, queryKey: archivedKey, liveQueryKey: [...liveKey] },
  );

  assert.equal(client.getQueryData(activeKey), undefined);
  assert.equal(client.getQueryData(liveKey), "live");
  assert.equal(client.getQueryData(siblingKey), "sibling");
});

test("optimistic rollback preserves unrelated and newer live projections", () => {
  const item = parseThreadDirectoryPage(
    [itemEvent(), boundsEvent()],
    CHANNEL_ID,
    "active",
  ).items[0];
  const prior = threadDirectoryProjection(item, "live", 1);
  const optimistic = threadDirectoryProjection(
    { ...item, pinned: true },
    "optimistic",
    2,
  );
  const other = threadDirectoryProjection(
    {
      ...item,
      rootId: ROOT_B,
      projectionEventId: "1".repeat(64),
    },
    "live",
    3,
  );
  const state = {
    nextOrder: 3,
    byRootId: new Map([
      [ROOT_A, optimistic],
      [ROOT_B, other],
    ]),
  };

  const rolledBack = rollbackThreadDirectoryOptimisticProjection(
    state,
    ROOT_A,
    2,
    prior,
  );
  assert.equal(rolledBack.byRootId.get(ROOT_A), prior);
  assert.equal(rolledBack.byRootId.get(ROOT_B), other);

  const newerRoot = threadDirectoryProjection(item, "live", 4);
  const withNewerRoot = {
    nextOrder: 4,
    byRootId: new Map([
      [ROOT_A, newerRoot],
      [ROOT_B, other],
    ]),
  };
  assert.equal(
    rollbackThreadDirectoryOptimisticProjection(
      withNewerRoot,
      ROOT_A,
      2,
      prior,
    ),
    withNewerRoot,
  );
});
