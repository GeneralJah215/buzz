/**
 * Wire conformance against `crates/buzz-edge/src/protocol.rs`.
 *
 * The sidecar answers a wrong-shaped BIND with a NOTICE and simply never
 * binds, so a drift here yields a socket that connects and silently delivers
 * nothing. These assertions are deliberately literal: they encode the exact
 * frames the Rust parser accepts and emits.
 */
import assert from "node:assert/strict";
import test from "node:test";

import {
  buildBindFrame,
  classifyEdgeFrame,
  edgeRefusal,
} from "./relayEdgeProtocol.ts";

const BINDING = {
  relayUrl: "ws://127.0.0.1:7777",
  httpUrl: "http://127.0.0.1:7777",
  canonicalOrigin: "wss://relay.example.com",
  communityId: "550e8400-e29b-41d4-a716-446655440000",
};

test("BIND matches the frame the Rust parser accepts", () => {
  // protocol.rs: verb "BUZZ-EDGE", require_len 3, values[1] == "BIND".
  assert.deepEqual(buildBindFrame(BINDING), [
    "BUZZ-EDGE",
    "BIND",
    {
      canonical_origin: "wss://relay.example.com",
      community_id: "550e8400-e29b-41d4-a716-446655440000",
    },
  ]);
});

test("the BIND object carries exactly the two keys the sidecar allows", () => {
  // protocol.rs rejects the handshake outright when `binding.len() != 2`.
  const keys = Object.keys(buildBindFrame(BINDING)[2]);
  assert.deepEqual(keys.sort(), ["canonical_origin", "community_id"]);
});

test("an accepted BOUND frame is recognized", () => {
  assert.deepEqual(classifyEdgeFrame(["BUZZ-EDGE", "BOUND", true, ""], null), {
    type: "bound",
    accepted: true,
    message: "",
  });
});

test("a refused BOUND keeps the sidecar's reason", () => {
  assert.deepEqual(
    classifyEdgeFrame(
      [
        "BUZZ-EDGE",
        "BOUND",
        false,
        "canonical relay/community binding mismatch",
      ],
      null,
    ),
    {
      type: "bound",
      accepted: false,
      message: "canonical relay/community binding mismatch",
    },
  );
});

test("only an explicit true accepts the bind", () => {
  for (const value of [false, undefined, null, "true", 1]) {
    assert.equal(
      classifyEdgeFrame(["BUZZ-EDGE", "BOUND", value, ""], null).accepted,
      false,
      `${String(value)} must not be read as an accepted bind`,
    );
  }
});

test("an unknown BUZZ-EDGE operation is not mistaken for a bind result", () => {
  assert.deepEqual(classifyEdgeFrame(["BUZZ-EDGE", "SOMETHING", true], null), {
    type: "other",
  });
});

test("the AUTH challenge is recognized", () => {
  assert.deepEqual(classifyEdgeFrame(["AUTH", "challenge-1"], null), {
    type: "challenge",
    challenge: "challenge-1",
  });
});

test("only the OK naming our own AUTH event is an auth result", () => {
  const authId = "a".repeat(64);
  assert.deepEqual(classifyEdgeFrame(["OK", authId, true, ""], authId), {
    type: "auth-result",
    accepted: true,
    message: "",
  });
  // The sidecar OKs submitted events too; those must not resolve the gate.
  assert.deepEqual(
    classifyEdgeFrame(["OK", "b".repeat(64), true, ""], authId),
    { type: "other" },
  );
  // With no AUTH in flight, no OK may be read as an auth result.
  assert.deepEqual(classifyEdgeFrame(["OK", authId, true, ""], null), {
    type: "other",
  });
});

test("a failed auth keeps the sidecar's reason", () => {
  const authId = "a".repeat(64);
  assert.deepEqual(
    classifyEdgeFrame(
      ["OK", authId, false, "auth-required: verification failed"],
      authId,
    ),
    {
      type: "auth-result",
      accepted: false,
      message: "auth-required: verification failed",
    },
  );
});

test("EVENT frames carry the subscription id and payload", () => {
  const event = { id: "e".repeat(64), created_at: 5 };
  assert.deepEqual(classifyEdgeFrame(["EVENT", "edge-1", event], null), {
    type: "event",
    subId: "edge-1",
    event,
  });
});

test("CLOSED is a rejection, not silence", () => {
  // lib.rs answers CLOSED when a channel is not selected, the routing table is
  // not ready, the sub id is duplicated, or the local query fails. Treating it
  // as inert leaves the channel with no message half and no error.
  assert.deepEqual(
    classifyEdgeFrame(
      ["CLOSED", "edge-1", "restricted: channel is not selected"],
      null,
    ),
    {
      type: "closed",
      subId: "edge-1",
      message: "restricted: channel is not selected",
    },
  );
});

test("NOTICE is surfaced so a dropped REQ is not mistaken for quiet", () => {
  assert.deepEqual(
    classifyEdgeFrame(
      ["NOTICE", "binding-required: complete BUZZ-EDGE BIND first"],
      null,
    ),
    {
      type: "notice",
      message: "binding-required: complete BUZZ-EDGE BIND first",
    },
  );
});

test("malformed and empty frames are inert", () => {
  for (const frame of [
    ["EVENT", "edge-1"],
    ["AUTH"],
    ["CLOSED"],
    ["NOTICE"],
    [],
    null,
    "not an array",
    { type: "EVENT" },
  ]) {
    assert.deepEqual(
      classifyEdgeFrame(frame, null),
      { type: "other" },
      `${JSON.stringify(frame)} must be inert`,
    );
  }
});

test("a refusal without a message still names its stage", () => {
  assert.equal(
    edgeRefusal("Edge rejected the bind", "").message,
    "Edge rejected the bind",
  );
  assert.equal(
    edgeRefusal("Edge rejected the bind", "mismatch").message,
    "Edge rejected the bind: mismatch",
  );
});
