import assert from "node:assert/strict";
import { beforeEach, describe, it } from "node:test";

import {
  MAX_OBSERVER_EVENTS,
  _testGetTranscriptRebuildCount,
  getAgentTranscript,
  resetAgentObserverStore,
  subscribeAgentObserverStore,
  syncAgentObserverEvents,
} from "@/features/agents/observerRelayStore.ts";
import { buildTranscriptStateWithGaps } from "@/features/agents/observerGapDetection.ts";
import {
  _testGetTurnSyncAgentVisits,
  _testGetTurnSyncScanCount,
  getActiveTurnsForAgent,
  resetActiveAgentTurnsStore,
  syncActiveAgentTurnsFromObserver,
} from "@/features/agents/activeAgentTurnsStore.ts";

const AGENT_A = "a".repeat(64);
const AGENT_B = "b".repeat(64);
const CHANNEL = "11111111-1111-1111-1111-111111111111";
const EPOCH = Date.UTC(2026, 0, 1, 0, 0, 0);

/**
 * One `turn_started` frame. Each distinct `turnId` produces exactly one
 * lifecycle item (`turn:<channel>:<turnId>`), so item identity is a direct,
 * readable function of the events that are still in the journal.
 */
function turnFrame(seq, overrides = {}) {
  return {
    seq,
    timestamp: new Date(EPOCH + seq * 1000).toISOString(),
    kind: "turn_started",
    agentIndex: 0,
    channelId: CHANNEL,
    sessionId: "sess-1",
    turnId: `t${seq}`,
    payload: { channel_id: CHANNEL },
    ...overrides,
  };
}

function frames(fromSeq, toSeq) {
  const out = [];
  for (let seq = fromSeq; seq <= toSeq; seq += 1) out.push(turnFrame(seq));
  return out;
}

function itemIds(agentPubkey) {
  return getAgentTranscript(agentPubkey).map((item) => item.id);
}

describe("capped observer journal keeps the incremental transcript path", () => {
  beforeEach(() => {
    resetAgentObserverStore();
    resetActiveAgentTurnsStore();
  });

  it("never rebuilds the transcript while in-order frames overflow the cap", () => {
    const overflow = 500;
    syncAgentObserverEvents(AGENT_A, frames(1, MAX_OBSERVER_EVENTS + overflow));

    // Before the fix every append past the cap took the full-rebuild branch,
    // so this counter read `overflow`. It must now be exactly zero: not
    // "small", not "fast" — zero.
    assert.equal(_testGetTranscriptRebuildCount(), 0);
  });

  it("evicts exactly the transcript items the evicted events produced", () => {
    const overflow = 250;
    const total = MAX_OBSERVER_EVENTS + overflow;
    syncAgentObserverEvents(AGENT_A, frames(1, total));

    const ids = itemIds(AGENT_A);
    assert.equal(ids.length, MAX_OBSERVER_EVENTS);
    assert.equal(ids[0], `turn:${CHANNEL}:t${overflow + 1}`);
    assert.equal(ids[ids.length - 1], `turn:${CHANNEL}:t${total}`);
    assert.ok(!ids.includes(`turn:${CHANNEL}:t${overflow}`));
  });

  it("produces the same item set a full rebuild over the window would", () => {
    const overflow = 120;
    const total = MAX_OBSERVER_EVENTS + overflow;
    syncAgentObserverEvents(AGENT_A, frames(1, total));

    const rebuilt = buildTranscriptStateWithGaps(frames(overflow + 1, total));
    assert.deepEqual(
      itemIds(AGENT_A),
      rebuilt.items.map((item) => item.id),
    );
  });

  it("keeps an item whose creating event was evicted but was touched again", () => {
    const total = MAX_OBSERVER_EVENTS;
    syncAgentObserverEvents(AGENT_A, frames(1, total));

    // Re-touch turn t1 (created by the very first, about-to-be-evicted frame)
    // in the same frame that pushes seq 1 out of the journal.
    syncAgentObserverEvents(AGENT_A, [
      turnFrame(total + 1, { turnId: "t1", payload: { revisited: true } }),
    ]);

    const ids = itemIds(AGENT_A);
    assert.ok(
      ids.includes(`turn:${CHANNEL}:t1`),
      "an item re-touched by a surviving event must not be evicted with its creating event",
    );
    assert.equal(_testGetTranscriptRebuildCount(), 0);
  });

  it("still rebuilds on genuinely out-of-order arrival", () => {
    syncAgentObserverEvents(AGENT_A, frames(1, 5));
    assert.equal(_testGetTranscriptRebuildCount(), 0);

    // A frame that sorts before the newest one already in the journal.
    syncAgentObserverEvents(AGENT_A, [
      turnFrame(3, {
        turnId: "late",
        timestamp: new Date(EPOCH + 2500).toISOString(),
      }),
    ]);
    assert.equal(_testGetTranscriptRebuildCount(), 1);
    assert.ok(itemIds(AGENT_A).includes(`turn:${CHANNEL}:late`));
  });
});

describe("observer store notifies with the agent that changed", () => {
  beforeEach(() => {
    resetAgentObserverStore();
    resetActiveAgentTurnsStore();
  });

  it("passes the changed agent key to subscribers", () => {
    const seen = [];
    const unsubscribe = subscribeAgentObserverStore((key) => seen.push(key));
    syncAgentObserverEvents(AGENT_A, [turnFrame(1)]);
    unsubscribe();

    assert.deepEqual(seen, [AGENT_A]);
  });

  it("reports null for store-wide changes", () => {
    const seen = [];
    const unsubscribe = subscribeAgentObserverStore((key) => seen.push(key));
    resetAgentObserverStore();
    unsubscribe();

    assert.deepEqual(seen, [null]);
  });
});

describe("active-turn sync does bounded work per frame", () => {
  const agents = [
    { pubkey: AGENT_A, status: "running" },
    { pubkey: AGENT_B, status: "running" },
  ];

  beforeEach(() => {
    resetAgentObserverStore();
    resetActiveAgentTurnsStore();
  });

  it("walks only the events newer than the watermark", () => {
    syncAgentObserverEvents(AGENT_A, frames(1, 400));
    syncActiveAgentTurnsFromObserver(agents);
    const afterSeed = _testGetTurnSyncScanCount();
    assert.equal(afterSeed, 400);

    syncAgentObserverEvents(AGENT_A, [turnFrame(401)]);
    syncActiveAgentTurnsFromObserver(agents);

    // One new frame must cost one event of work, not a re-walk of the whole
    // 400-event journal.
    assert.equal(_testGetTurnSyncScanCount() - afterSeed, 1);
  });

  it("reads only the changed agent's snapshot", () => {
    syncAgentObserverEvents(AGENT_A, frames(1, 50));
    syncAgentObserverEvents(AGENT_B, frames(1, 50));
    syncActiveAgentTurnsFromObserver(agents);
    const afterSeed = _testGetTurnSyncAgentVisits();
    assert.equal(afterSeed, 2);

    syncAgentObserverEvents(AGENT_B, [turnFrame(51)]);
    syncActiveAgentTurnsFromObserver(agents, AGENT_B);

    // An up-to-date agent costs zero *scanned events*, so the event counter
    // cannot see the fan-out. Visiting one agent instead of every agent is the
    // whole point of threading the changed key through the notification.
    assert.equal(_testGetTurnSyncAgentVisits() - afterSeed, 1);
    assert.equal(_testGetTurnSyncScanCount(), 101);
    // And the scoped sync still did the real work for that agent.
    assert.ok(getActiveTurnsForAgent(AGENT_B).length > 0);
  });

  it("falls back to syncing every agent when no key is supplied", () => {
    syncAgentObserverEvents(AGENT_A, frames(1, 10));
    syncAgentObserverEvents(AGENT_B, frames(1, 10));
    syncActiveAgentTurnsFromObserver(agents, null);

    assert.equal(_testGetTurnSyncAgentVisits(), 2);
    assert.equal(_testGetTurnSyncScanCount(), 20);
  });
});
