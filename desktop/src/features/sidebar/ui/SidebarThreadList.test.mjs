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
  localRenameDraftTransition,
  threadDirectoryActionPatch,
  threadDirectoryCanTogglePin,
  threadDirectoryNavigationSearch,
  threadDirectoryRenameIsValid,
  threadDirectoryRenameValidationMessage,
  stopSidebarThreadInteraction,
} from "./SidebarThreadList.tsx";
import {
  localThreadNameIsValid,
  resolveLocalThreadName,
} from "../lib/localThreadNames.ts";

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

const LEGACY_ITEMS = [
  { rootId: ROOT_ID, title: "Derived first line", lastReplyAt: 200 },
];

function renderLegacy(overrides = {}) {
  return renderToStaticMarkup(
    React.createElement(LegacySidebarThreadList, {
      items: LEGACY_ITEMS,
      onNavigate() {},
      ...overrides,
    }),
  );
}

/** The unsupported-relay branch, where the machine-local rename dialog lives. */
const LOCAL_BRANCH_SOURCE = UI_SOURCE.slice(
  UI_SOURCE.indexOf("if (directory.isUnsupported)"),
  UI_SOURCE.indexOf("<fieldset"),
);

test("the local rename control renders with a label and a keyboard-reachable style", () => {
  const html = renderLegacy({ onRename() {} });
  assert.match(html, /aria-label="Rename Derived first line"/);
  // `hidden` is display:none, which drops the button out of the tab order
  // entirely — the focus ring can never fire and a keyboard-only user can
  // never rename a thread. Reveal must be opacity-based and must respond to
  // focus as well as hover.
  const pencil =
    /<button[^>]*aria-label="Rename Derived first line"[^>]*>/.exec(html)[0];
  const pencilClasses = /class="([^"]*)"/.exec(pencil)[1].split(/\s+/);
  assert.ok(
    !pencilClasses.includes("hidden"),
    "display:none removes the rename control from the tab order",
  );
  assert.ok(pencilClasses.includes("group-hover/thread:opacity-100"));
  assert.ok(pencilClasses.includes("group-focus-within/thread:opacity-100"));
  assert.ok(pencilClasses.includes("focus-visible:ring-2"));

  // No rename callback (read-only contexts) means no control at all.
  assert.doesNotMatch(renderLegacy(), /aria-label="Rename /);
});

test("the local rename control reserves its column instead of covering the title", () => {
  // The pencil is absolutely positioned at right-1 over a 24px box. Without a
  // right-padding reserve a long title's ellipsis renders underneath it, the
  // same reason ThreadDirectoryRow uses pl-2 pr-8.
  const withRename = /<button[^>]*class="([^"]*)"[^>]*type="button"/.exec(
    renderLegacy({ onRename() {} }),
  )[1];
  assert.match(withRename, /\bpr-8\b/);
  assert.doesNotMatch(withRename, /\bpx-2\b/);
});

test("a machine-local name replaces the derived label everywhere it is shown", () => {
  const html = renderLegacy({
    localNames: { [ROOT_ID]: "Dungeon planning" },
    onRename() {},
  });
  assert.match(html, /Dungeon planning/);
  assert.doesNotMatch(html, /Derived first line/);
  // The tooltip, the visible text, and the rename control's label all follow.
  assert.match(html, /title="Dungeon planning"/);
  assert.match(html, /aria-label="Rename Dungeon planning"/);

  // A name set on a different thread must not bleed onto this row.
  const otherOnly = renderLegacy({
    localNames: { ["f".repeat(64)]: "Somebody else's label" },
  });
  assert.match(otherOnly, /Derived first line/);
  assert.doesNotMatch(otherOnly, /Somebody else's label/);
});

test("a shared thread name still wins over a machine-local one", () => {
  // The shared name is what everyone in the channel sees. A relay that can
  // serve the directory renders it, and the local override never applies.
  assert.equal(resolveLocalThreadName("Shared", "Local", "Derived"), "Shared");
  const itemHtml = renderResults({ items: [ITEM] });
  assert.match(itemHtml, /Shared thread title/);
  assert.doesNotMatch(itemHtml, /Generated thread title/);
  // The directory row has no local-name input at all, structurally.
  const directoryRowSource = UI_SOURCE.slice(
    UI_SOURCE.indexOf("export function ThreadDirectoryRow"),
    UI_SOURCE.indexOf("export function ThreadDirectoryResults"),
  );
  assert.ok(directoryRowSource.length > 0);
  assert.doesNotMatch(directoryRowSource, /localNames/);
});

test("the local rename draft never carries one thread's text onto another", () => {
  const first = localRenameDraftTransition(null, {
    type: "open",
    rootId: ROOT_ID,
    currentName: "Half-typed name",
  });
  assert.deepEqual(first, { rootId: ROOT_ID, name: "Half-typed name" });

  const otherRoot = "f".repeat(64);
  const second = localRenameDraftTransition(first, {
    type: "open",
    rootId: otherRoot,
    currentName: "",
  });
  assert.deepEqual(
    second,
    { rootId: otherRoot, name: "" },
    "reopening rebuilds the draft from the row that was clicked",
  );

  // Cancel/dismiss discards the draft rather than leaving it to reappear.
  assert.equal(localRenameDraftTransition(second, { type: "close" }), null);
  assert.equal(localRenameDraftTransition(null, { type: "close" }), null);

  // And the component routes both through it instead of setting state inline.
  assert.match(
    LOCAL_BRANCH_SOURCE,
    /onRename=\{[\s\S]*localRenameDraftTransition\(draft, \{\s*type: "open"/,
  );
  assert.equal(
    (
      LOCAL_BRANCH_SOURCE.match(
        /localRenameDraftTransition\(draft, \{ type: "close" \}\)/g,
      ) ?? []
    ).length,
    2,
    "both the dialog dismiss and the Cancel button clear the draft",
  );
});

test("the local rename dialog is labelled and announces its own error", () => {
  // The sibling shared dialog in this file does all of this; the local one
  // shipped with a bare Input and an unannounced validation paragraph.
  assert.match(LOCAL_BRANCH_SOURCE, /htmlFor=\{renameInputId\}/);
  assert.match(LOCAL_BRANCH_SOURCE, /id=\{renameInputId\}/);
  assert.match(
    LOCAL_BRANCH_SOURCE,
    /aria-describedby=\{[\s\S]*renameValidationId/,
  );
  assert.match(LOCAL_BRANCH_SOURCE, /aria-invalid=\{/);
  assert.match(
    LOCAL_BRANCH_SOURCE,
    /id=\{renameValidationId\}\s*\n\s*role="alert"/,
  );
  // maxLength counts UTF-16 units, so a 120 cap stops an emoji typist at 60.
  assert.match(
    LOCAL_BRANCH_SOURCE,
    /maxLength=\{MAX_LOCAL_THREAD_NAME_LENGTH \* 2\}/,
  );
});

test("the local name validator agrees with the shared rename validator", () => {
  // A local name is promotable to a shared one the day the relay supports it,
  // so the two rules must not disagree in either direction.
  const corpus = [
    "Normal name",
    "",
    "   ",
    "line one\nline two",
    "bell\u0007",
    "line\u2028separator",
    "paragraph\u2029separator",
    "next\u0085line",
    "csi\u009bescape",
    "delete\u007fcharacter",
    "lone \ud800 surrogate",
    "🎲".repeat(100),
    "🎲".repeat(120),
    "🎲".repeat(121),
    "x".repeat(120),
    "x".repeat(121),
  ];
  for (const value of corpus) {
    assert.equal(
      localThreadNameIsValid(value),
      threadDirectoryRenameValidationMessage(value) === null,
      `validators disagree on ${JSON.stringify(value)}`,
    );
  }
  // Spot-check the two directions the review actually caught.
  assert.equal(localThreadNameIsValid("csi\u009bescape"), false);
  assert.equal(localThreadNameIsValid("🎲".repeat(100)), true);
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
