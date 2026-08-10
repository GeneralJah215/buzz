/**
 * Guardrails for reveal resolution (BUG-058).
 *
 * The find bar's client pass matches text in every loaded message, thread
 * replies included, and adds them to `matchCount`. The main timeline renders
 * NO replies. Before this module the two facts never met: `Enter` scrolled to a
 * row that does not exist, the pending-target ref held the id, and the bar
 * cheerfully read "3 of 27" while doing nothing.
 *
 * The fix is not to stop counting those matches — that would delete exactly the
 * results the reader is hunting for. The fix is that a reply resolves to its
 * THREAD ROOT, which the panel can open. These tests pin that, and pin that an
 * id which genuinely cannot be shown resolves to an explicit "unreachable"
 * rather than to something a caller would keep retrying.
 */

import assert from "node:assert/strict";
import { describe, test } from "node:test";

const { isMainTimelineMessage, resolveMessageRevealTarget } = await import(
  "./messageRevealTarget.ts"
);
const { buildMainTimelineEntries } = await import("./threadPanel.ts");

const CHANNEL_ID = "11111111-2222-3333-4444-555555555555";

function makeMessage(id, overrides = {}) {
  return {
    id,
    createdAt: 1_700_000_000,
    author: "tester",
    time: "12:00",
    body: `body of ${id}`,
    depth: 0,
    parentId: null,
    rootId: null,
    kind: 9,
    tags: [["h", CHANNEL_ID]],
    ...overrides,
  };
}

function indexOf(messages) {
  return new Map(messages.map((message) => [message.id, message]));
}

describe("resolveMessageRevealTarget", () => {
  test("nothing to reveal resolves to null", () => {
    assert.equal(resolveMessageRevealTarget(null, indexOf([])), null);
  });

  test("a root message reveals on the main timeline", () => {
    const root = makeMessage("root");
    const target = resolveMessageRevealTarget("root", indexOf([root]));

    assert.equal(target.kind, "main-timeline");
    assert.equal(target.mainTimelineMessageId, "root");
    assert.equal(target.threadHeadId, null);
    assert.equal(target.unreachableReason, null);
  });

  test("a broadcast reply reveals on the main timeline", () => {
    const root = makeMessage("root");
    const broadcast = makeMessage("shout", {
      parentId: "root",
      rootId: "root",
      tags: [
        ["h", CHANNEL_ID],
        ["broadcast", "1"],
      ],
    });
    const target = resolveMessageRevealTarget(
      "shout",
      indexOf([root, broadcast]),
    );

    assert.equal(
      target.kind,
      "main-timeline",
      "broadcast replies are mirrored onto the main timeline and have a row there",
    );
    assert.equal(target.mainTimelineMessageId, "shout");
  });

  test("a thread reply resolves to its thread root, not to a main-timeline row", () => {
    const root = makeMessage("root");
    const reply = makeMessage("reply", { parentId: "root", rootId: "root" });
    const target = resolveMessageRevealTarget("reply", indexOf([root, reply]));

    assert.equal(
      target.kind,
      "thread-reply",
      "a reply is revealed in a thread panel, never by scrolling the main timeline",
    );
    assert.equal(
      target.threadHeadId,
      "root",
      "the thread panel opens on a root id — that is what the reply must resolve to",
    );
    assert.equal(
      target.mainTimelineMessageId,
      "root",
      "the main timeline points at the conversation that holds the match",
    );
    assert.deepEqual(
      [...target.expandedReplyIds],
      [],
      "a direct child of the root needs no intermediate branch expanded",
    );
    assert.equal(target.unreachableReason, null);
  });

  test("a nested reply expands every ancestor between it and the root", () => {
    const messages = [
      makeMessage("root"),
      makeMessage("mid", { parentId: "root", rootId: "root" }),
      makeMessage("deep", { parentId: "mid", rootId: "root" }),
      makeMessage("deeper", { parentId: "deep", rootId: "root" }),
    ];
    const target = resolveMessageRevealTarget("deeper", indexOf(messages));

    assert.equal(target.threadHeadId, "root");
    assert.deepEqual(
      [...target.expandedReplyIds].sort(),
      ["deep", "mid"],
      "every collapsed branch between the root and the match must be opened",
    );
  });

  test("a reply whose root is not loaded is unreachable, not pending", () => {
    const reply = makeMessage("reply", {
      parentId: "missing-root",
      rootId: "missing-root",
    });
    const target = resolveMessageRevealTarget("reply", indexOf([reply]));

    assert.equal(target.kind, "unreachable");
    assert.equal(target.unreachableReason, "detached-ancestry");
    assert.equal(
      target.mainTimelineMessageId,
      null,
      "handing the raw id to the timeline is what made it retry forever",
    );
  });

  test("a reply with a missing intermediate ancestor is unreachable", () => {
    const messages = [
      makeMessage("root"),
      makeMessage("deep", { parentId: "gone", rootId: "root" }),
    ];
    const target = resolveMessageRevealTarget("deep", indexOf(messages));

    assert.equal(target.kind, "unreachable");
    assert.equal(target.unreachableReason, "detached-ancestry");
  });

  test("an id outside the loaded window is unreachable", () => {
    const target = resolveMessageRevealTarget(
      "never-loaded",
      indexOf([makeMessage("root")]),
    );

    assert.equal(target.kind, "unreachable");
    assert.equal(target.unreachableReason, "not-loaded");
    assert.equal(target.mainTimelineMessageId, null);
  });

  test("a cyclic parent chain terminates instead of looping", () => {
    const messages = [
      makeMessage("root"),
      makeMessage("a", { parentId: "b", rootId: "root" }),
      makeMessage("b", { parentId: "a", rootId: "root" }),
    ];
    const target = resolveMessageRevealTarget("a", indexOf(messages));

    assert.equal(target.kind, "unreachable");
    assert.equal(target.unreachableReason, "detached-ancestry");
  });
});

describe("isMainTimelineMessage", () => {
  test("agrees exactly with the rows buildMainTimelineEntries renders", () => {
    const messages = [
      makeMessage("root-a"),
      makeMessage("reply-a", { parentId: "root-a", rootId: "root-a" }),
      makeMessage("shout", {
        parentId: "root-a",
        rootId: "root-a",
        tags: [
          ["h", CHANNEL_ID],
          ["broadcast", "1"],
        ],
      }),
      makeMessage("root-b"),
      makeMessage("deep", { parentId: "reply-a", rootId: "root-a" }),
    ];

    assert.deepEqual(
      buildMainTimelineEntries(messages).map((entry) => entry.message.id),
      messages.filter(isMainTimelineMessage).map((message) => message.id),
      "reveal resolution and the main timeline's row filter must never drift apart",
    );
  });
});
