import { Channel, invoke } from "@tauri-apps/api/core";

import { createAuthEvent } from "@/shared/api/tauri";
import {
  getTextPayload,
  type RelaySubscriptionFilter,
} from "@/shared/api/relayClientShared";
import {
  getEdgeRelayBinding,
  type EdgeRelayBinding,
} from "@/shared/api/relayEdgeRouting";
import {
  buildBindFrame,
  classifyEdgeFrame,
  edgeRefusal,
} from "@/shared/api/relayEdgeProtocol";
import { closeWebSocket } from "@/shared/api/relayWebSocketClose";
import {
  isWebSocketClose,
  isWebSocketError,
} from "@/shared/api/relayReconnectPolicy";
import type { RelayEvent } from "@/shared/api/types";

/**
 * Loopback is local. A sidecar that cannot answer the handshake this fast is
 * not going to beat the canonical relay, and every millisecond spent waiting
 * is a millisecond the user's message is not moving.
 */
const EDGE_HANDSHAKE_TIMEOUT_MS = 2_000;

/**
 * Second WebSocket client for the phase-1 `buzz-edge` sidecar
 * (SPEC-2026-08-05 §14, §15).
 *
 * This client carries persistent kind-9 message subscriptions ONLY. The
 * canonical `RelayClient` keeps every other responsibility — discovery,
 * membership, typing, presence, uploads, auth bootstrap, and every non-kind-9
 * kind — so nothing here can move traffic off the canonical relay by accident.
 *
 * Every failure path is the same: report unavailable and let the caller stay
 * canonical. The sidecar is an accelerator, never a dependency.
 */
export class RelayEdgeClient {
  private wsId: number | null = null;
  private binding: EdgeRelayBinding | null = null;
  private generation = 0;
  private connecting: Promise<boolean> | null = null;
  private messageChannel: Channel<unknown> | null = null;
  private handshake: {
    stage: "bind" | "auth";
    resolve: () => void;
    reject: (error: Error) => void;
    timeout: number;
  } | null = null;
  private pendingChallenge: string | null = null;
  private challengeWaiter: {
    resolve: (challenge: string) => void;
    timeout: number;
  } | null = null;
  private authEventId: string | null = null;
  /** True only once BIND and AUTH have both completed. A REQ before that is
   * answered with a NOTICE and dropped, so `wsId` alone is not "usable". */
  private ready = false;
  private subscriptions = new Map<
    string,
    {
      filter: RelaySubscriptionFilter;
      onEvent: (event: RelayEvent) => void;
      onLost: () => void;
    }
  >();

  /**
   * Re-read the binding and drop the session if it changed. Desktop switches
   * communities without a process restart, so a stale socket would otherwise
   * keep serving the previous community's cache — the exact cross-community
   * reuse §14 forbids.
   */
  async rebind(): Promise<EdgeRelayBinding | null> {
    let next: EdgeRelayBinding | null = null;
    try {
      next = await getEdgeRelayBinding();
    } catch {
      next = null;
    }
    if (!sameBinding(this.binding, next)) {
      // Teardown, not loss: the old community's subscriptions must not fail
      // over to canonical, because the whole session is being replaced.
      this.reset({ notifyLost: false });
      this.binding = next;
    }
    return this.binding;
  }

  /** Current binding without re-reading it. Null means canonical-only. */
  currentBinding(): EdgeRelayBinding | null {
    return this.binding;
  }

  /**
   * Subscribe one already-split, edge-eligible filter. Returns an unsubscribe
   * function, or null when the edge is unavailable — null is the caller's
   * signal to route the whole filter canonically.
   *
   * `onLost` fires when this subscription stops being served after it was
   * accepted: the sidecar answered CLOSED for it, or the socket dropped. The
   * caller MUST re-route to canonical on that signal. Without it the channel
   * simply goes quiet, which is the worst possible failure here — the UI still
   * looks connected because the canonical half keeps delivering reactions and
   * edits while messages stop.
   */
  async subscribe(
    filter: RelaySubscriptionFilter,
    onEvent: (event: RelayEvent) => void,
    onLost: () => void,
  ): Promise<(() => Promise<void>) | null> {
    if (!(await this.ensureConnected())) return null;

    const subId = `edge-${crypto.randomUUID()}`;
    this.subscriptions.set(subId, { filter, onEvent, onLost });
    try {
      await this.send(["REQ", subId, filter]);
    } catch {
      this.subscriptions.delete(subId);
      return null;
    }
    return async () => {
      if (!this.subscriptions.delete(subId)) return;
      await this.send(["CLOSE", subId]).catch(() => {});
    };
  }

  /**
   * Close the socket and forget every subscription.
   *
   * `notifyLost` distinguishes the two reasons this happens. A dropped socket
   * loses subscriptions the caller still wants, so each one is told to fail
   * over to canonical. An explicit teardown — community switch, rebind — is
   * discarding those subscriptions on purpose, and failing them over would
   * resurrect the previous community's traffic on the canonical relay.
   */
  reset({ notifyLost = false }: { notifyLost?: boolean } = {}) {
    this.generation += 1;
    this.ready = false;
    const lost = notifyLost
      ? [...this.subscriptions.values()].map((entry) => entry.onLost)
      : [];
    this.subscriptions.clear();
    this.rejectHandshake(new Error("Edge session was reset."));
    if (this.challengeWaiter) {
      window.clearTimeout(this.challengeWaiter.timeout);
      this.challengeWaiter = null;
    }
    this.pendingChallenge = null;
    this.authEventId = null;
    this.connecting = null;
    this.messageChannel = null;
    const wsId = this.wsId;
    this.wsId = null;
    // A stale binding is a live cross-community hazard: `currentBinding()` is
    // read synchronously when a subscription opens, and re-reading it is an
    // async round trip that the next subscribe may not have made yet.
    this.binding = null;
    if (wsId !== null) void closeWebSocket(wsId, "edge session reset");
    for (const notify of lost) notify();
  }

  private async ensureConnected(): Promise<boolean> {
    // Connect-in-flight is checked FIRST. A socket exists well before the
    // handshake completes, and a REQ sent in that window is dropped by the
    // sidecar with a NOTICE.
    if (this.connecting) return this.connecting;
    if (this.ready) return true;
    if (!this.binding && !(await this.rebind())) return false;
    const attempt = this.connect().finally(() => {
      // Identity-guarded: a reset may have already installed a newer attempt.
      if (this.connecting === attempt) this.connecting = null;
    });
    this.connecting = attempt;
    return attempt;
  }

  private async connect(): Promise<boolean> {
    const binding = this.binding;
    if (!binding) return false;
    const generation = ++this.generation;
    const channel = new Channel<unknown>((message) => {
      this.handleMessage(message, generation);
    });
    this.messageChannel = channel;

    try {
      const wsId = await invoke<number>("plugin:websocket|connect", {
        url: binding.relayUrl,
        onMessage: channel,
        config: {},
      });
      // The channel must still be the installed one: holding the reference is
      // what keeps the callback alive for the socket's lifetime, and a
      // replaced channel means this attempt was superseded.
      if (generation !== this.generation || this.messageChannel !== channel) {
        void closeWebSocket(wsId, "stale edge connection attempt");
        return false;
      }
      this.wsId = wsId;
      await this.performHandshake(binding, generation);
      if (generation !== this.generation) return false;
      this.ready = true;
      return true;
    } catch {
      // Never surface an edge failure. The caller falls back to canonical.
      // Nothing was accepted yet, so there is nothing to fail over.
      if (generation === this.generation) this.reset();
      return false;
    }
  }

  /**
   * Bring the session to "ready for REQ". The sidecar enforces a strict order
   * (`crates/buzz-edge/src/lib.rs`): BIND must complete before anything else,
   * NIP-42 must complete after that, and any REQ before both is answered with
   * a NOTICE rather than data. Getting this order wrong would look exactly
   * like a healthy connection that never delivers a message.
   */
  private async performHandshake(
    binding: EdgeRelayBinding,
    generation: number,
  ): Promise<void> {
    await this.awaitGate("bind", () => this.send(buildBindFrame(binding)));
    if (generation !== this.generation) {
      throw new Error("Edge connection attempt was superseded.");
    }
    // The challenge arrives immediately on connect, before BIND completes, so
    // it is buffered and answered here rather than on arrival.
    await this.awaitGate("auth", () => this.sendAuth(binding, generation));
    if (generation !== this.generation) {
      throw new Error("Edge connection attempt was superseded.");
    }
  }

  private async awaitGate(
    stage: "bind" | "auth",
    send: () => Promise<void>,
  ): Promise<void> {
    const settled = new Promise<void>((resolve, reject) => {
      const timeout = window.setTimeout(() => {
        this.handshake = null;
        reject(new Error(`Edge ${stage} timed out.`));
      }, EDGE_HANDSHAKE_TIMEOUT_MS);
      this.handshake = { stage, resolve, reject, timeout };
    });
    await send();
    await settled;
  }

  private async sendAuth(binding: EdgeRelayBinding, generation: number) {
    const challenge = this.pendingChallenge ?? (await this.awaitChallenge());
    this.pendingChallenge = null;
    const event = await createAuthEvent({
      challenge,
      relayUrl: binding.relayUrl,
    });
    if (generation !== this.generation) return;
    this.authEventId = event.id;
    await this.send(["AUTH", event]);
  }

  /** The challenge is sent on connect; if BIND won the race, wait for it. */
  private awaitChallenge(): Promise<string> {
    return new Promise((resolve, reject) => {
      const timeout = window.setTimeout(() => {
        this.challengeWaiter = null;
        reject(new Error("Edge auth challenge never arrived."));
      }, EDGE_HANDSHAKE_TIMEOUT_MS);
      this.challengeWaiter = { resolve, timeout };
    });
  }

  private handleMessage(message: unknown, generation: number) {
    if (generation !== this.generation) return;
    if (isWebSocketClose(message) || isWebSocketError(message)) {
      // The socket carried subscriptions the caller still wants. There is no
      // edge reconnect in phase 1, so canonical is the recovery path.
      this.reset({ notifyLost: true });
      return;
    }
    const payload = getTextPayload(message);
    if (!payload) return;

    let data: unknown;
    try {
      data = JSON.parse(payload);
    } catch {
      return;
    }
    if (!Array.isArray(data) || data.length === 0) return;

    const frame = classifyEdgeFrame(data, this.authEventId);
    switch (frame.type) {
      case "bound":
        if (frame.accepted) this.resolveHandshake("bind");
        else
          this.rejectHandshake(
            edgeRefusal("Edge rejected the community bind", frame.message),
          );
        return;
      case "challenge":
        this.acceptChallenge(frame.challenge);
        return;
      case "auth-result":
        if (frame.accepted) this.resolveHandshake("auth");
        else
          this.rejectHandshake(
            edgeRefusal("Edge rejected authentication", frame.message),
          );
        return;
      case "event":
        this.subscriptions.get(frame.subId)?.onEvent(frame.event);
        return;
      case "closed":
        this.loseSubscription(frame.subId);
        return;
      case "notice":
        // Not subscription-scoped, so the safe reading is that this connection
        // rejected something we sent. Anything already accepted on it is
        // suspect; drop the session and let every half fail over.
        this.reset({ notifyLost: true });
        return;
      default:
        return;
    }
  }

  /** Hand one rejected subscription back to the caller for canonical re-route. */
  private loseSubscription(subId: string) {
    const entry = this.subscriptions.get(subId);
    if (!entry) return;
    this.subscriptions.delete(subId);
    entry.onLost();
  }

  private acceptChallenge(challenge: string) {
    const waiter = this.challengeWaiter;
    if (waiter) {
      this.challengeWaiter = null;
      window.clearTimeout(waiter.timeout);
      waiter.resolve(challenge);
      return;
    }
    this.pendingChallenge = challenge;
  }

  private resolveHandshake(stage: "bind" | "auth") {
    const pending = this.handshake;
    // A frame for a stage we are not waiting on is out of order, not progress.
    if (pending?.stage !== stage) return;
    this.handshake = null;
    window.clearTimeout(pending.timeout);
    pending.resolve();
  }

  private rejectHandshake(error: Error) {
    const pending = this.handshake;
    if (!pending) return;
    this.handshake = null;
    window.clearTimeout(pending.timeout);
    pending.reject(error);
  }

  private async send(payload: unknown[]) {
    if (this.wsId === null) throw new Error("Edge socket is not connected.");
    await invoke("plugin:websocket|send", {
      id: this.wsId,
      message: { type: "Text", data: JSON.stringify(payload) },
    });
  }
}

export function sameBinding(
  left: EdgeRelayBinding | null,
  right: EdgeRelayBinding | null,
): boolean {
  if (!left || !right) return left === right;
  return (
    left.relayUrl === right.relayUrl &&
    left.httpUrl === right.httpUrl &&
    left.canonicalOrigin === right.canonicalOrigin &&
    left.communityId === right.communityId
  );
}
