/**
 * BUG-067 — ManagedAgentSessionPanel's third full transcript rebuild.
 *
 * The panel derives its transcript from live+archive on every frame, because
 * `invalidateSnapshot` hands back a new events array whenever the agent's
 * journal changes — including for a frame that belongs to a channel this panel
 * is not showing. `scopeByChannel` then allocated a fresh (identical) array,
 * which broke the `useMemo` on the merge and on `buildPanelTranscriptItems`.
 *
 * The fix is value-stabilisation, not filtering: when the panel's own channel
 * does gain a frame, it MUST still rebuild, and the assertions below check
 * both halves. Counts, never timings.
 */

import assert from "node:assert/strict";
import { beforeEach, describe, it } from "node:test";

import { installDOMShim } from "@/shared/testing/reactDomShim.mjs";

installDOMShim();

globalThis.__TAURI_INTERNALS__ = {
  invoke: () => Promise.resolve(null),
  transformCallback: () => Math.random(),
};

import React from "react";
import { act } from "react";
import { createRoot } from "react-dom/client";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

import {
  _testGetPanelTranscriptBuildCount,
  _testResetPanelTranscriptBuildCount,
  useSessionPanelTranscript,
} from "@/features/agents/ui/useSessionPanelTranscript.ts";
import {
  useArchivedChannelEvents,
  useObserverEvents,
} from "@/features/agents/ui/useObserverEvents.ts";
import {
  resetAgentObserverStore,
  syncAgentObserverEvents,
} from "@/features/agents/observerRelayStore.ts";
import { resetActiveAgentTurnsStore } from "@/features/agents/activeAgentTurnsStore.ts";

const AGENT_A = "a".repeat(64);
const SHOWN_CHANNEL = "11111111-1111-1111-1111-111111111111";
const OTHER_CHANNEL = "22222222-2222-2222-2222-222222222222";
const EPOCH = Date.UTC(2026, 0, 1, 0, 0, 0);

function turnFrame(seq, channelId) {
  return {
    seq,
    timestamp: new Date(EPOCH + seq * 1000).toISOString(),
    kind: "turn_started",
    agentIndex: 0,
    channelId,
    sessionId: "sess-1",
    turnId: `t${seq}`,
    payload: { channel_id: channelId },
  };
}

/**
 * Mounts exactly the chain ManagedAgentSessionPanel runs: the same two store
 * hooks feeding the same derivation hook, with the same arguments. The panel's
 * own `.tsx` cannot be imported here — it reads `import.meta.env`, which node
 * does not provide — which is why the derivation lives in its own module.
 */
function mountPanel(channelId) {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false, refetchInterval: false } },
  });
  const seen = { transcript: null };

  function PanelBody() {
    const { events } = useObserverEvents(true, AGENT_A);
    const archivedChannelEvents = useArchivedChannelEvents(AGENT_A, channelId);
    const { derivedTranscript } = useSessionPanelTranscript(
      events,
      channelId,
      archivedChannelEvents,
    );
    seen.transcript = derivedTranscript;
    return React.createElement("div", {
      "data-items": derivedTranscript.length,
    });
  }

  const container = document.createElement("div");
  const root = createRoot(container);
  act(() => {
    root.render(
      React.createElement(
        QueryClientProvider,
        { client: queryClient },
        React.createElement(PanelBody, null),
      ),
    );
  });
  return {
    seen,
    renderedItemCount: () =>
      Number(container.children[0].getAttribute("data-items")),
    unmount: () => act(() => root.unmount()),
  };
}

function stream(seq, channelId) {
  act(() => {
    syncAgentObserverEvents(AGENT_A, [turnFrame(seq, channelId)]);
  });
}

beforeEach(() => {
  resetAgentObserverStore();
  resetActiveAgentTurnsStore();
  _testResetPanelTranscriptBuildCount();
});

describe("ManagedAgentSessionPanel transcript rebuilds", () => {
  it("does not rebuild for a frame in a channel it is not showing", () => {
    const h = mountPanel(SHOWN_CHANNEL);
    _testResetPanelTranscriptBuildCount();

    for (let seq = 1; seq <= 20; seq += 1) {
      stream(seq, OTHER_CHANNEL);
    }

    assert.equal(
      _testGetPanelTranscriptBuildCount(),
      0,
      "an unrelated channel's frames rebuilt this panel's whole transcript",
    );
    h.unmount();
  });

  it("STILL rebuilds when its own channel gains a frame", () => {
    // The silence guard: a panel that stopped rebuilding would also pass the
    // test above, and would show a transcript frozen at mount.
    const h = mountPanel(SHOWN_CHANNEL);
    _testResetPanelTranscriptBuildCount();

    stream(1, SHOWN_CHANNEL);
    assert.equal(_testGetPanelTranscriptBuildCount(), 1);
    assert.equal(h.renderedItemCount(), 1);

    stream(2, SHOWN_CHANNEL);
    assert.equal(_testGetPanelTranscriptBuildCount(), 2);
    assert.equal(h.renderedItemCount(), 2);

    h.unmount();
  });

  it("rebuilds exactly once per frame, mixing both channels", () => {
    const h = mountPanel(SHOWN_CHANNEL);
    _testResetPanelTranscriptBuildCount();

    stream(1, OTHER_CHANNEL);
    stream(2, SHOWN_CHANNEL);
    stream(3, OTHER_CHANNEL);
    stream(4, SHOWN_CHANNEL);
    stream(5, OTHER_CHANNEL);

    assert.equal(_testGetPanelTranscriptBuildCount(), 2);
    assert.equal(
      h.renderedItemCount(),
      2,
      "the panel must render both of its own channel's turns",
    );
    h.unmount();
  });
});
