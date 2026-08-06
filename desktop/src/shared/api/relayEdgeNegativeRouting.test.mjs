/**
 * Negative routing gate — SPEC-2026-08-05 test 16.
 *
 * The sidecar may carry persistent kind-9 channel messages and nothing else.
 * Every other class of traffic must provably resolve to canonical-only. These
 * assertions run against the real filter builders `RelayClient` uses, so a
 * future filter change that widens what reaches the sidecar fails here rather
 * than shipping.
 */
import assert from "node:assert/strict";
import test from "node:test";

import { splitEdgeMessageFilter } from "./relayEdgeRouting.ts";
import {
  buildChannelAuxDeletionFilter,
  buildChannelFilter,
  buildChannelHistoryFilter,
  buildChannelMentionFilter,
  buildGlobalStreamFilter,
} from "./relayChannelFilters.ts";
import {
  CHANNEL_EVENT_KINDS,
  KIND_CHANNEL_THREAD_SUMMARY,
  KIND_STREAM_MESSAGE,
  KIND_TYPING_INDICATOR,
  KIND_USER_STATUS,
} from "@/shared/constants/kinds";

const CHANNEL = "550e8400-e29b-41d4-a716-446655440000";
const PUBKEY = "a".repeat(64);

/** Filters `RelayClient` builds that must never reach the sidecar at all. */
const CANONICAL_ONLY = [
  ["presence", { kinds: [20001], limit: 0 }],
  [
    "typing indicators",
    {
      kinds: [KIND_TYPING_INDICATOR],
      "#h": [CHANNEL],
      limit: 10,
      since: 1,
    },
  ],
  ["user status", { kinds: [KIND_USER_STATUS], "#d": ["general"], limit: 0 }],
  [
    "huddle lifecycle",
    { kinds: [48100, 48101, 48102, 48103], "#h": [CHANNEL], limit: 100 },
  ],
  ["global stream (no channel scope)", buildGlobalStreamFilter(50)],
  [
    "aux deletions (id-scoped, not channel-scoped)",
    buildChannelAuxDeletionFilter(CHANNEL, ["b".repeat(64)]),
  ],
];

for (const [label, filter] of CANONICAL_ONLY) {
  test(`${label} never reaches the sidecar`, () => {
    assert.equal(
      splitEdgeMessageFilter(filter),
      null,
      `${label} must resolve to canonical-only`,
    );
  });
}

test("channel subscriptions hand the sidecar messages and nothing else", () => {
  for (const filter of [
    buildChannelFilter(CHANNEL, 50),
    buildChannelHistoryFilter(CHANNEL, 50),
    buildChannelMentionFilter(CHANNEL, PUBKEY, 50),
    {
      kinds: [...CHANNEL_EVENT_KINDS, KIND_CHANNEL_THREAD_SUMMARY],
      "#h": [CHANNEL],
      limit: 1000,
      since: 1,
    },
  ]) {
    const split = splitEdgeMessageFilter(filter);
    assert.ok(split, "a channel-scoped message filter must split");
    assert.deepEqual(
      split.edge.kinds,
      [KIND_STREAM_MESSAGE],
      "the sidecar half must be exactly kind 9",
    );
    assert.equal(
      split.canonical?.kinds.includes(KIND_STREAM_MESSAGE) ?? false,
      false,
      "kind 9 must not also be requested canonically",
    );
    // Every other kind the caller asked for still has to be served.
    const covered = [...split.edge.kinds, ...(split.canonical?.kinds ?? [])];
    assert.deepEqual(
      [...covered].sort((a, b) => a - b),
      [...filter.kinds].sort((a, b) => a - b),
      "the split must lose no kind",
    );
  }
});

test("scope-bearing fields survive into both halves", () => {
  const split = splitEdgeMessageFilter({
    kinds: [KIND_STREAM_MESSAGE, 7],
    "#h": [CHANNEL],
    "#p": [PUBKEY],
    limit: 50,
    since: 100,
    until: 200,
  });
  for (const half of [split.edge, split.canonical]) {
    assert.deepEqual(half["#h"], [CHANNEL]);
    assert.deepEqual(half["#p"], [PUBKEY]);
    assert.equal(half.limit, 50);
    assert.equal(half.since, 100);
    assert.equal(half.until, 200);
  }
});

test("a multi-channel filter is only eligible when every channel is a UUID", () => {
  const other = "6ba7b810-9dad-11d1-80b4-00c04fd430c8";
  assert.ok(
    splitEdgeMessageFilter({
      kinds: [KIND_STREAM_MESSAGE],
      "#h": [CHANNEL, other],
      limit: 50,
    }),
  );
  assert.equal(
    splitEdgeMessageFilter({
      kinds: [KIND_STREAM_MESSAGE],
      "#h": [CHANNEL, "legacy-slug"],
      limit: 50,
    }),
    null,
  );
});
