import assert from "node:assert/strict";
import test from "node:test";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import React, { act } from "react";
import { createRoot } from "react-dom/client";

import { relayClient } from "@/shared/api/relayClient";
import { KIND_THREAD_DIRECTORY_BOUNDS } from "@/shared/constants/kinds";
import {
  threadDirectoryLiveQueryKey,
  threadDirectoryQueryKey,
} from "./lib/threadDirectory.ts";
import { useThreadDirectory } from "./useThreadDirectory.ts";

class MinimalEventTarget {
  listeners = new Map();

  addEventListener(type, listener) {
    const listeners = this.listeners.get(type) ?? [];
    listeners.push(listener);
    this.listeners.set(type, listeners);
  }

  removeEventListener(type, listener) {
    this.listeners.set(
      type,
      (this.listeners.get(type) ?? []).filter(
        (candidate) => candidate !== listener,
      ),
    );
  }
}

class MinimalNode extends MinimalEventTarget {
  constructor(tagName, nodeType = 1) {
    super();
    this.tagName = tagName;
    this.nodeType = nodeType;
    this.parentNode = null;
    this.children = [];
    this.childNodes = this.children;
    this.style = {};
  }

  get ownerDocument() {
    return globalThis.document;
  }

  get firstChild() {
    return this.children[0] ?? null;
  }

  get lastChild() {
    return this.children.at(-1) ?? null;
  }

  get nextSibling() {
    return null;
  }

  appendChild(child) {
    this.children.push(child);
    child.parentNode = this;
    return child;
  }

  removeChild(child) {
    this.children = this.children.filter((candidate) => candidate !== child);
    this.childNodes = this.children;
    child.parentNode = null;
    return child;
  }

  insertBefore(child, reference) {
    const index = this.children.indexOf(reference);
    if (index === -1) return this.appendChild(child);
    this.children.splice(index, 0, child);
    child.parentNode = this;
    return child;
  }

  contains(node) {
    return (
      this === node ||
      this.children.some((child) => child === node || child.contains?.(node))
    );
  }
}

class MinimalDocument extends MinimalEventTarget {
  nodeType = 9;

  createElement(tagName) {
    return new MinimalNode(tagName);
  }

  createTextNode(value) {
    const node = new MinimalNode("#text", 3);
    node.nodeValue = value;
    return node;
  }

  createComment(value) {
    const node = new MinimalNode("#comment", 8);
    node.nodeValue = value;
    return node;
  }

  get activeElement() {
    return null;
  }
}

globalThis.document = new MinimalDocument();
globalThis.HTMLIFrameElement = MinimalNode;
globalThis.HTMLElement = MinimalNode;
globalThis.IS_REACT_ACT_ENVIRONMENT = true;
globalThis.window = globalThis;
Object.defineProperty(globalThis, "navigator", {
  value: { userAgent: "node" },
  configurable: true,
});

const COMMUNITY_ID = "community";
const RELAY_URL = "wss://relay";
const PUBKEY = "c".repeat(64);
const FIRST_CHANNEL_ID = "36411e44-0e2d-4cfe-bd6e-567eb169db9f";
const SIBLING_CHANNEL_ID = "4c411e44-0e2d-4cfe-bd6e-567eb169db9f";
const ROOT_ID = "a".repeat(64);

const EMPTY_PAGE = { pages: [], pageParams: [] };
const EMPTY_LIVE = { nextOrder: 0, byRootId: new Map() };
const ARCHIVED_ITEM = {
  rootId: ROOT_ID,
  channelId: FIRST_CHANNEL_ID,
  title: "Generated title",
  titleOverride: null,
  rootAuthor: PUBKEY,
  rootCreatedAt: 100,
  replyCount: 3,
  descendantCount: 3,
  lastReplyAt: 200,
  participants: [PUBKEY],
  pinned: false,
  archived: true,
  present: true,
  stateCreatedAt: 150,
  stateEventId: "b".repeat(64),
  projectionCreatedAt: 200,
  projectionEventId: "d".repeat(64),
};
const ARCHIVED_LIVE = {
  nextOrder: 1,
  byRootId: new Map([
    [ROOT_ID, { item: ARCHIVED_ITEM, source: "live", sourceOrder: 1 }],
  ]),
};
const ACTIVE_ITEM = { ...ARCHIVED_ITEM, archived: false };
const ACTIVE_LIVE = {
  nextOrder: 1,
  byRootId: new Map([
    [ROOT_ID, { item: ACTIVE_ITEM, source: "live", sourceOrder: 1 }],
  ]),
};

function Harness({
  channelId,
  state = "active",
  enabled = false,
  onDirectory,
  onItems,
}) {
  const directory = useThreadDirectory({
    channelId,
    communityId: COMMUNITY_ID,
    relayUrl: RELAY_URL,
    pubkey: PUBKEY,
    state,
    enabled,
  });
  onDirectory?.(directory);
  onItems?.(directory.items);
  return null;
}

function waitForTask() {
  return new Promise((resolve) => setTimeout(resolve, 0));
}

async function waitForCondition(predicate) {
  for (let attempt = 0; attempt < 50; attempt += 1) {
    if (predicate()) return;
    await act(async () => waitForTask());
  }
  assert.fail("condition was not met");
}

test("capability cache follows unsupported and supported reconnect transitions", async () => {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  const container = document.createElement("div");
  const root = createRoot(container);
  let currentDirectory;
  let invokeCount = 0;
  let generation = 10;
  let reconnectListener = null;
  let supportsDirectory = false;
  const originalGetConnectionGeneration =
    relayClient.getConnectionGeneration.bind(relayClient);
  const originalSubscribeToReconnects =
    relayClient.subscribeToReconnects.bind(relayClient);
  const originalSubscribeToThreadDirectory =
    relayClient.subscribeToThreadDirectory.bind(relayClient);
  relayClient.getConnectionGeneration = () => generation;
  relayClient.subscribeToReconnects = (listener) => {
    reconnectListener = listener;
    return () => {
      if (reconnectListener === listener) reconnectListener = null;
    };
  };
  relayClient.subscribeToThreadDirectory = () =>
    Promise.resolve(async () => {});
  globalThis.window.__TAURI_INTERNALS__ = {
    invoke(command, args) {
      assert.equal(command, "get_thread_directory");
      invokeCount += 1;
      if (!supportsDirectory) throw new Error("command failed");
      return Promise.resolve([
        {
          id: "f".repeat(64),
          pubkey: PUBKEY,
          created_at: 200,
          kind: KIND_THREAD_DIRECTORY_BOUNDS,
          tags: [
            ["d", `${args.channelId}:active:head`],
            ["h", args.channelId],
          ],
          content: JSON.stringify({
            has_more: false,
            next_cursor: null,
          }),
          sig: "sig",
        },
      ]);
    },
  };

  const renderHarness = (channelId) =>
    root.render(
      React.createElement(
        QueryClientProvider,
        { client },
        React.createElement(Harness, {
          channelId,
          enabled: true,
          onDirectory(directory) {
            currentDirectory = directory;
          },
        }),
      ),
    );

  try {
    await act(async () => {
      renderHarness(FIRST_CHANNEL_ID);
    });
    await waitForCondition(() => currentDirectory?.isUnsupported === true);
    assert.equal(currentDirectory.error, null);
    assert.equal(invokeCount, 1);

    await act(async () => {
      renderHarness(SIBLING_CHANNEL_ID);
      await waitForTask();
    });
    assert.equal(currentDirectory.isUnsupported, true);
    assert.equal(invokeCount, 1, "cached verdict must suppress sibling probes");

    supportsDirectory = true;
    generation += 1;
    await act(async () => {
      reconnectListener();
    });
    await waitForCondition(
      () =>
        currentDirectory?.isUnsupported === false &&
        currentDirectory?.isSuccess === true,
    );
    assert.equal(invokeCount, 2);
    assert.equal(currentDirectory.error, null);

    generation += 1;
    await act(async () => {
      reconnectListener();
    });
    await waitForCondition(() => invokeCount === 3);
    assert.equal(currentDirectory.isUnsupported, false);
    assert.equal(currentDirectory.error, null);

    supportsDirectory = false;
    generation += 1;
    await act(async () => {
      reconnectListener();
    });
    await waitForCondition(() => currentDirectory?.isUnsupported === true);
    assert.equal(invokeCount, 4);
    assert.equal(currentDirectory.error, null);

    await act(async () => {
      renderHarness(FIRST_CHANNEL_ID);
      await waitForTask();
    });
    assert.equal(currentDirectory.isUnsupported, true);
    assert.equal(
      invokeCount,
      4,
      "current unsupported verdict must suppress sibling probes",
    );
  } finally {
    await act(async () => root.unmount());
    relayClient.getConnectionGeneration = originalGetConnectionGeneration;
    relayClient.subscribeToReconnects = originalSubscribeToReconnects;
    relayClient.subscribeToThreadDirectory = originalSubscribeToThreadDirectory;
    delete globalThis.window.__TAURI_INTERNALS__;
    client.clear();
  }
});

test("missing bounds selects fallback without surfacing a query error", async () => {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  const container = document.createElement("div");
  const root = createRoot(container);
  let currentDirectory;
  const originalSubscribeToReconnects =
    relayClient.subscribeToReconnects.bind(relayClient);
  const originalSubscribeToThreadDirectory =
    relayClient.subscribeToThreadDirectory.bind(relayClient);
  relayClient.subscribeToReconnects = () => () => {};
  relayClient.subscribeToThreadDirectory = () =>
    Promise.resolve(async () => {});
  globalThis.window.__TAURI_INTERNALS__ = {
    invoke(command) {
      assert.equal(command, "get_thread_directory");
      return Promise.resolve([]);
    },
  };
  try {
    await act(async () => {
      root.render(
        React.createElement(
          QueryClientProvider,
          { client },
          React.createElement(Harness, {
            channelId: FIRST_CHANNEL_ID,
            enabled: true,
            onDirectory(directory) {
              currentDirectory = directory;
            },
          }),
        ),
      );
    });
    await waitForCondition(() => currentDirectory?.isUnsupported === true);
    assert.equal(currentDirectory.error, null);
    assert.deepEqual(currentDirectory.items, []);
  } finally {
    await act(async () => root.unmount());
    relayClient.subscribeToReconnects = originalSubscribeToReconnects;
    relayClient.subscribeToThreadDirectory = originalSubscribeToThreadDirectory;
    delete globalThis.window.__TAURI_INTERNALS__;
    client.clear();
  }
});

function boundsResponse(channelId, { hasMore = false, cursor = null } = {}) {
  return [
    {
      id: "f".repeat(64),
      pubkey: PUBKEY,
      created_at: 200,
      kind: KIND_THREAD_DIRECTORY_BOUNDS,
      tags: [
        ["d", `${channelId}:active:${cursor ?? "head"}`],
        ["h", channelId],
      ],
      content: JSON.stringify({
        has_more: hasMore,
        next_cursor: hasMore ? "cursor-2" : null,
      }),
      sig: "sig",
    },
  ];
}

async function renderDirectoryHarness({ invoke, onDirectory }) {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  const container = document.createElement("div");
  const root = createRoot(container);
  const originalSubscribeToReconnects =
    relayClient.subscribeToReconnects.bind(relayClient);
  const originalSubscribeToThreadDirectory =
    relayClient.subscribeToThreadDirectory.bind(relayClient);
  relayClient.subscribeToReconnects = () => () => {};
  relayClient.subscribeToThreadDirectory = () =>
    Promise.resolve(async () => {});
  globalThis.window.__TAURI_INTERNALS__ = { invoke };
  await act(async () => {
    root.render(
      React.createElement(
        QueryClientProvider,
        { client },
        React.createElement(Harness, {
          channelId: FIRST_CHANNEL_ID,
          enabled: true,
          onDirectory,
        }),
      ),
    );
  });
  return async () => {
    await act(async () => root.unmount());
    relayClient.subscribeToReconnects = originalSubscribeToReconnects;
    relayClient.subscribeToThreadDirectory = originalSubscribeToThreadDirectory;
    delete globalThis.window.__TAURI_INTERNALS__;
    client.clear();
  };
}

test("a transient relay failure stays an error and never latches unsupported", async () => {
  for (const message of [
    "relay unreachable: request timed out",
    "relay rate-limited: retry in 4s",
    "relay returned 503 Service Unavailable",
  ]) {
    let currentDirectory;
    const cleanup = await renderDirectoryHarness({
      invoke() {
        return Promise.reject(new Error(message));
      },
      onDirectory(directory) {
        currentDirectory = directory;
      },
    });
    try {
      await waitForCondition(() => currentDirectory?.isError === true);
      assert.equal(
        currentDirectory.isUnsupported,
        false,
        `${message} must not be read as a missing feature`,
      );
      assert.match(currentDirectory.error.message, /relay/);
    } finally {
      await cleanup();
    }
  }
});

test("a later page failure keeps the working directory instead of falling back", async () => {
  let currentDirectory;
  let pageRequests = 0;
  const cleanup = await renderDirectoryHarness({
    invoke(_command, args) {
      pageRequests += 1;
      if (args.cursor === null) {
        return Promise.resolve(
          boundsResponse(args.channelId, { hasMore: true }),
        );
      }
      return Promise.reject(new Error("relay returned 400: bad cursor"));
    },
    onDirectory(directory) {
      currentDirectory = directory;
    },
  });
  try {
    await waitForCondition(() => currentDirectory?.isSuccess === true);
    assert.equal(currentDirectory.isUnsupported, false);
    await act(async () => {
      await currentDirectory.fetchNextPage().catch(() => {});
    });
    await waitForCondition(() => pageRequests === 2);
    assert.equal(
      currentDirectory.isUnsupported,
      false,
      "page two failing must not discard a supported directory",
    );
  } finally {
    await cleanup();
  }
});

test("mounted directory hook disposes only its captured exact scope", async () => {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  const container = document.createElement("div");
  const root = createRoot(container);
  const firstPageKey = threadDirectoryQueryKey(
    COMMUNITY_ID,
    RELAY_URL,
    PUBKEY,
    FIRST_CHANNEL_ID,
    "active",
  );
  const firstLiveKey = threadDirectoryLiveQueryKey(
    COMMUNITY_ID,
    RELAY_URL,
    PUBKEY,
    FIRST_CHANNEL_ID,
  );
  const archivedPageKey = threadDirectoryQueryKey(
    COMMUNITY_ID,
    RELAY_URL,
    PUBKEY,
    FIRST_CHANNEL_ID,
    "archived",
  );
  const siblingKey = threadDirectoryQueryKey(
    COMMUNITY_ID,
    RELAY_URL,
    PUBKEY,
    SIBLING_CHANNEL_ID,
    "active",
  );
  client.setQueryData(firstPageKey, EMPTY_PAGE);
  client.setQueryData(firstLiveKey, EMPTY_LIVE);
  client.setQueryData(siblingKey, "sibling");

  await act(async () => {
    root.render(
      React.createElement(
        QueryClientProvider,
        { client },
        React.createElement(
          React.StrictMode,
          null,
          React.createElement(Harness, { channelId: FIRST_CHANNEL_ID }),
        ),
      ),
    );
  });
  assert.deepEqual(client.getQueryData(firstPageKey), EMPTY_PAGE);
  assert.deepEqual(client.getQueryData(firstLiveKey), EMPTY_LIVE);

  await act(async () => {
    root.render(
      React.createElement(
        QueryClientProvider,
        { client },
        React.createElement(
          React.StrictMode,
          null,
          React.createElement(Harness, {
            channelId: FIRST_CHANNEL_ID,
            state: "archived",
          }),
        ),
      ),
    );
  });
  await waitForTask();
  assert.equal(client.getQueryData(firstPageKey), undefined);
  assert.deepEqual(client.getQueryData(firstLiveKey), EMPTY_LIVE);
  assert.equal(client.getQueryData(siblingKey), "sibling");

  await act(async () => {
    client.setQueryData(archivedPageKey, EMPTY_PAGE);
  });
  await act(async () => {
    root.unmount();
  });
  await waitForTask();

  assert.equal(client.getQueryData(archivedPageKey), undefined);
  assert.equal(client.getQueryData(firstLiveKey), undefined);
  assert.equal(client.getQueryData(siblingKey), "sibling");
  client.clear();
});

test("one unmount preserves a shared live query until its last observer leaves", async () => {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  const container = document.createElement("div");
  const root = createRoot(container);
  const liveKey = threadDirectoryLiveQueryKey(
    COMMUNITY_ID,
    RELAY_URL,
    PUBKEY,
    FIRST_CHANNEL_ID,
  );
  let archivedItems = [];

  function renderHarnesses(showActive) {
    root.render(
      React.createElement(
        QueryClientProvider,
        { client },
        React.createElement(
          React.StrictMode,
          null,
          React.createElement(
            React.Fragment,
            null,
            showActive
              ? React.createElement(Harness, {
                  key: "active",
                  channelId: FIRST_CHANNEL_ID,
                })
              : null,
            React.createElement(Harness, {
              key: "archived",
              channelId: FIRST_CHANNEL_ID,
              state: "archived",
              onItems(items) {
                archivedItems = items;
              },
            }),
          ),
        ),
      ),
    );
  }

  client.setQueryData(liveKey, EMPTY_LIVE);
  await act(async () => {
    renderHarnesses(true);
  });
  assert.deepEqual(archivedItems, []);

  await act(async () => {
    renderHarnesses(false);
  });
  await waitForTask();
  assert.deepEqual(client.getQueryData(liveKey), EMPTY_LIVE);

  await act(async () => {
    client.setQueryData(liveKey, ARCHIVED_LIVE);
    await waitForTask();
  });
  assert.equal(archivedItems.length, 1);
  assert.equal(archivedItems[0].rootId, ROOT_ID);

  await act(async () => {
    root.unmount();
  });
  await waitForTask();
  assert.equal(client.getQueryData(liveKey), undefined);
  client.clear();
});

test("late mutation rejection cannot recreate a disposed scope", async () => {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  const container = document.createElement("div");
  const root = createRoot(container);
  const pageKey = threadDirectoryQueryKey(
    COMMUNITY_ID,
    RELAY_URL,
    PUBKEY,
    FIRST_CHANNEL_ID,
    "active",
  );
  const liveKey = threadDirectoryLiveQueryKey(
    COMMUNITY_ID,
    RELAY_URL,
    PUBKEY,
    FIRST_CHANNEL_ID,
  );
  const siblingKey = threadDirectoryQueryKey(
    COMMUNITY_ID,
    RELAY_URL,
    PUBKEY,
    SIBLING_CHANNEL_ID,
    "active",
  );
  const signedEvent = {
    id: "e".repeat(64),
    pubkey: PUBKEY,
    created_at: 200,
    kind: 40009,
    tags: [],
    content: "",
    sig: "sig",
  };
  let currentDirectory;
  let rejectPublish;
  const pendingPublish = new Promise((_resolve, reject) => {
    rejectPublish = reject;
  });
  const originalPublishEvent = relayClient.publishEvent;
  relayClient.publishEvent = () => pendingPublish;
  globalThis.window.__TAURI_INTERNALS__ = {
    invoke(command) {
      assert.equal(command, "sign_event");
      return Promise.resolve(JSON.stringify(signedEvent));
    },
  };
  client.setQueryData(pageKey, EMPTY_PAGE);
  client.setQueryData(liveKey, ACTIVE_LIVE);
  client.setQueryData(siblingKey, "sibling");

  try {
    await act(async () => {
      root.render(
        React.createElement(
          QueryClientProvider,
          { client },
          React.createElement(Harness, {
            channelId: FIRST_CHANNEL_ID,
            onDirectory(directory) {
              currentDirectory = directory;
            },
          }),
        ),
      );
    });
    assert.equal(currentDirectory.items.length, 1);

    let updatePromise;
    await act(async () => {
      updatePromise = currentDirectory.updateThread({
        rootId: ROOT_ID,
        patch: { pinned: true },
      });
      await waitForTask();
    });
    await act(async () => {
      root.unmount();
    });
    await waitForTask();
    assert.equal(client.getQueryData(pageKey), undefined);
    assert.equal(client.getQueryData(liveKey), undefined);
    assert.equal(client.getQueryData(siblingKey), "sibling");

    await act(async () => {
      rejectPublish(new Error("denied"));
      await assert.rejects(updatePromise, /denied/);
      await waitForTask();
    });
    assert.equal(client.getQueryData(pageKey), undefined);
    assert.equal(client.getQueryData(liveKey), undefined);
    assert.equal(client.getQueryData(siblingKey), "sibling");
  } finally {
    relayClient.publishEvent = originalPublishEvent;
    delete globalThis.window.__TAURI_INTERNALS__;
    client.clear();
  }
});
