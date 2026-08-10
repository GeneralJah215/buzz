/**
 * Wiring guardrails for revealing a find-in-channel match (BUG-058).
 *
 * `useChannelFind` decides WHERE a match can be shown; this hook is what
 * actually opens the surface. These tests mount the real hook against stub
 * panel setters and assert the composition end to end: a reply match opens the
 * thread panel on its root, expands the branch, and scrolls the panel to the
 * reply — the same composition a deep link to a reply already performs.
 *
 * They also pin the negative half, which matters just as much for how
 * Next/Previous feels: a main-timeline match must NOT touch panel state.
 * Opening a panel per match would make walking the result list flap panels
 * open and shut.
 */

import assert from "node:assert/strict";
import { after, describe, test } from "node:test";

import { JSDOM } from "jsdom";

const dom = new JSDOM("<!doctype html><html><body></body></html>", {
  url: "http://localhost",
});

after(() => dom.window.close());

Object.assign(globalThis, {
  document: dom.window.document,
  Event: dom.window.Event,
  HTMLElement: dom.window.HTMLElement,
  Node: dom.window.Node,
  IS_REACT_ACT_ENVIRONMENT: true,
  window: dom.window,
});

const React = await import("react");
const { act } = React;
const { createRoot } = await import("react-dom/client");
const { useChannelRouteTarget } = await import("./useChannelRouteTarget.ts");
const { resolveMessageRevealTarget } = await import(
  "@/features/messages/lib/messageRevealTarget.ts"
);

const CHANNEL_ID = "11111111-2222-3333-4444-555555555555";
const CHANNEL = { id: CHANNEL_ID, name: "general", channelType: "stream" };

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

const TIMELINE = [
  makeMessage("root"),
  makeMessage("r1", { parentId: "root", rootId: "root" }),
  makeMessage("deep", { parentId: "r1", rootId: "root" }),
  makeMessage("other-root"),
];

/** Mount the hook with recording setters; re-render with a new reveal target. */
async function mountRouteTarget() {
  const container = dom.window.document.createElement("div");
  dom.window.document.body.appendChild(container);

  const calls = {
    closeAgentSession: 0,
    expandedReplyIds: [],
    openThreadHeadId: [],
    threadReplyTargetId: [],
    threadScrollTargetId: [],
  };

  let setReveal = () => {};

  function Probe() {
    const [reveal, setRevealState] = React.useState(null);
    setReveal = setRevealState;
    useChannelRouteTarget({
      activeChannel: CHANNEL,
      activeChannelId: CHANNEL_ID,
      closeAgentSession: () => {
        calls.closeAgentSession += 1;
      },
      setEditTargetId: () => {},
      setExpandedThreadReplyIds: (value) =>
        calls.expandedReplyIds.push([...value]),
      setOpenThreadHeadId: (value) => calls.openThreadHeadId.push(value),
      setProfilePanelPubkey: () => {},
      setThreadReplyTargetId: (value) => calls.threadReplyTargetId.push(value),
      setThreadScrollTargetId: (value) =>
        calls.threadScrollTargetId.push(value),
      searchRevealTarget: reveal,
      targetMessageId: null,
      timelineMessages: TIMELINE,
    });
    return null;
  }

  const root = createRoot(container);
  await act(async () => {
    root.render(React.createElement(Probe));
  });

  return {
    calls,
    async revealMatch(messageId) {
      const target = resolveMessageRevealTarget(
        messageId,
        new Map(TIMELINE.map((message) => [message.id, message])),
      );
      await act(async () => {
        setReveal(target);
      });
      return target;
    },
    async unmount() {
      await act(async () => {
        root.unmount();
      });
      container.remove();
    },
  };
}

describe("useChannelRouteTarget search reveal", { concurrency: 1 }, () => {
  test("a reply match opens its thread panel and scrolls the panel to the reply", async (t) => {
    const harness = await mountRouteTarget();
    t.after(() => harness.unmount());

    await harness.revealMatch("deep");

    assert.deepEqual(
      harness.calls.openThreadHeadId,
      ["root"],
      "the panel opens on the thread ROOT — that is the only id it accepts",
    );
    assert.deepEqual(
      harness.calls.threadScrollTargetId,
      ["deep"],
      "and the panel is then scrolled to the match itself",
    );
    assert.deepEqual(
      harness.calls.expandedReplyIds,
      [["r1"]],
      "the branch between the root and the match must be expanded to show it",
    );
  });

  test("a main-timeline match leaves the panels exactly as they are", async (t) => {
    const harness = await mountRouteTarget();
    t.after(() => harness.unmount());

    await harness.revealMatch("other-root");

    assert.deepEqual(
      harness.calls.openThreadHeadId,
      [],
      "walking the result list must not flap a thread panel open on every root match",
    );
    assert.deepEqual(harness.calls.threadScrollTargetId, []);
    assert.equal(harness.calls.closeAgentSession, 0);
  });

  test("moving off a reply match does not close the panel it opened", async (t) => {
    const harness = await mountRouteTarget();
    t.after(() => harness.unmount());

    await harness.revealMatch("r1");
    await harness.revealMatch("other-root");

    assert.deepEqual(
      harness.calls.openThreadHeadId,
      ["root"],
      "the panel stays put; a jump that silently closes panels is worse than the bug",
    );
  });

  test("an unreachable match opens nothing", async (t) => {
    const harness = await mountRouteTarget();
    t.after(() => harness.unmount());

    const target = await harness.revealMatch("never-loaded");

    assert.equal(target.kind, "unreachable");
    assert.deepEqual(harness.calls.openThreadHeadId, []);
    assert.deepEqual(harness.calls.threadScrollTargetId, []);
  });

  test("re-entering the same reply match does not re-open or re-scroll", async (t) => {
    const harness = await mountRouteTarget();
    t.after(() => harness.unmount());

    await harness.revealMatch("deep");
    await harness.revealMatch("deep");

    assert.deepEqual(
      harness.calls.openThreadHeadId,
      ["root"],
      "a render churn must not re-drive the panel and yank the reader's scroll",
    );
    assert.deepEqual(harness.calls.threadScrollTargetId, ["deep"]);
  });
});
