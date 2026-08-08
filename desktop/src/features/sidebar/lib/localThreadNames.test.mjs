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
      key: (i) => [...store.keys()][i] ?? null,
      get length() {
        return store.size;
      },
    },
  };
}

const {
  loadLocalThreadNames,
  localThreadNameIsValid,
  localThreadNamesKey,
  MAX_LOCAL_THREAD_NAME_LENGTH,
  MAX_LOCAL_THREAD_NAMES,
  normalizeLocalThreadNameRelay,
  resolveLocalThreadName,
  setLocalThreadName,
} = await import("./localThreadNames.ts");

const PUBKEY = "a".repeat(64);
const OTHER = "b".repeat(64);
const RELAY = "wss://relay.example.com";
const ROOT = "c".repeat(64);

/** Every key currently in the shim, so "no write happened" is checkable. */
function storedKeys() {
  const keys = [];
  for (let i = 0; i < window.localStorage.length; i++) {
    keys.push(window.localStorage.key(i));
  }
  return keys;
}

function storedPayload(pubkey = PUBKEY, relayUrl = RELAY) {
  const raw = window.localStorage.getItem(
    localThreadNamesKey(pubkey, relayUrl),
  );
  return raw === null ? null : JSON.parse(raw);
}

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

test("relays differing only by path case stay separate buckets", () => {
  // RFC 3986 paths are case-sensitive, so wss://r.example.com/Alice and
  // .../alice are two different relays. Lowercasing the whole URL — which the
  // shared normaliser does — merges them and leaks names between them.
  window.localStorage.clear();
  const upper = "wss://r.example.com/Alice";
  const lower = "wss://r.example.com/alice";
  setLocalThreadName(PUBKEY, upper, ROOT, "Alice's relay");
  assert.equal(loadLocalThreadNames(PUBKEY, lower)[ROOT], undefined);
  assert.notEqual(
    localThreadNamesKey(PUBKEY, upper),
    localThreadNamesKey(PUBKEY, lower),
  );
  // Scheme and host really are case-insensitive, so those still collapse.
  assert.equal(
    normalizeLocalThreadNameRelay("WSS://R.Example.COM/Alice"),
    "wss://r.example.com/Alice",
  );
  assert.equal(
    localThreadNamesKey(PUBKEY, "WSS://R.Example.COM/Alice"),
    localThreadNamesKey(PUBKEY, upper),
  );
});

test("pubkeys differing only by case stay separate buckets", () => {
  // Nothing lowercases the pubkey, and nothing should: two hex strings that
  // differ in case are not guaranteed to be the same key material here.
  window.localStorage.clear();
  const upper = "A".repeat(64);
  setLocalThreadName(PUBKEY, RELAY, ROOT, "lowercase identity");
  assert.equal(loadLocalThreadNames(upper, RELAY)[ROOT], undefined);
  assert.notEqual(
    localThreadNamesKey(PUBKEY, RELAY),
    localThreadNamesKey(upper, RELAY),
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
  assert.equal(localThreadNameIsValid("bell\u0007"), false);
  assert.equal(
    localThreadNameIsValid("x".repeat(MAX_LOCAL_THREAD_NAME_LENGTH + 1)),
    false,
  );
});

test("the local validator matches the shared one on the hard characters", () => {
  // A local name is meant to be promotable to a shared rename, so anything
  // accepted here must survive the relay-side validator. These five were all
  // accepted locally and rejected by the shared rule.
  for (const value of [
    "line\u2028separator",
    "paragraph\u2029separator",
    "next\u0085line",
    "csi\u009bescape",
    "lone \ud800 surrogate",
  ]) {
    assert.equal(
      localThreadNameIsValid(value),
      false,
      `${JSON.stringify(value)} must be rejected`,
    );
  }
});

test("the length cap counts code points, not UTF-16 units", () => {
  // 100 typed dice are 100 characters and 200 UTF-16 units. Counting units
  // told a user who typed 100 characters they had exceeded 120.
  assert.equal(localThreadNameIsValid("🎲".repeat(100)), true);
  assert.equal(
    localThreadNameIsValid("🎲".repeat(MAX_LOCAL_THREAD_NAME_LENGTH)),
    true,
  );
  assert.equal(
    localThreadNameIsValid("🎲".repeat(MAX_LOCAL_THREAD_NAME_LENGTH + 1)),
    false,
  );
});

test("an oversized name is truncated on code-point boundaries", () => {
  // Over-length is truncated rather than refused — the user's intent is clear
  // — but the cut must never split a surrogate pair, or the stored name holds
  // an unpaired surrogate that the shared validator then rejects and the
  // sidebar renders as a replacement glyph.
  window.localStorage.clear();
  const long = "x".repeat(MAX_LOCAL_THREAD_NAME_LENGTH + 50);
  const next = setLocalThreadName(PUBKEY, RELAY, ROOT, long);
  assert.equal(
    Array.from(next[ROOT]).length,
    MAX_LOCAL_THREAD_NAME_LENGTH,
    "truncation is measured in code points",
  );

  const emoji = `${"x".repeat(MAX_LOCAL_THREAD_NAME_LENGTH - 1)}🎲`;
  const stored = setLocalThreadName(PUBKEY, RELAY, ROOT, emoji)[ROOT];
  assert.equal(stored, emoji, "the trailing emoji survives intact");
  assert.equal(Array.from(stored).length, MAX_LOCAL_THREAD_NAME_LENGTH);
  assert.equal(
    localThreadNameIsValid(stored),
    true,
    "a truncated name must still pass validation",
  );
  // With the u flag a well-formed pair is one code point, so this class can
  // only match a surrogate left stranded by a UTF-16-unit truncation.
  assert.doesNotMatch(
    stored,
    /[\ud800-\udfff]/u,
    "no unpaired surrogate may be stored",
  );
  const halfCut = `${"x".repeat(MAX_LOCAL_THREAD_NAME_LENGTH - 1)}🎲tail`;
  assert.doesNotMatch(
    setLocalThreadName(PUBKEY, RELAY, ROOT, halfCut)[ROOT],
    /[\ud800-\udfff]/u,
    "truncating mid-string must not split the emoji either",
  );
});

test("an invalid name is refused by the write path, not just the button", () => {
  // The disabled Save button is a hint, not a guard. The action layer has to
  // refuse control characters itself, or any other caller stores them.
  window.localStorage.clear();
  setLocalThreadName(PUBKEY, RELAY, ROOT, "Good name");
  const after = setLocalThreadName(PUBKEY, RELAY, ROOT, "two\nlines\u0007bell");
  assert.equal(after[ROOT], "Good name", "the map is returned unchanged");
  assert.equal(loadLocalThreadNames(PUBKEY, RELAY)[ROOT], "Good name");
  for (const bad of ["line\u2028sep", "csi\u009bescape", "lone \ud800 pair"]) {
    setLocalThreadName(PUBKEY, RELAY, ROOT, bad);
    assert.equal(
      loadLocalThreadNames(PUBKEY, RELAY)[ROOT],
      "Good name",
      `${JSON.stringify(bad)} must not reach storage`,
    );
  }
});

test("a corrupt cache renders as empty instead of throwing", () => {
  // A broken cache must never stop the sidebar rendering.
  window.localStorage.clear();
  window.localStorage.setItem(localThreadNamesKey(PUBKEY, RELAY), "{not json");
  assert.deepEqual(loadLocalThreadNames(PUBKEY, RELAY), {});
  window.localStorage.setItem(localThreadNamesKey(PUBKEY, RELAY), '["array"]');
  assert.deepEqual(loadLocalThreadNames(PUBKEY, RELAY), {});
  window.localStorage.setItem(
    localThreadNamesKey(PUBKEY, RELAY),
    '{"root":{"updatedAt":1}}',
  );
  assert.deepEqual(loadLocalThreadNames(PUBKEY, RELAY), {});
});

test("no identity or relay yields no names and no write", () => {
  window.localStorage.clear();
  assert.deepEqual(loadLocalThreadNames(null, RELAY), {});
  assert.deepEqual(loadLocalThreadNames(PUBKEY, null), {});
  setLocalThreadName(null, RELAY, ROOT, "orphan");
  assert.deepEqual(loadLocalThreadNames(PUBKEY, RELAY), {});
  assert.deepEqual(storedKeys(), [], "nothing was written");
});

test("a relay URL that normalises to nothing is refused, not bucketed", () => {
  // "/", "///", "   " and "\t" are all truthy, so a guard on the RAW string
  // lets them through — and every one of them normalises to "", producing one
  // identical key where unrelated sessions read and overwrite each other.
  window.localStorage.clear();
  for (const degenerate of ["   ", "/", "///", "\t", ""]) {
    assert.deepEqual(
      loadLocalThreadNames(PUBKEY, degenerate),
      {},
      `${JSON.stringify(degenerate)} must not resolve to a bucket`,
    );
    assert.deepEqual(
      setLocalThreadName(PUBKEY, degenerate, ROOT, "orphan"),
      {},
      `${JSON.stringify(degenerate)} must not accept a write`,
    );
  }
  assert.deepEqual(storedKeys(), [], "no shared bucket was created");

  // And a real name is not readable through any of them.
  setLocalThreadName(PUBKEY, RELAY, ROOT, "Mine");
  for (const degenerate of ["   ", "/", "///", "\t"]) {
    assert.deepEqual(loadLocalThreadNames(PUBKEY, degenerate), {});
  }
  // An empty pubkey is refused the same way.
  assert.deepEqual(setLocalThreadName("   ", RELAY, ROOT, "orphan"), {});
});

test("names written in the original bare-string format still load", () => {
  // The first shipped format was {rootId: "name"}. Dropping it on the upgrade
  // to timestamped entries would silently erase every name already stored.
  window.localStorage.clear();
  window.localStorage.setItem(
    localThreadNamesKey(PUBKEY, RELAY),
    JSON.stringify({ [ROOT]: "Legacy name", ignored: "" }),
  );
  assert.equal(loadLocalThreadNames(PUBKEY, RELAY)[ROOT], "Legacy name");
  assert.equal(loadLocalThreadNames(PUBKEY, RELAY).ignored, undefined);

  // A later write migrates the whole map to the record form in place.
  const other = "d".repeat(64);
  setLocalThreadName(PUBKEY, RELAY, other, "New name");
  const payload = storedPayload();
  assert.equal(payload[ROOT].name, "Legacy name");
  assert.equal(
    payload[ROOT].updatedAt,
    0,
    "an unknown write time sorts oldest",
  );
  assert.ok(payload[other].updatedAt > 0);
  assert.equal(loadLocalThreadNames(PUBKEY, RELAY)[ROOT], "Legacy name");
});

test("the map is capped and evicts the oldest name first", () => {
  // Nothing else can ever remove an entry: the fallback list is built from a
  // capped activity buffer, so a thread that ages out of it loses its row and
  // with it the only rename control. Without a cap the map grows forever and
  // every rename re-parses and re-serialises all of it.
  window.localStorage.clear();
  for (let i = 0; i < MAX_LOCAL_THREAD_NAMES; i++) {
    setLocalThreadName(PUBKEY, RELAY, `root-${i}`, `Name ${i}`);
  }
  const full = loadLocalThreadNames(PUBKEY, RELAY);
  assert.equal(Object.keys(full).length, MAX_LOCAL_THREAD_NAMES);

  const afterOverflow = setLocalThreadName(PUBKEY, RELAY, "root-new", "Newest");
  assert.equal(
    Object.keys(afterOverflow).length,
    MAX_LOCAL_THREAD_NAMES,
    "the cap holds",
  );
  assert.equal(afterOverflow["root-new"], "Newest", "the new name is kept");
  assert.equal(
    afterOverflow["root-0"],
    undefined,
    "the oldest name is the one evicted",
  );
  assert.equal(
    afterOverflow[`root-${MAX_LOCAL_THREAD_NAMES - 1}`],
    `Name ${MAX_LOCAL_THREAD_NAMES - 1}`,
    "the newest surviving name is untouched",
  );
  assert.equal(
    Object.keys(loadLocalThreadNames(PUBKEY, RELAY)).length,
    MAX_LOCAL_THREAD_NAMES,
    "the cap is persisted, not only returned",
  );
});

test("re-touching a name refreshes its place in the eviction order", () => {
  window.localStorage.clear();
  for (let i = 0; i < MAX_LOCAL_THREAD_NAMES; i++) {
    setLocalThreadName(PUBKEY, RELAY, `root-${i}`, `Name ${i}`);
  }
  setLocalThreadName(PUBKEY, RELAY, "root-0", "Renamed again");
  const after = setLocalThreadName(PUBKEY, RELAY, "root-new", "Newest");
  assert.equal(after["root-0"], "Renamed again", "the touched name survives");
  assert.equal(after["root-1"], undefined, "the next-oldest goes instead");
});
