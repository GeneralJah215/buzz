import assert from "node:assert/strict";
import { beforeEach, describe, it } from "node:test";

import {
  _testRegisterKnownAgents,
  getAgentObserverGaps,
  getAgentObserverSnapshot,
  getAgentTranscript,
  isAgentObserverStateStale,
  resetAgentObserverStore,
  subscribeControlResults,
  trackObserverContinuity,
} from "@/features/agents/observerRelayStore.ts";
import {
  CONTROL_RESULT_GAP_STATUS,
  OBSERVER_GAP_KIND,
} from "@/features/agents/observerGapDetection.ts";

const AGENT_PUBKEY = "a".repeat(64);
const SUB_ID = "gap-test-sub";

function frame(seq, overrides = {}) {
  return {
    seq,
    timestamp: `2026-08-09T00:00:${String(seq % 60).padStart(2, "0")}Z`,
    kind: "acp_read",
    agentIndex: 0,
    channelId: "11111111-1111-1111-1111-111111111111",
    sessionId: "sess-1",
    turnId: "turn-1",
    payload: {},
    ...overrides,
  };
}

describe("observer seq-gap tracking", () => {
  beforeEach(() => {
    resetAgentObserverStore();
    _testRegisterKnownAgents(SUB_ID, [AGENT_PUBKEY]);
  });

  it("records nothing while frames arrive in order", () => {
    for (const seq of [1, 2, 3]) {
      trackObserverContinuity(AGENT_PUBKEY, frame(seq));
    }
    assert.deepEqual([...getAgentObserverGaps(AGENT_PUBKEY)], []);
    assert.equal(isAgentObserverStateStale(AGENT_PUBKEY), false);
  });

  it("flags a relay-level seq jump as unrecoverable stale state", () => {
    // Frames lost after the harness published them. No observer_gap frame is
    // coming, and there is no replay ring on this side of the relay — so the
    // agent's lifecycle badge, model switch outcome and session config may all
    // be wrong, and the app must be able to find that out.
    trackObserverContinuity(AGENT_PUBKEY, frame(1));
    trackObserverContinuity(AGENT_PUBKEY, frame(3319));

    const gaps = getAgentObserverGaps(AGENT_PUBKEY);
    assert.equal(gaps.length, 1);
    assert.equal(gaps[0].source, "relay");
    assert.equal(gaps[0].unrecoverable, 3317);
    assert.equal(gaps[0].controlComplete, false);
    assert.equal(isAgentObserverStateStale(AGENT_PUBKEY), true);
  });

  it("puts the relay gap in the journal so a rebuild cannot lose it", () => {
    trackObserverContinuity(AGENT_PUBKEY, frame(1));
    trackObserverContinuity(AGENT_PUBKEY, frame(90));

    const events = getAgentObserverSnapshot(AGENT_PUBKEY, true).events;
    const marker = events.find((event) => event.kind === OBSERVER_GAP_KIND);
    assert.ok(marker, "the hole must be a real event, not a transient flag");
    assert.ok(marker.seq > 1 && marker.seq < 90);

    const items = getAgentTranscript(AGENT_PUBKEY, true);
    const rendered = items.find((item) => item.id.startsWith("observer-gap:"));
    assert.ok(rendered, "the hole must be visible in the transcript");
    assert.equal(rendered.renderClass, "error");
  });

  it("unblocks a caller waiting on a control_result destroyed in the hole", () => {
    // Without this the ModelPicker's switch_model request hangs forever: the
    // reply frame is gone and the harness never re-sends it.
    const received = [];
    subscribeControlResults(AGENT_PUBKEY, (result) => received.push(result));

    trackObserverContinuity(AGENT_PUBKEY, frame(1));
    trackObserverContinuity(AGENT_PUBKEY, frame(500));

    assert.equal(received.length, 1);
    assert.equal(received[0].status, CONTROL_RESULT_GAP_STATUS);
  });

  it("does not fabricate an RPC failure when the harness refilled the hole", () => {
    // A recovered gap still lost telemetry, but no waiting caller was
    // stranded — inventing a failure would be its own wrong answer.
    const received = [];
    subscribeControlResults(AGENT_PUBKEY, (result) => received.push(result));

    trackObserverContinuity(AGENT_PUBKEY, frame(1));
    trackObserverContinuity(
      AGENT_PUBKEY,
      frame(2, {
        kind: OBSERVER_GAP_KIND,
        payload: {
          fromSeq: 1,
          toSeq: 2,
          unrecoverable: 0,
          lostContent: 400,
          controlComplete: true,
        },
      }),
    );

    assert.deepEqual(received, []);
    assert.equal(getAgentObserverGaps(AGENT_PUBKEY).length, 1);
    assert.equal(isAgentObserverStateStale(AGENT_PUBKEY), false);
  });

  it("treats an unreadable harness receipt as stale, not as success", () => {
    trackObserverContinuity(AGENT_PUBKEY, frame(1));
    trackObserverContinuity(
      AGENT_PUBKEY,
      frame(2, { kind: OBSERVER_GAP_KIND, payload: { fromSeq: 1 } }),
    );
    assert.equal(isAgentObserverStateStale(AGENT_PUBKEY), true);
  });

  it("keeps gap state per agent and clears it on reset", () => {
    trackObserverContinuity(AGENT_PUBKEY, frame(1));
    trackObserverContinuity(AGENT_PUBKEY, frame(50));
    assert.equal(isAgentObserverStateStale(AGENT_PUBKEY), true);
    assert.equal(isAgentObserverStateStale("b".repeat(64)), false);

    resetAgentObserverStore();
    assert.equal(isAgentObserverStateStale(AGENT_PUBKEY), false);
  });

  it("does not read a re-delivered or out-of-order frame as a gap", () => {
    trackObserverContinuity(AGENT_PUBKEY, frame(10));
    trackObserverContinuity(AGENT_PUBKEY, frame(11));
    trackObserverContinuity(AGENT_PUBKEY, frame(11));
    trackObserverContinuity(AGENT_PUBKEY, frame(4));
    trackObserverContinuity(AGENT_PUBKEY, frame(12));
    assert.deepEqual([...getAgentObserverGaps(AGENT_PUBKEY)], []);
  });
});
