import type { EdgeRelayBinding } from "@/shared/api/relayEdgeRouting";
import type { RelayEvent } from "@/shared/api/types";

/**
 * Wire shapes for the phase-1 `buzz-edge` sidecar.
 *
 * These mirror `crates/buzz-edge/src/protocol.rs` exactly and are kept in a
 * pure module so tests can pin them. A silent shape drift here does not throw
 * — it produces a socket that connects, never binds, and quietly falls back to
 * canonical forever, which is the hardest kind of failure to notice.
 */

/**
 * `["BUZZ-EDGE", "BIND", {canonical_origin, community_id}]`.
 *
 * The sidecar rejects the object outright unless it has exactly these two
 * snake_case keys, so nothing else may be added here.
 */
export function buildBindFrame(binding: EdgeRelayBinding): unknown[] {
  return [
    "BUZZ-EDGE",
    "BIND",
    {
      canonical_origin: binding.canonicalOrigin,
      community_id: binding.communityId,
    },
  ];
}

export type EdgeFrame =
  | { type: "bound"; accepted: boolean; message: string }
  | { type: "challenge"; challenge: string }
  | { type: "auth-result"; accepted: boolean; message: string }
  | { type: "event"; subId: string; event: RelayEvent }
  | { type: "closed"; subId: string; message: string }
  | { type: "notice"; message: string }
  | { type: "other" };

/**
 * Classify one decoded sidecar frame.
 *
 * `authEventId` scopes OK frames: the sidecar answers every EVENT with an OK
 * too, so only the OK naming our own AUTH event is an authentication result.
 */
export function classifyEdgeFrame(
  data: unknown,
  authEventId: string | null,
): EdgeFrame {
  if (!Array.isArray(data) || data.length === 0) return { type: "other" };
  const [type, ...rest] = data;

  if (type === "BUZZ-EDGE" && rest[0] === "BOUND") {
    return {
      type: "bound",
      // Only an explicit `true` is an accept. Anything else — including a
      // missing field — is a refusal, never an unbound session that proceeds.
      accepted: rest[1] === true,
      message: typeof rest[2] === "string" ? rest[2] : "",
    };
  }
  if (type === "AUTH" && typeof rest[0] === "string") {
    return { type: "challenge", challenge: rest[0] };
  }
  if (type === "OK" && authEventId !== null && rest[0] === authEventId) {
    return {
      type: "auth-result",
      accepted: rest[1] === true,
      message: typeof rest[2] === "string" ? rest[2] : "",
    };
  }
  if (type === "EVENT" && typeof rest[0] === "string" && rest[1]) {
    return { type: "event", subId: rest[0], event: rest[1] as RelayEvent };
  }
  // The sidecar refuses a REQ it cannot serve — an unselected channel, a
  // not-yet-ready routing table, a local query failure. This is a rejection,
  // not an absence of data, and the caller has to re-route rather than wait.
  if (type === "CLOSED" && typeof rest[0] === "string") {
    return {
      type: "closed",
      subId: rest[0],
      message: typeof rest[1] === "string" ? rest[1] : "",
    };
  }
  // A NOTICE is not subscription-scoped, so it cannot say which REQ was
  // dropped. It still means this connection rejected something we sent.
  if (type === "NOTICE" && typeof rest[0] === "string") {
    return { type: "notice", message: rest[0] };
  }
  return { type: "other" };
}

/** Prefix a refusal message with its stage, keeping the sidecar's own text. */
export function edgeRefusal(stage: string, message: string): Error {
  return new Error(message ? `${stage}: ${message}` : stage);
}
