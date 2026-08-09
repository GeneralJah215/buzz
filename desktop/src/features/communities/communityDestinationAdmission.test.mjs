import assert from "node:assert/strict";
import test from "node:test";

import {
  admitRememberedChannelRoute,
  observeRememberedChannel,
} from "./communityDestinationAdmission.ts";

function channel(overrides = {}) {
  return {
    id: "general",
    name: "general",
    channelType: "stream",
    visibility: "open",
    description: "",
    topic: null,
    purpose: null,
    memberCount: 1,
    memberPubkeys: [],
    lastMessageAt: null,
    archivedAt: null,
    participants: [],
    participantPubkeys: [],
    isMember: true,
    ttlSeconds: null,
    ttlDeadline: null,
    ...overrides,
  };
}

const REMEMBERED = { kind: "channel", channelId: "general" };

test("a positively observed, joined, unarchived channel is admitted", () => {
  assert.equal(
    observeRememberedChannel("general", [channel()]),
    "observed-available",
  );
  assert.equal(admitRememberedChannelRoute(REMEMBERED, [channel()]), "general");
});

// BUG-052 GUARDRAIL. This is the state that shipped the defect: the target
// community's channel list had never been read to completion, so the remembered
// channel was neither known-good nor known-bad. The old guard asked only "is
// the stored destination a channel?", which an unknown state passes, and the
// app entered a channel it had never confirmed existed. Absence of failure is
// not success (GRD-014/GRD-015). No wait, retry or longer timeout changes this
// assertion — the evidence is simply not there.
test("an UNOBSERVED channel list refuses the navigation (BUG-052)", () => {
  assert.equal(observeRememberedChannel("general", null), "unobserved");
  assert.equal(observeRememberedChannel("general", undefined), "unobserved");
  assert.equal(
    admitRememberedChannelRoute(REMEMBERED, null),
    null,
    "a never-read channel list must not admit a remembered channel",
  );
  assert.equal(
    admitRememberedChannelRoute(REMEMBERED, undefined),
    null,
    "an in-flight or failed read must not admit a remembered channel",
  );
});

test("an empty observation is unavailable, not unknown", () => {
  assert.equal(observeRememberedChannel("general", []), "observed-unavailable");
  assert.equal(admitRememberedChannelRoute(REMEMBERED, []), null);
});

test("a channel absent from the target community is refused", () => {
  const observed = [channel({ id: "random", name: "random" })];
  assert.equal(
    observeRememberedChannel("general", observed),
    "observed-unavailable",
  );
  assert.equal(admitRememberedChannelRoute(REMEMBERED, observed), null);
});

test("a channel the user is not a member of is refused", () => {
  const observed = [channel({ isMember: false })];
  assert.equal(
    observeRememberedChannel("general", observed),
    "observed-unavailable",
  );
  assert.equal(admitRememberedChannelRoute(REMEMBERED, observed), null);
});

test("an archived channel is refused", () => {
  const observed = [channel({ archivedAt: "2026-08-08T00:00:00Z" })];
  assert.equal(
    observeRememberedChannel("general", observed),
    "observed-unavailable",
  );
  assert.equal(admitRememberedChannelRoute(REMEMBERED, observed), null);
});

test("unobserved and unavailable stay distinguishable", () => {
  // Collapsing these two into one boolean is exactly the mistake that lets
  // "not failed" read as "succeeded". Keep them apart.
  assert.notEqual(
    observeRememberedChannel("general", null),
    observeRememberedChannel("general", []),
  );
});

test("a home destination, or none at all, never produces a channel route", () => {
  assert.equal(
    admitRememberedChannelRoute({ kind: "home" }, [channel()]),
    null,
  );
  assert.equal(admitRememberedChannelRoute(null, [channel()]), null);
  assert.equal(admitRememberedChannelRoute(undefined, [channel()]), null);
});

test("admission picks the remembered channel out of a mixed list", () => {
  const observed = [
    channel({ id: "random", name: "random" }),
    channel({ id: "general", name: "general" }),
    channel({ id: "archived", name: "archived", archivedAt: "2026-01-01" }),
  ];
  assert.equal(admitRememberedChannelRoute(REMEMBERED, observed), "general");
  assert.equal(
    admitRememberedChannelRoute(
      { kind: "channel", channelId: "archived" },
      observed,
    ),
    null,
  );
});
