import assert from "node:assert/strict";
import test from "node:test";

import { threadDirectoryQueryKey } from "./threadDirectory.ts";

const PUBKEY = "c".repeat(64);
const CHANNEL = "36411e44-0e2d-4cfe-bd6e-567eb169db9f";

test("thread-directory keys isolate community, relay, identity, channel, and state", () => {
  const base = threadDirectoryQueryKey(
    "community-a",
    "wss://relay-a.example",
    PUBKEY,
    CHANNEL,
    "active",
  );
  for (const changed of [
    threadDirectoryQueryKey(
      "community-b",
      "wss://relay-a.example",
      PUBKEY,
      CHANNEL,
      "active",
    ),
    threadDirectoryQueryKey(
      "community-a",
      "wss://relay-b.example",
      PUBKEY,
      CHANNEL,
      "active",
    ),
    threadDirectoryQueryKey(
      "community-a",
      "wss://relay-a.example",
      "d".repeat(64),
      CHANNEL,
      "active",
    ),
    threadDirectoryQueryKey(
      "community-a",
      "wss://relay-a.example",
      PUBKEY,
      "other",
      "active",
    ),
    threadDirectoryQueryKey(
      "community-a",
      "wss://relay-a.example",
      PUBKEY,
      CHANNEL,
      "archived",
    ),
  ]) {
    assert.notDeepEqual(changed, base);
  }
});
