/**
 * Regressions for the live timeline cache merge (BUG-066, memory).
 *
 * `useLiveChannelUpdates` merges every inbound message into the cache of every
 * subscribed channel. The old merge rebuilt the whole array per message —
 * dedupe, find, filter, spread, then dedupe again plus an O(M log M) sort —
 * and nothing capped M. Both claims are asserted by COUNTING OPERATIONS
 * (comparator calls) and array length, never by timing.
 */

import assert from "node:assert/strict";
import { describe, it } from "node:test";

import {
  MAX_TIMELINE_CACHE_MESSAGES,
  mergeTimelineCacheMessages,
  mergeMessages,
} from "@/features/messages/lib/messageMerge.ts";
import { sortMessages } from "@/features/messages/lib/messageQueryKeys.ts";

const AUTHOR = "f".repeat(64);

function message(index, overrides = {}) {
  return {
    id: `${index}`.padStart(64, "0"),
    pubkey: AUTHOR,
    created_at: 1_760_000_000 + index,
    kind: 9,
    tags: [["h", "chan-1"]],
    content: `m${index}`,
    sig: "s".repeat(128),
    ...overrides,
  };
}

/**
 * Build a normalized cache the way production does: through `sortMessages`,
 * which is what marks an array as being in known timeline order.
 */
function normalizedCache(count, from = 0) {
  return sortMessages(
    Array.from({ length: count }, (_, index) => message(from + index)),
  );
}

/**
 * Count comparator invocations by counting how often the sort has to look at a
 * pair. `Array.prototype.sort` is the only caller, so a merge that appends
 * without sorting reads zero.
 */
function countSortComparisons(run) {
  const original = Array.prototype.sort;
  let comparisons = 0;
  Array.prototype.sort = function patched(comparator) {
    if (typeof comparator !== "function") return original.call(this);
    return original.call(this, (left, right) => {
      comparisons += 1;
      return comparator(left, right);
    });
  };
  try {
    run();
  } finally {
    Array.prototype.sort = original;
  }
  return comparisons;
}

describe("mergeTimelineCacheMessages insert cost", () => {
  it("appends a strictly newer message without sorting the cache", () => {
    const cache = normalizedCache(500);
    const comparisons = countSortComparisons(() => {
      mergeTimelineCacheMessages(cache, message(500));
    });
    assert.equal(
      comparisons,
      0,
      `appending the newest message ran ${comparisons} comparisons; it should run none`,
    );
  });

  it("keeps append cost at zero comparisons across a long run of messages", () => {
    let cache = normalizedCache(200);
    const comparisons = countSortComparisons(() => {
      for (let index = 200; index < 1200; index += 1) {
        cache = mergeTimelineCacheMessages(cache, message(index));
      }
    });
    assert.equal(comparisons, 0);
    assert.equal(cache.length, 1200);
  });

  it("produces exactly the order a full re-sort would have produced", () => {
    let fast = normalizedCache(50);
    let slow = normalizedCache(50);
    for (let index = 50; index < 120; index += 1) {
      fast = mergeTimelineCacheMessages(fast, message(index));
      // The reference: force the slow path by handing it an unmarked copy.
      slow = mergeTimelineCacheMessages([...slow], message(index));
    }
    assert.deepEqual(
      fast.map((entry) => entry.id),
      slow.map((entry) => entry.id),
    );
  });

  it("falls back to the sorting path for an out-of-order arrival", () => {
    const cache = normalizedCache(100);
    const comparisons = countSortComparisons(() => {
      mergeTimelineCacheMessages(cache, message(50, { id: "z".repeat(64) }));
    });
    assert.ok(
      comparisons > 0,
      "an older message must still be sorted into position, not appended",
    );
  });

  it("falls back for a same-second arrival so the id tiebreak still decides", () => {
    const newest = message(9, { id: "b".repeat(64) });
    const cache = sortMessages([...normalizedCache(9), newest]);
    // Same second as the newest row, but an id that sorts BEFORE it. Appending
    // would put it last; only the tiebreaking sort puts it in the right place.
    const tie = message(9, { id: "a".repeat(64), content: "tie" });
    const merged = mergeTimelineCacheMessages(cache, tie);
    assert.deepEqual(
      merged.map((entry) => entry.id),
      sortMessages([...cache, tie]).map((entry) => entry.id),
    );
    assert.equal(merged.at(-1).id, "b".repeat(64));
  });

  it("falls back for a duplicate id rather than appending a second copy", () => {
    const cache = normalizedCache(10);
    const merged = mergeTimelineCacheMessages(cache, {
      ...message(9),
      content: "edited",
    });
    assert.equal(merged.length, 10);
    assert.equal(merged.at(-1).content, "edited");
  });

  it("falls back when the arrival acknowledges a pending local send", () => {
    const pending = message(20, {
      id: "p".repeat(64),
      pending: true,
      localKey: "local-20",
    });
    const cache = sortMessages([...normalizedCache(20), pending]);
    const acknowledged = message(21, { id: "q".repeat(64), content: "m20" });
    const merged = mergeTimelineCacheMessages(cache, acknowledged);
    assert.equal(
      merged.some((entry) => entry.pending),
      false,
      "the pending row must be replaced by its acknowledgement, not kept alongside it",
    );
    assert.equal(merged.at(-1).localKey, "local-20");
  });

  it("does not append into an array of unknown provenance", () => {
    // A relay page or a window projection is not in known timeline order, so
    // taking the append path on it would publish an unsorted cache.
    const unsorted = [message(5), message(1), message(3)];
    const merged = mergeTimelineCacheMessages(unsorted, message(9));
    assert.deepEqual(
      merged.map((entry) => entry.content),
      ["m1", "m3", "m5", "m9"],
    );
  });
});

describe("mergeTimelineCacheMessages retention", () => {
  it("caps the retained cache instead of growing for the whole session", () => {
    let cache = normalizedCache(1);
    for (let index = 1; index < MAX_TIMELINE_CACHE_MESSAGES + 500; index += 1) {
      cache = mergeTimelineCacheMessages(cache, message(index));
    }
    assert.equal(cache.length, MAX_TIMELINE_CACHE_MESSAGES);
  });

  it("drops the oldest rows, never the newest", () => {
    let cache = normalizedCache(1);
    const total = MAX_TIMELINE_CACHE_MESSAGES + 10;
    for (let index = 1; index < total; index += 1) {
      cache = mergeTimelineCacheMessages(cache, message(index));
    }
    assert.equal(cache.at(-1).content, `m${total - 1}`);
    assert.equal(
      cache.at(0).content,
      `m${total - MAX_TIMELINE_CACHE_MESSAGES}`,
    );
  });

  it("never drops a pending local send, however far it falls behind", () => {
    const pending = message(0, {
      id: "p".repeat(64),
      pending: true,
      localKey: "local-0",
    });
    let cache = sortMessages([pending]);
    for (let index = 1; index < MAX_TIMELINE_CACHE_MESSAGES + 400; index += 1) {
      cache = mergeTimelineCacheMessages(cache, message(index));
    }
    assert.equal(
      cache.some((entry) => entry.id === "p".repeat(64)),
      true,
      "an unacknowledged local send was trimmed away; nothing can bring it back",
    );
  });

  it("caps the slow path too, not only the append path", () => {
    let cache = normalizedCache(MAX_TIMELINE_CACHE_MESSAGES);
    // Unmarked copy forces the sorting path on every merge.
    for (let index = 0; index < 20; index += 1) {
      cache = mergeTimelineCacheMessages(
        [...cache],
        message(MAX_TIMELINE_CACHE_MESSAGES + index),
      );
    }
    assert.equal(cache.length, MAX_TIMELINE_CACHE_MESSAGES);
  });

  it("leaves mergeMessages uncapped — thread replies are not the timeline cache", () => {
    let replies = normalizedCache(1);
    for (let index = 1; index < MAX_TIMELINE_CACHE_MESSAGES + 50; index += 1) {
      replies = mergeMessages(replies, message(index));
    }
    assert.equal(replies.length, MAX_TIMELINE_CACHE_MESSAGES + 50);
  });
});
