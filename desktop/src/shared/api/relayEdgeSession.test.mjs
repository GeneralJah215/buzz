import assert from "node:assert/strict";
import test from "node:test";

import { sameBinding } from "./relayEdgeSession.ts";

const BINDING = {
  relayUrl: "ws://127.0.0.1:7777",
  httpUrl: "http://127.0.0.1:7777",
  canonicalOrigin: "wss://relay.example.com",
  communityId: "550e8400-e29b-41d4-a716-446655440000",
};

test("an identical binding is recognized so a healthy session is kept", () => {
  assert.equal(sameBinding(BINDING, { ...BINDING }), true);
});

test("null bindings compare only to null", () => {
  assert.equal(sameBinding(null, null), true);
  assert.equal(sameBinding(BINDING, null), false);
  assert.equal(sameBinding(null, BINDING), false);
});

test("any field change is a different binding, so the session resets", () => {
  // A community switch that reuses the same sidecar port is the case that
  // matters: only communityId moves, and missing it would serve the previous
  // community's cache — the cross-community reuse §14 forbids.
  for (const field of [
    "relayUrl",
    "httpUrl",
    "canonicalOrigin",
    "communityId",
  ]) {
    assert.equal(
      sameBinding(BINDING, { ...BINDING, [field]: "changed" }),
      false,
      `${field} must force a rebind`,
    );
  }
});
