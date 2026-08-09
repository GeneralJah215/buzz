import assert from "node:assert/strict";
import test from "node:test";

import {
  CONTROL_RESULT_GAP_STATUS,
  OBSERVER_GAP_KIND,
  appendGapMarker,
  buildTranscriptStateWithGaps,
  detectRelaySeqGap,
  gapTranscriptItem,
  parseObserverGapFrame,
  processTranscriptEventWithGaps,
  syntheticGapEvent,
} from "./observerGapDetection.ts";
import { createEmptyTranscriptState } from "./ui/agentSessionTranscript.ts";

const CHANNEL = "11111111-1111-1111-1111-111111111111";

function baseEvent(overrides = {}) {
  return {
    seq: 1,
    timestamp: "2026-08-09T00:00:00Z",
    kind: "acp_write",
    agentIndex: 0,
    channelId: CHANNEL,
    sessionId: "sess-1",
    turnId: "turn-1",
    payload: {},
    ...overrides,
  };
}

function chunk(seq, text, messageId = "m1") {
  return baseEvent({
    seq,
    kind: "acp_read",
    timestamp: `2026-08-09T00:00:0${seq}Z`,
    payload: {
      method: "session/update",
      params: {
        sessionId: "sess-1",
        update: {
          sessionUpdate: "agent_message_chunk",
          messageId,
          content: { type: "text", text },
        },
      },
    },
  });
}

function gapFrame(seq, payload) {
  return baseEvent({ seq, kind: OBSERVER_GAP_KIND, payload });
}

test("parseObserverGapFrame reads a well-formed harness receipt", () => {
  const gap = parseObserverGapFrame(
    gapFrame(50, {
      fromSeq: 10,
      toSeq: 40,
      unrecoverable: 0,
      lostContent: 29,
      controlComplete: true,
    }),
  );
  assert.deepEqual(gap, {
    fromSeq: 10,
    toSeq: 40,
    unrecoverable: 0,
    lostContent: 29,
    controlComplete: true,
    source: "harness",
  });
});

test("an unparseable receipt is treated as incomplete, never as success", () => {
  // "I cannot read the receipt" and "the recovery succeeded" must not be the
  // same answer. Only a literal `true` counts.
  for (const payload of [
    {},
    { controlComplete: "true" },
    { controlComplete: 1 },
    { controlComplete: null },
    null,
  ]) {
    const gap = parseObserverGapFrame(gapFrame(5, payload));
    assert.equal(
      gap.controlComplete,
      false,
      `payload ${JSON.stringify(payload)} must not read as complete`,
    );
  }
});

test("parseObserverGapFrame ignores frames that are not gap announcements", () => {
  assert.equal(parseObserverGapFrame(baseEvent()), null);
});

test("a seq jump with no announcement is a relay gap and is never recoverable", () => {
  // Frames lost after the harness published them. No observer_gap frame is
  // coming and there is no replay ring on this side of the relay.
  const gap = detectRelaySeqGap(10, baseEvent({ seq: 25 }));
  assert.deepEqual(gap, {
    fromSeq: 10,
    toSeq: 25,
    unrecoverable: 14,
    lostContent: 0,
    controlComplete: false,
    source: "relay",
  });
});

test("ordinary delivery is not mistaken for a gap", () => {
  assert.equal(detectRelaySeqGap(undefined, baseEvent({ seq: 900 })), null);
  assert.equal(detectRelaySeqGap(10, baseEvent({ seq: 11 })), null);
  assert.equal(detectRelaySeqGap(10, baseEvent({ seq: 10 })), null);
  assert.equal(detectRelaySeqGap(10, baseEvent({ seq: 4 })), null);
});

test("a synthetic relay gap event sorts inside the hole it describes", () => {
  const gap = detectRelaySeqGap(10, baseEvent({ seq: 25 }));
  const event = syntheticGapEvent(gap, baseEvent({ seq: 25 }));
  assert.equal(event.kind, OBSERVER_GAP_KIND);
  assert.ok(event.seq > 10 && event.seq < 25);
  assert.equal(event.payload.controlComplete, false);
  // Round-trips through the same parser as a harness frame, so downstream
  // cannot tell the two apart.
  assert.equal(parseObserverGapFrame(event).source, "relay");
});

test("an unrecoverable gap renders as an error, a recovered one does not", () => {
  const lost = gapTranscriptItem(
    { ...parseObserverGapFrame(gapFrame(5, {})), controlComplete: false },
    baseEvent({ seq: 5 }),
  );
  const recovered = gapTranscriptItem(
    { ...parseObserverGapFrame(gapFrame(5, {})), controlComplete: true },
    baseEvent({ seq: 5 }),
  );
  assert.equal(lost.renderClass, "error");
  assert.equal(recovered.renderClass, "status");
  assert.equal(lost.type, "lifecycle");
  assert.match(lost.text, /could not be recovered/);
});

test("a gap breaks the streaming accumulator instead of splicing across it", () => {
  // The consumer-2 defect: upsertMessage does `text: existing.text + text`, so
  // without sealing, "before" and "after" concatenate into one message that
  // reads as continuous prose with a span silently deleted from the middle.
  const events = [
    chunk(1, "before the gap. "),
    gapFrame(2, { fromSeq: 1, toSeq: 90, unrecoverable: 3, lostContent: 85 }),
    chunk(3, "after the gap."),
  ];
  const items = buildTranscriptStateWithGaps(events).items;

  const messages = items.filter((item) => item.type === "message");
  assert.equal(
    messages.length,
    2,
    "the text either side of the hole must be two messages, not one",
  );
  assert.equal(messages[0].text, "before the gap. ");
  assert.equal(messages[1].text, "after the gap.");
  assert.ok(
    items.some((item) => item.id.startsWith("observer-gap:")),
    "the hole must be marked in the transcript",
  );
});

test("a gap frame alone produces nothing without the gap wrapper", () => {
  // Pins the reason this module exists: agentSessionTranscript has no dispatch
  // arm for observer_gap and no terminal else, so the frame is silent there.
  const withGaps = processTranscriptEventWithGaps(
    createEmptyTranscriptState(),
    gapFrame(2, { fromSeq: 1, toSeq: 9 }),
  );
  assert.equal(withGaps.items.length, 1);
  assert.equal(withGaps.items[0].type, "lifecycle");
});

test("a full rebuild keeps the gap markers", () => {
  // The store rebuilds from the journal on out-of-order arrival and on trim.
  // Markers derived from the journal survive that; stored ones would not.
  const events = [
    chunk(1, "a"),
    gapFrame(2, { fromSeq: 1, toSeq: 9, unrecoverable: 2 }),
    chunk(9, "b"),
  ];
  const rebuilt = buildTranscriptStateWithGaps(events);
  assert.equal(
    rebuilt.items.filter((item) => item.id.startsWith("observer-gap:")).length,
    1,
  );
});

test("appendGapMarker seals every open message key", () => {
  let state = processTranscriptEventWithGaps(
    createEmptyTranscriptState(),
    chunk(1, "open"),
  );
  const openKeys = [...state.activeMessageKey.values()];
  assert.ok(openKeys.length > 0, "precondition: a message is open");
  state = appendGapMarker(
    state,
    parseObserverGapFrame(gapFrame(2, {})),
    baseEvent({ seq: 2 }),
  );
  for (const key of openKeys) {
    assert.ok(state.sealedKeys.has(key), `${key} must be sealed`);
  }
});

test("the control_result gap status is distinct from every real outcome", () => {
  // A waiting caller must be able to tell "the reply was destroyed" from any
  // status the harness can actually report.
  for (const real of [
    "sent",
    "turn_ending",
    "switched",
    "unsupported_model",
    "no_active_turn",
  ]) {
    assert.notEqual(CONTROL_RESULT_GAP_STATUS, real);
  }
});
