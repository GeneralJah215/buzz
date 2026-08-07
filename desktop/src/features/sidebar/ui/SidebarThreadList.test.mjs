import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import * as React from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { AppShellProvider } from "@/app/AppShellContext";

import {
  LegacySidebarThreadList,
  SidebarThreadDisclosure,
  ThreadDirectoryRow,
  ThreadDirectoryResults,
  legacySidebarThreadItems,
  threadDirectoryActionPatch,
  threadDirectoryCanTogglePin,
  threadDirectoryNavigationSearch,
  threadDirectoryRenameIsValid,
  threadDirectoryRenameValidationMessage,
  stopSidebarThreadInteraction,
} from "./SidebarThreadList.tsx";

const UI_SOURCE = await readFile(
  new URL("./SidebarThreadList.tsx", import.meta.url),
  "utf8",
);
const HOOK_SOURCE = await readFile(
  new URL("../useThreadDirectory.ts", import.meta.url),
  "utf8",
);

const CHANNEL_ID = "36411e44-0e2d-4cfe-bd6e-567eb169db9f";
const ROOT_ID = "a".repeat(64);
const ITEM = {
  rootId: ROOT_ID,
  channelId: CHANNEL_ID,
  title: "Generated thread title",
  titleOverride: "Shared thread title",
  rootAuthor: "b".repeat(64),
  rootCreatedAt: 100,
  replyCount: 3,
  descendantCount: 3,
  lastReplyAt: 200,
  participants: ["b".repeat(64)],
  pinned: true,
  archived: false,
  present: true,
  stateCreatedAt: 150,
  stateEventId: "c".repeat(64),
  projectionCreatedAt: 200,
  projectionEventId: "d".repeat(64),
};

function renderResults(overrides = {}) {
  return renderToStaticMarkup(
    React.createElement(ThreadDirectoryResults, {
      channelId: CHANNEL_ID,
      directoryState: "active",
      error: null,
      isPending: false,
      isUpdating: false,
      items: [],
      onNavigate() {},
      onRename() {},
      onUpdate() {},
      ...overrides,
    }),
  );
}

function maximumButtonDepth(html) {
  let depth = 0;
  let maximum = 0;
  for (const [token] of html.matchAll(/<\/?button\b[^>]*>/g)) {
    depth += token.startsWith("</") ? -1 : 1;
    maximum = Math.max(maximum, depth);
  }
  assert.equal(depth, 0, "button markup must be balanced");
  return maximum;
}

test("archive actions clear a pin while other state actions stay narrow", () => {
  assert.deepEqual(threadDirectoryActionPatch("archive"), {
    archived: true,
    pinned: false,
  });
  assert.deepEqual(threadDirectoryActionPatch("restore"), { archived: false });
  assert.deepEqual(threadDirectoryActionPatch("pin"), { pinned: true });
  assert.deepEqual(threadDirectoryActionPatch("unpin"), { pinned: false });
});

test("archived rows cannot offer a pin action that violates shared state", () => {
  assert.equal(threadDirectoryCanTogglePin({ archived: false }), true);
  assert.equal(threadDirectoryCanTogglePin({ archived: true }), false);
});

test("rename validation counts Unicode scalars instead of UTF-16 code units", () => {
  assert.equal(threadDirectoryRenameIsValid("Shared title"), true);
  assert.equal(threadDirectoryRenameIsValid("   "), false);
  assert.equal(threadDirectoryRenameIsValid("😀".repeat(120)), true);
  assert.equal(threadDirectoryRenameIsValid("😀".repeat(121)), false);
  assert.equal(threadDirectoryRenameIsValid("line one\nline two"), false);
  assert.equal(threadDirectoryRenameIsValid("control\u0085character"), false);
  assert.equal(threadDirectoryRenameIsValid("lone \ud800 surrogate"), false);
  assert.equal(
    threadDirectoryRenameValidationMessage("😀".repeat(121)),
    "Thread names must be 120 characters or fewer.",
  );
  assert.equal(
    threadDirectoryRenameValidationMessage("line one\nline two"),
    "Thread names must be a single line without control characters.",
  );
});

test("directory results render loading, empty, error, and pinned item states", () => {
  assert.match(renderResults({ isPending: true }), /Loading threads…/);
  assert.match(renderResults(), /No active threads/);

  const errorHtml = renderResults({ error: new Error("directory failed") });
  assert.match(errorHtml, /role="alert"/);
  assert.match(errorHtml, /directory failed/);

  const itemHtml = renderResults({ items: [ITEM] });
  assert.match(itemHtml, /Shared thread title/);
  assert.match(itemHtml, /Pinned/);
  assert.match(itemHtml, /data-testid="thread-directory-item"/);
  assert.match(itemHtml, /aria-label="More actions for Shared thread title"/);
  assert.equal(maximumButtonDepth(itemHtml), 1);
});

test("directory row reads the explicit channel marker and renders dot-only unread state", () => {
  const calls = [];
  const html = renderToStaticMarkup(
    React.createElement(
      AppShellProvider,
      {
        value: {
          getThreadReadAt(rootId, channelId) {
            calls.push([rootId, channelId]);
            return 150;
          },
        },
      },
      React.createElement(ThreadDirectoryRow, {
        channelId: CHANNEL_ID,
        item: ITEM,
        isUpdating: false,
        onNavigate() {},
        onRename() {},
        onUpdate() {},
      }),
    ),
  );
  assert.deepEqual(calls, [[ROOT_ID, CHANNEL_ID]]);
  assert.match(html, /aria-label="Unread thread"/);
  assert.doesNotMatch(html, />3</);
});

test("stream disclosure is a separate accessible control and forums omit it", () => {
  const streamHtml = renderToStaticMarkup(
    React.createElement(SidebarThreadDisclosure, {
      channel: {
        id: CHANNEL_ID,
        name: "general",
        channelType: "stream",
      },
    }),
  );
  assert.match(
    streamHtml,
    new RegExp(`data-testid="thread-directory-disclosure-${CHANNEL_ID}"`),
  );
  assert.match(streamHtml, /aria-label="Show threads for general"/);
  assert.equal(maximumButtonDepth(streamHtml), 1);

  const forumHtml = renderToStaticMarkup(
    React.createElement(SidebarThreadDisclosure, {
      channel: { id: "forum", name: "forum", channelType: "forum" },
    }),
  );
  assert.equal(forumHtml, "");
});

test("legacy fallback groups local activity by root and renders a plain list", () => {
  const otherRoot = "e".repeat(64);
  const items = legacySidebarThreadItems(
    [
      {
        id: "1",
        channelId: CHANNEL_ID,
        content: "Older reply",
        createdAt: 100,
        tags: [
          ["e", ROOT_ID, "", "root"],
          ["e", ROOT_ID, "", "reply"],
        ],
      },
      {
        id: "2",
        channelId: CHANNEL_ID,
        content: "Latest reply\nwith more detail",
        createdAt: 200,
        tags: [
          ["e", ROOT_ID, "", "root"],
          ["e", ROOT_ID, "", "reply"],
        ],
      },
      {
        id: "3",
        channelId: CHANNEL_ID,
        content: "Another thread",
        createdAt: 150,
        tags: [
          ["e", otherRoot, "", "root"],
          ["e", otherRoot, "", "reply"],
        ],
      },
    ],
    CHANNEL_ID,
  );
  // The row opens the thread root, so the label is the oldest known message in
  // the thread — not whatever was said last. Ordering still uses the newest.
  assert.deepEqual(items, [
    { rootId: ROOT_ID, title: "Older reply", lastReplyAt: 200 },
    { rootId: otherRoot, title: "Another thread", lastReplyAt: 150 },
  ]);

  const html = renderToStaticMarkup(
    React.createElement(LegacySidebarThreadList, {
      items,
      onNavigate() {},
    }),
  );
  assert.match(html, /data-testid="legacy-thread-list"/);
  assert.match(html, /Older reply/);
  assert.doesNotMatch(html, /Latest reply/);
});

test("legacy fallback ignores other channels and reply-less events", () => {
  assert.deepEqual(
    legacySidebarThreadItems(
      [
        {
          id: "1",
          channelId: "another-channel",
          content: "Elsewhere",
          createdAt: 100,
          tags: [
            ["e", ROOT_ID, "", "root"],
            ["e", ROOT_ID, "", "reply"],
          ],
        },
        {
          id: "2",
          channelId: CHANNEL_ID,
          content: "A thread root, not a reply",
          createdAt: 120,
          tags: [],
        },
      ],
      CHANNEL_ID,
    ),
    [],
  );
});

test("legacy fallback empty state does not claim the channel has no threads", () => {
  const html = renderToStaticMarkup(
    React.createElement(LegacySidebarThreadList, {
      items: [],
      onNavigate() {},
    }),
  );
  // The activity buffer is partial, so "No active threads" would be a claim the
  // fallback cannot support.
  assert.match(html, /No recent thread activity/);
  assert.doesNotMatch(html, /No active threads/);
});

test("the unsupported branch returns the fallback before any error UI", () => {
  const unsupportedBranch = UI_SOURCE.indexOf("if (directory.isUnsupported)");
  assert.ok(unsupportedBranch > 0, "unsupported branch is missing");
  assert.match(
    UI_SOURCE.slice(unsupportedBranch),
    /^[\s\S]{0,900}LegacySidebarThreadList/,
    "the unsupported branch must render the fallback list",
  );
  assert.ok(
    unsupportedBranch < UI_SOURCE.indexOf("error={directory.error"),
    "the fallback must short-circuit before the directory error surface",
  );
});

test("navigation and pointer guards preserve thread routing and channel isolation", () => {
  assert.deepEqual(threadDirectoryNavigationSearch(ROOT_ID), {
    messageId: ROOT_ID,
    threadRootId: ROOT_ID,
  });
  let stopped = 0;
  stopSidebarThreadInteraction({
    stopPropagation() {
      stopped += 1;
    },
  });
  assert.equal(stopped, 1);
});

test("production JSX wires navigation, drag isolation, and mutation rollback errors", () => {
  assert.match(
    UI_SOURCE,
    /goChannel\([\s\S]*threadDirectoryNavigationSearch\(rootId\)/,
  );
  assert.match(UI_SOURCE, /onPointerDown=\{stopSidebarThreadInteraction\}/);
  assert.match(UI_SOURCE, /\{renameError\}/);
  assert.match(UI_SOURCE, /\{directory\.updateError\.message/);
  assert.match(
    HOOK_SOURCE,
    /onError:[\s\S]*rollbackThreadDirectoryOptimisticProjection/,
  );
  assert.match(
    HOOK_SOURCE,
    /subscribeToReconnects[\s\S]*subscriptionRef\.current\?\.reconnect\(\)/,
  );
  assert.match(HOOK_SOURCE, /isUnsupported:/);
});
