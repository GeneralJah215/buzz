import assert from "node:assert/strict";
import test from "node:test";

// Minimal localStorage shim — these run under node:test, not a browser.
if (typeof globalThis.window === "undefined") {
  const store = new Map();
  globalThis.window = {
    localStorage: {
      getItem: (k) => (store.has(k) ? store.get(k) : null),
      setItem: (k, v) => store.set(k, String(v)),
      removeItem: (k) => store.delete(k),
      clear: () => store.clear(),
    },
  };
}

const {
  loadLocalThreadNames,
  localThreadNameIsValid,
  localThreadNamesKey,
  MAX_LOCAL_THREAD_NAME_LENGTH,
  resolveLocalThreadName,
  setLocalThreadName,
} = await import("./localThreadNames.ts");

const PUBKEY = "a".repeat(64);
const OTHER = "b".repeat(64);
const RELAY = "wss://relay.example.com";
const ROOT = "c".repeat(64);

test("a name round-trips for the identity that set it", () => {
  window.localStorage.clear();
  setLocalThreadName(PUBKEY, RELAY, ROOT, "Dungeon planning");
  assert.equal(loadLocalThreadNames(PUBKEY, RELAY)[ROOT], "Dungeon planning");
});

test("names do not leak across identities or relays", () => {
  // Local caches here are scoped by identity and relay. A label set while
  // signed in as one identity must not appear in another's sidebar.
  window.localStorage.clear();
  setLocalThreadName(PUBKEY, RELAY, ROOT, "Mine");
  assert.equal(loadLocalThreadNames(OTHER, RELAY)[ROOT], undefined);
  assert.equal(
    loadLocalThreadNames(PUBKEY, "wss://other.example.com")[ROOT],
    undefined,
  );
  assert.notEqual(
    localThreadNamesKey(PUBKEY, RELAY),
    localThreadNamesKey(OTHER, RELAY),
  );
});

test("an empty name clears the entry instead of storing a blank", () => {
  // Otherwise "rename to nothing" leaves a row with no label at all.
  window.localStorage.clear();
  setLocalThreadName(PUBKEY, RELAY, ROOT, "Temporary");
  const cleared = setLocalThreadName(PUBKEY, RELAY, ROOT, "   ");
  assert.equal(cleared[ROOT], undefined);
  assert.equal(loadLocalThreadNames(PUBKEY, RELAY)[ROOT], undefined);
});

test("a shared name always beats a local one", () => {
  // The shared name is what every other member sees. A stale local override
  // masking it would show this machine something nobody else sees.
  assert.equal(resolveLocalThreadName("Shared", "Local", "Derived"), "Shared");
  assert.equal(resolveLocalThreadName(null, "Local", "Derived"), "Local");
  assert.equal(resolveLocalThreadName(null, undefined, "Derived"), "Derived");
  assert.equal(resolveLocalThreadName("   ", "Local", "Derived"), "Local");
});

test("names are bounded and reject control characters", () => {
  assert.equal(localThreadNameIsValid("Normal name"), true);
  assert.equal(localThreadNameIsValid(""), true, "clearing is valid");
  assert.equal(localThreadNameIsValid("two\nlines"), false);
  assert.equal(localThreadNameIsValid("bell"), false);
  assert.equal(
    localThreadNameIsValid("x".repeat(MAX_LOCAL_THREAD_NAME_LENGTH + 1)),
    false,
  );
});

test("an oversized name is truncated rather than rejected on write", () => {
  window.localStorage.clear();
  const long = "x".repeat(MAX_LOCAL_THREAD_NAME_LENGTH + 50);
  const next = setLocalThreadName(PUBKEY, RELAY, ROOT, long);
  assert.equal(next[ROOT].length, MAX_LOCAL_THREAD_NAME_LENGTH);
});

test("a corrupt cache renders as empty instead of throwing", () => {
  // A broken cache must never stop the sidebar rendering.
  window.localStorage.clear();
  window.localStorage.setItem(localThreadNamesKey(PUBKEY, RELAY), "{not json");
  assert.deepEqual(loadLocalThreadNames(PUBKEY, RELAY), {});
  window.localStorage.setItem(localThreadNamesKey(PUBKEY, RELAY), '["array"]');
  assert.deepEqual(loadLocalThreadNames(PUBKEY, RELAY), {});
});

test("no identity or relay yields no names and no write", () => {
  window.localStorage.clear();
  assert.deepEqual(loadLocalThreadNames(null, RELAY), {});
  assert.deepEqual(loadLocalThreadNames(PUBKEY, null), {});
  setLocalThreadName(null, RELAY, ROOT, "orphan");
  assert.deepEqual(loadLocalThreadNames(PUBKEY, RELAY), {});
});
