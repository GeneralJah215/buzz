import assert from "node:assert/strict";
import test from "node:test";

import {
  resolveChannelBrowserEnterAction,
  selectOrderedChannels,
  shouldShowCreateRow,
} from "./channelBrowserEnterAction.ts";

/**
 * BUG-043 guardrail suite.
 *
 * These drive the Enter decision directly with a deliberately MISMATCHED
 * immediate/deferred pair — the exact state `useDeferredValue` produces for a
 * frame after a fast keystroke — instead of typing into a real dialog and
 * waiting. A test that types and waits is a test someone can widen: commit
 * 7bcfe7e0a on this repo widened a spec's wait from 5s to 10s and declared the
 * flow correct. There is no wait to widen here; the stale pair is an argument.
 */

function makeChannel(overrides) {
  return {
    id: overrides.name,
    name: "unnamed",
    channelType: "stream",
    visibility: "open",
    description: "",
    topic: null,
    purpose: null,
    memberCount: 0,
    memberPubkeys: [],
    lastMessageAt: null,
    archivedAt: null,
    participants: [],
    participantPubkeys: [],
    isMember: false,
    ttlSeconds: null,
    ttlDeadline: null,
    ...overrides,
  };
}

const design = makeChannel({ name: "design", memberCount: 3, isMember: true });
const general = makeChannel({ name: "general", memberCount: 9 });
const random = makeChannel({ name: "random", memberCount: 1 });
const CHANNELS = [design, general, random];

/** Alphabetical order for the empty query — what a stale list looks like. */
const ALL_ORDERED = [design, general, random];

function enter(overrides) {
  return resolveChannelBrowserEnterAction({
    activeTab: "all",
    canCreate: true,
    channels: CHANNELS,
    query: "",
    renderedChannels: ALL_ORDERED,
    renderedQuery: "",
    selectedIndex: null,
    sort: "alpha",
    ...overrides,
  });
}

// ---------------------------------------------------------------------------
// The bug itself: immediate query vs. deferred list.
// ---------------------------------------------------------------------------

test("enter: a stale list never navigates for a name that does not exist", () => {
  // The user typed a brand-new name and hit Enter before the deferred list
  // caught up, so the list on screen is still the whole (empty-query) roster.
  const action = enter({
    query: "zzz-nonexistent",
    renderedQuery: "",
    renderedChannels: ALL_ORDERED,
  });

  assert.notEqual(
    action.kind,
    "select",
    "Enter must not navigate off a list built for a different query",
  );
  assert.deepEqual(action, { kind: "create", name: "zzz-nonexistent" });
});

test("enter: a stale list selects the live query's match, not the stale first row", () => {
  // "gene" matches #general only. The stale list's first row is #design — the
  // wrong channel is right there waiting to be picked.
  const action = enter({
    query: "gene",
    renderedQuery: "",
    renderedChannels: ALL_ORDERED,
  });

  assert.equal(action.kind, "select");
  assert.equal(action.channel.name, "general");
});

test("enter: the immediate query decides the create row, not the deferred one", () => {
  // Typing "generalx" one key past an exact match. The deferred query is still
  // "general", which IS an exact match and would therefore hide the create row
  // and hand Enter to #general. Deferring the create row instead of the list
  // would make the two agree on precisely this wrong answer.
  const action = enter({
    query: "generalx",
    renderedQuery: "general",
    renderedChannels: [general],
  });

  assert.deepEqual(action, { kind: "create", name: "generalx" });
});

test("enter: a stale highlight index falls back to a live channel, never off-list", () => {
  // ArrowDown highlighted row 3 of the stale (3-channel) list; the live query
  // matches one channel, so index 3 does not exist in the authoritative list.
  const action = enter({
    query: "gene",
    renderedQuery: "",
    renderedChannels: ALL_ORDERED,
    selectedIndex: 3,
  });

  assert.equal(action.kind, "select");
  assert.equal(action.channel.name, "general");
});

test("enter: a stale list still creates when the live query matches nothing at all", () => {
  // Backwards staleness — the deferred list is narrower than the live query's.
  const action = enter({
    query: "qqq",
    renderedQuery: "gene",
    renderedChannels: [general],
  });

  assert.deepEqual(action, { kind: "create", name: "qqq" });
});

// ---------------------------------------------------------------------------
// Behaviour that must survive the fix (in-sync pairs).
// ---------------------------------------------------------------------------

test("enter: in-sync list with no highlight selects the top match", () => {
  const action = enter({
    query: "gene",
    renderedQuery: "gene",
    renderedChannels: [general],
  });

  assert.equal(action.kind, "select");
  assert.equal(action.channel.name, "general");
});

test("enter: in-sync list selects the highlighted channel past the create row", () => {
  // Create row occupies nav index 0, so index 2 is the second channel.
  const action = enter({
    query: "e",
    renderedQuery: "e",
    renderedChannels: [design, general],
    selectedIndex: 2,
  });

  assert.equal(action.kind, "select");
  assert.equal(action.channel.name, "general");
});

test("enter: a highlighted create row creates even when channels match", () => {
  const action = enter({
    query: "desig",
    renderedQuery: "desig",
    renderedChannels: [design],
    selectedIndex: 0,
  });

  assert.deepEqual(action, { kind: "create", name: "desig" });
});

test("enter: an exact existing name selects it instead of creating a duplicate", () => {
  const action = enter({
    query: "general",
    renderedQuery: "general",
    renderedChannels: [general],
  });

  assert.equal(action.kind, "select");
  assert.equal(action.channel.name, "general");
});

test("enter: index 0 selects the first channel when there is no create row", () => {
  // Without a create row the channels are not shifted down.
  const action = enter({
    canCreate: false,
    query: "e",
    renderedQuery: "e",
    renderedChannels: [design, general],
    selectedIndex: 0,
  });

  assert.equal(action.kind, "select");
  assert.equal(action.channel.name, "design");
});

test("enter: nothing to select and nothing to create does nothing", () => {
  const action = enter({
    canCreate: false,
    query: "zzz-nonexistent",
    renderedQuery: "zzz-nonexistent",
    renderedChannels: [],
  });

  assert.deepEqual(action, { kind: "none" });
});

test("enter: create carries the query verbatim, preserving case", () => {
  const action = enter({
    query: "Design-Review",
    renderedQuery: "design-review",
    renderedChannels: [],
  });

  assert.deepEqual(action, { kind: "create", name: "Design-Review" });
});

// ---------------------------------------------------------------------------
// The list pipeline the resolver recomputes with.
// ---------------------------------------------------------------------------

test("selectOrderedChannels: empty query lists every browsable channel alphabetically", () => {
  const list = selectOrderedChannels({
    activeTab: "all",
    channels: CHANNELS,
    query: "",
    sort: "alpha",
  });

  assert.deepEqual(
    list.map((channel) => channel.name),
    ["design", "general", "random"],
  );
});

test("selectOrderedChannels: filters by fuzzy match and orders by relevance", () => {
  const list = selectOrderedChannels({
    activeTab: "all",
    channels: CHANNELS,
    query: "gene",
    sort: "alpha",
  });

  assert.deepEqual(
    list.map((channel) => channel.name),
    ["general"],
  );
});

test("selectOrderedChannels: joined tab keeps only channels you are in", () => {
  const list = selectOrderedChannels({
    activeTab: "joined",
    channels: CHANNELS,
    query: "",
    sort: "alpha",
  });

  assert.deepEqual(
    list.map((channel) => channel.name),
    ["design"],
  );
});

test("selectOrderedChannels: archived tab keeps only joined archived channels", () => {
  const archivedJoined = makeChannel({
    name: "old-joined",
    archivedAt: "2024-01-01T00:00:00Z",
    isMember: true,
  });
  const archivedStranger = makeChannel({
    name: "old-stranger",
    archivedAt: "2024-01-01T00:00:00Z",
  });

  const list = selectOrderedChannels({
    activeTab: "archived",
    channels: [...CHANNELS, archivedJoined, archivedStranger],
    query: "",
    sort: "alpha",
  });

  assert.deepEqual(
    list.map((channel) => channel.name),
    ["old-joined"],
  );
});

test("selectOrderedChannels: dms and private non-member channels never appear", () => {
  const dm = makeChannel({ name: "dm-thread", channelType: "dm" });
  const privateStranger = makeChannel({
    name: "secret",
    visibility: "private",
  });

  const list = selectOrderedChannels({
    activeTab: "all",
    channels: [...CHANNELS, dm, privateStranger],
    query: "",
    sort: "alpha",
  });

  assert.deepEqual(
    list.map((channel) => channel.name),
    ["design", "general", "random"],
  );
});

test("selectOrderedChannels: members sort orders by member count", () => {
  const list = selectOrderedChannels({
    activeTab: "all",
    channels: CHANNELS,
    query: "",
    sort: "members",
  });

  assert.deepEqual(
    list.map((channel) => channel.name),
    ["general", "design", "random"],
  );
});

test("selectOrderedChannels: the type filter restricts to one channel kind", () => {
  const forum = makeChannel({ name: "ideas", channelType: "forum" });

  const list = selectOrderedChannels({
    activeTab: "all",
    channelTypeFilter: "forum",
    channels: [...CHANNELS, forum],
    query: "",
    sort: "alpha",
  });

  assert.deepEqual(
    list.map((channel) => channel.name),
    ["ideas"],
  );
});

// ---------------------------------------------------------------------------
// Create-row visibility.
// ---------------------------------------------------------------------------

test("shouldShowCreateRow: hidden for an exact existing name", () => {
  assert.equal(
    shouldShowCreateRow({
      canCreate: true,
      channels: CHANNELS,
      query: "general",
    }),
    false,
  );
});

test("shouldShowCreateRow: shown for a partial name and for no query at all", () => {
  assert.equal(
    shouldShowCreateRow({ canCreate: true, channels: CHANNELS, query: "gene" }),
    true,
  );
  assert.equal(
    shouldShowCreateRow({ canCreate: true, channels: CHANNELS, query: "" }),
    true,
  );
});

test("shouldShowCreateRow: never shown when the caller cannot create", () => {
  assert.equal(
    shouldShowCreateRow({
      canCreate: false,
      channels: CHANNELS,
      query: "gene",
    }),
    false,
  );
});

test("shouldShowCreateRow: an exact name of the other kind does not block creation", () => {
  const forum = makeChannel({ name: "general", channelType: "forum" });

  assert.equal(
    shouldShowCreateRow({
      canCreate: true,
      channelTypeFilter: "forum",
      channels: [...CHANNELS, forum],
      query: "general",
    }),
    false,
  );
  assert.equal(
    shouldShowCreateRow({
      canCreate: true,
      channelTypeFilter: "forum",
      channels: CHANNELS,
      query: "general",
    }),
    true,
  );
});
