/**
 * Regressions for the channel-scoped archive event window (BUG-066, memory).
 *
 * Two independent claims, both asserted as OPERATION COUNTS rather than as
 * elapsed time — a timing threshold can be widened until it passes, a call
 * count cannot:
 *
 * 1. Hydrating a channel costs a bounded number of `compareObserverEvents`
 *    calls, not one full window sort per arriving frame.
 * 2. The retained total is bounded by dropping WHOLE cold channels, and a
 *    channel a component is reading is never one of them.
 */

import assert from "node:assert/strict";
import { beforeEach, describe, it } from "node:test";

import {
  MAX_ARCHIVED_EVENTS_RETAINED,
  appendArchivedChannelEvent,
  clearArchivedChannelEvents,
  readArchivedChannelEvents,
  retainArchivedChannel,
  _testGetEvictedChannelCount,
  _testGetRetainedArchivedEventCount,
} from "@/features/agents/archiveEventWindow.ts";
import {
  _testGetObserverComparisonCount,
  _testResetObserverComparisonCount,
} from "@/features/agents/observerEventOrder.ts";

const AGENT = "a".repeat(64);

function key(channelId) {
  return `${AGENT}:${channelId}`;
}

function event(seq) {
  return {
    seq,
    timestamp: new Date(1_760_000_000_000 + seq * 1000).toISOString(),
    kind: "acp_write",
    agentIndex: 0,
    channelId: "chan-1",
    sessionId: "sess-1",
    turnId: "turn-1",
    payload: {},
  };
}

/** Archive pages arrive newest-first from SQLite, one frame at a time. */
function hydratePage(channelId, fromSeq, count) {
  for (let index = 0; index < count; index += 1) {
    appendArchivedChannelEvent(
      key(channelId),
      channelId,
      event(fromSeq - index),
    );
  }
}

beforeEach(() => {
  clearArchivedChannelEvents();
  _testResetObserverComparisonCount();
});

describe("archive window insert cost", () => {
  it("hydrates ten pages without a full re-sort per archived frame", () => {
    // The old implementation sorted the whole window on every append, so
    // 2,000 frames cost on the order of 2,000 * 2,000 comparisons. The bound
    // below is far above what the batched merge needs and far below what a
    // per-frame sort costs, so it fails loudly if per-frame sorting returns.
    const PAGES = 10;
    const PAGE_SIZE = 200;
    for (let page = 0; page < PAGES; page += 1) {
      hydratePage("chan-1", 2000 - page * PAGE_SIZE, PAGE_SIZE);
      // The UI reads once per page: ingest batches its notification.
      readArchivedChannelEvents(key("chan-1"));
    }

    assert.equal(readArchivedChannelEvents(key("chan-1")).length, 2000);
    assert.ok(
      _testGetObserverComparisonCount() < 200_000,
      `hydration used ${_testGetObserverComparisonCount()} comparisons; a per-frame sort uses millions`,
    );
  });

  it("costs zero comparisons when a whole page is appended without a read", () => {
    hydratePage("chan-1", 200, 200);
    assert.equal(_testGetObserverComparisonCount(), 0);
    // Publishing is what sorts, and it happens once.
    readArchivedChannelEvents(key("chan-1"));
    assert.ok(_testGetObserverComparisonCount() > 0);
  });

  it("still returns the window in ascending order after batched publish", () => {
    hydratePage("chan-1", 50, 50);
    const events = readArchivedChannelEvents(key("chan-1"));
    assert.equal(events.length, 50);
    for (let index = 1; index < events.length; index += 1) {
      assert.ok(events[index - 1].seq < events[index].seq);
    }
  });

  it("deduplicates on (seq, timestamp) without scanning the window", () => {
    hydratePage("chan-1", 100, 100);
    assert.equal(
      appendArchivedChannelEvent(key("chan-1"), "chan-1", event(50)),
      false,
    );
    assert.equal(
      appendArchivedChannelEvent(key("chan-1"), "chan-1", event(500)),
      true,
    );
    assert.equal(readArchivedChannelEvents(key("chan-1")).length, 101);
  });

  it("returns the same array reference until something is appended", () => {
    hydratePage("chan-1", 10, 10);
    const first = readArchivedChannelEvents(key("chan-1"));
    assert.equal(readArchivedChannelEvents(key("chan-1")), first);
    appendArchivedChannelEvent(key("chan-1"), "chan-1", event(99));
    assert.notEqual(readArchivedChannelEvents(key("chan-1")), first);
  });
});

describe("archive window retention", () => {
  it("drops whole cold channels once the retained total exceeds the budget", () => {
    const perChannel = 2000;
    const channels = Math.ceil(MAX_ARCHIVED_EVENTS_RETAINED / perChannel) + 2;
    for (let index = 0; index < channels; index += 1) {
      hydratePage(`chan-${index}`, perChannel, perChannel);
    }

    assert.ok(_testGetEvictedChannelCount() > 0, "nothing was evicted");
    assert.ok(
      _testGetRetainedArchivedEventCount() <= MAX_ARCHIVED_EVENTS_RETAINED,
      `retained ${_testGetRetainedArchivedEventCount()} events, budget is ${MAX_ARCHIVED_EVENTS_RETAINED}`,
    );
  });

  it("evicts a channel whole, never partially — absent or complete, never short", () => {
    const perChannel = 2000;
    const channels = Math.ceil(MAX_ARCHIVED_EVENTS_RETAINED / perChannel) + 2;
    for (let index = 0; index < channels; index += 1) {
      hydratePage(`chan-${index}`, perChannel, perChannel);
      // Publish as the UI would, so a trim that cuts into an already-published
      // window is visible here instead of hiding in the pending buffer.
      readArchivedChannelEvents(key(`chan-${index}`));
    }

    for (let index = 0; index < channels; index += 1) {
      const length = readArchivedChannelEvents(key(`chan-${index}`)).length;
      assert.ok(
        length === 0 || length === perChannel,
        `chan-${index} was truncated to ${length}: a partially evicted channel reads as missing history`,
      );
    }
  });

  it("never evicts a channel a mounted reader has retained", () => {
    const perChannel = 2000;
    hydratePage("chan-pinned", perChannel, perChannel);
    const release = retainArchivedChannel("chan-pinned");

    const channels = Math.ceil(MAX_ARCHIVED_EVENTS_RETAINED / perChannel) + 3;
    for (let index = 0; index < channels; index += 1) {
      hydratePage(`chan-${index}`, perChannel, perChannel);
    }

    assert.equal(
      readArchivedChannelEvents(key("chan-pinned")).length,
      perChannel,
      "the channel on screen lost history underneath its reader",
    );

    // Once released it is an ordinary eviction candidate again.
    release();
    for (let index = 0; index < channels; index += 1) {
      hydratePage(`late-${index}`, perChannel, perChannel);
    }
    assert.equal(readArchivedChannelEvents(key("chan-pinned")).length, 0);
  });

  it("refcounts retention so two readers of one channel both have to release", () => {
    const perChannel = 2000;
    hydratePage("chan-pinned", perChannel, perChannel);
    const releaseA = retainArchivedChannel("chan-pinned");
    const releaseB = retainArchivedChannel("chan-pinned");
    releaseA();

    const channels = Math.ceil(MAX_ARCHIVED_EVENTS_RETAINED / perChannel) + 3;
    for (let index = 0; index < channels; index += 1) {
      hydratePage(`chan-${index}`, perChannel, perChannel);
    }
    assert.equal(
      readArchivedChannelEvents(key("chan-pinned")).length,
      perChannel,
      "the second reader's retention was ignored",
    );
    releaseB();
  });

  it("counts down again when an evicted channel's events are released", () => {
    const perChannel = 2000;
    const channels = Math.ceil(MAX_ARCHIVED_EVENTS_RETAINED / perChannel) + 4;
    for (let index = 0; index < channels; index += 1) {
      hydratePage(`chan-${index}`, perChannel, perChannel);
    }
    // A leaked counter would keep climbing past the budget forever; the store
    // would then evict on every single append.
    assert.ok(
      _testGetRetainedArchivedEventCount() >= perChannel,
      "the retained counter under-counted after eviction",
    );
    assert.ok(
      _testGetRetainedArchivedEventCount() <= MAX_ARCHIVED_EVENTS_RETAINED,
    );
  });
});
