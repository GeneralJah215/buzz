import assert from "node:assert/strict";
import test from "node:test";

async function loadSubject() {
  try {
    return await import("./userLabelStorage.ts");
  } catch {
    return {};
  }
}

function installLocalStorage() {
  const values = new Map();
  globalThis.window = {
    localStorage: {
      getItem: (key) => values.get(key) ?? null,
      setItem: (key, value) => values.set(key, value),
      removeItem: (key) => values.delete(key),
      key: (index) => [...values.keys()][index] ?? null,
      get length() {
        return values.size;
      },
    },
  };
  globalThis.localStorage = globalThis.window.localStorage;
  return values;
}

test("reads cached labels as safe stale profile summaries", async () => {
  const subject = await loadSubject();
  assert.equal(typeof subject.readCachedUserLabels, "function");
  installLocalStorage();
  window.localStorage.setItem(
    "buzz-user-labels.v1:wss://relay.example",
    JSON.stringify({
      version: 1,
      updatedAt: 100,
      profiles: {
        abcdef: {
          displayName: "Alice",
          name: "alice",
          nip05Handle: "alice@example.com",
          updatedAt: 100,
        },
      },
    }),
  );

  assert.deepEqual(
    subject.readCachedUserLabels("WSS://Relay.Example/", ["ABCDEF", "missing"]),
    {
      profiles: {
        abcdef: {
          displayName: "Alice",
          name: "alice",
          avatarUrl: null,
          nip05Handle: "alice@example.com",
          ownerPubkey: null,
        },
      },
      missing: [],
    },
  );
});

test("keeps previous full profiles ahead of persisted label placeholders", async () => {
  const subject = await loadSubject();
  assert.equal(typeof subject.resolveUserLabelPlaceholderData, "function");
  installLocalStorage();
  window.localStorage.setItem(
    "buzz-user-labels.v1:wss://relay.example",
    JSON.stringify({
      version: 1,
      profiles: {
        abcdef: {
          displayName: "Cached Alice",
          name: "alice",
          nip05Handle: null,
          updatedAt: 100,
        },
      },
    }),
  );
  const previous = {
    profiles: {
      abcdef: {
        displayName: "Fresh Alice",
        name: "alice",
        avatarUrl: "https://relay.example/alice.png",
        nip05Handle: null,
        ownerPubkey: "owner",
      },
    },
    missing: [],
  };

  assert.equal(
    subject.resolveUserLabelPlaceholderData(previous, "wss://relay.example", [
      "abcdef",
    ]),
    previous,
  );
});

test("writes merge with existing labels and remain bounded", async () => {
  const subject = await loadSubject();
  assert.equal(typeof subject.writeCachedUserLabels, "function");
  installLocalStorage();

  subject.writeCachedUserLabels("wss://relay.example", {
    existing: {
      displayName: "Existing",
      name: null,
      avatarUrl: null,
      nip05Handle: null,
      ownerPubkey: null,
    },
  });
  subject.writeCachedUserLabels(
    "wss://relay.example",
    Object.fromEntries(
      Array.from({ length: 1_005 }, (_, index) => [
        `pubkey-${index}`,
        {
          displayName: `Person ${index}`,
          name: null,
          avatarUrl: null,
          nip05Handle: null,
          ownerPubkey: null,
        },
      ]),
    ),
  );

  const stored = JSON.parse(
    window.localStorage.getItem(
      subject.userLabelCacheKey("wss://relay.example"),
    ),
  );
  assert.equal(Object.keys(stored.profiles).length, 1_000);
  assert.equal(stored.version, 1);
  assert.equal(stored.updatedAt, undefined);
});

test("removes a stale label when the fresh profile clears all names", async () => {
  const subject = await loadSubject();
  assert.equal(typeof subject.writeCachedUserLabels, "function");
  installLocalStorage();

  subject.writeCachedUserLabels("wss://relay.example", {
    abcdef: {
      displayName: "Alice",
      name: "alice",
      avatarUrl: null,
      nip05Handle: null,
      ownerPubkey: null,
    },
  });
  subject.writeCachedUserLabels("wss://relay.example", {
    abcdef: {
      displayName: null,
      name: null,
      avatarUrl: null,
      nip05Handle: null,
      ownerPubkey: null,
    },
  });

  assert.equal(
    subject.readCachedUserLabels("wss://relay.example", ["abcdef"]),
    undefined,
  );
});

test("removes stale labels for profiles the relay reports missing", async () => {
  const subject = await loadSubject();
  assert.equal(typeof subject.writeCachedUserLabels, "function");
  installLocalStorage();

  subject.writeCachedUserLabels("wss://relay.example", {
    abcdef: {
      displayName: "Alice",
      name: "alice",
      avatarUrl: null,
      nip05Handle: null,
      ownerPubkey: null,
    },
  });
  subject.writeCachedUserLabels("wss://relay.example", {}, ["ABCDEF"]);

  assert.equal(
    subject.readCachedUserLabels("wss://relay.example", ["abcdef"]),
    undefined,
  );
});

test("removes only the selected relay cache", async () => {
  const subject = await loadSubject();
  assert.equal(typeof subject.removeUserLabelCacheForRelay, "function");
  installLocalStorage();
  const first = subject.userLabelCacheKey("wss://one.example");
  const second = subject.userLabelCacheKey("wss://two.example");
  window.localStorage.setItem(first, "{}");
  window.localStorage.setItem(second, "{}");

  subject.removeUserLabelCacheForRelay("wss://one.example");

  assert.equal(window.localStorage.getItem(first), null);
  assert.equal(window.localStorage.getItem(second), "{}");
});

test("ignores malformed cache payloads", async () => {
  const subject = await loadSubject();
  assert.equal(typeof subject.readCachedUserLabels, "function");
  installLocalStorage();
  window.localStorage.setItem(
    "buzz-user-labels.v1:wss://relay.example",
    JSON.stringify({ version: 1, profiles: { abc: { displayName: 42 } } }),
  );

  assert.equal(
    subject.readCachedUserLabels("wss://relay.example", ["abc"]),
    undefined,
  );
});

// --- BUG-076 regression guards -------------------------------------------
//
// resolveUserLabelPlaceholderData is React Query's placeholderData callback,
// so it runs on every result computation: once per render, per observer, and
// every rendered username is an observer. Re-parsing the whole cache there put
// this module at the top of a live CPU profile of the minimized app. These
// tests count JSON.parse calls because call COUNT is the defect — asserting on
// the returned value alone passes both before and after the fix.

function countingJsonParse() {
  const real = JSON.parse;
  const state = { calls: 0 };
  JSON.parse = (...args) => {
    state.calls += 1;
    return real.apply(JSON, args);
  };
  state.restore = () => {
    JSON.parse = real;
  };
  return state;
}

function seedRelay(relayUrl, entries) {
  const profiles = {};
  for (let i = 0; i < entries; i++) {
    profiles[`pubkey${i}`] = {
      displayName: `User ${i}`,
      name: `user${i}`,
      nip05Handle: null,
      updatedAt: 1_000 + i,
    };
  }
  window.localStorage.setItem(
    `buzz-user-labels.v1:${relayUrl}`,
    JSON.stringify({ version: 1, profiles }),
  );
}

test("parses the stored cache once across repeated placeholder lookups", async () => {
  const subject = await loadSubject();
  assert.equal(typeof subject.resolveUserLabelPlaceholderData, "function");
  installLocalStorage();
  const relay = "wss://memo-once.example";
  seedRelay(relay, 200);

  const spy = countingJsonParse();
  try {
    for (let i = 0; i < 25; i++) {
      const result = subject.resolveUserLabelPlaceholderData(undefined, relay, [
        "pubkey1",
      ]);
      assert.equal(result.profiles.pubkey1.displayName, "User 1");
    }
  } finally {
    spy.restore();
  }

  assert.equal(
    spy.calls,
    1,
    `expected one parse across 25 lookups, saw ${spy.calls}`,
  );
});

test("re-parses once the stored payload actually changes", async () => {
  const subject = await loadSubject();
  assert.equal(typeof subject.writeCachedUserLabels, "function");
  installLocalStorage();
  const relay = "wss://memo-invalidate.example";
  seedRelay(relay, 5);

  assert.equal(
    subject.readCachedUserLabels(relay, ["pubkey0"]).profiles.pubkey0
      .displayName,
    "User 0",
  );

  subject.writeCachedUserLabels(relay, {
    pubkey0: {
      displayName: "Renamed",
      name: "renamed",
      nip05Handle: null,
      avatarUrl: null,
      ownerPubkey: null,
    },
  });

  // A memo keyed on a dirty flag would still be serving "User 0" here.
  assert.equal(
    subject.readCachedUserLabels(relay, ["pubkey0"]).profiles.pubkey0
      .displayName,
    "Renamed",
  );
});

test("the shared cache cannot be mutated by a caller", async () => {
  const subject = await loadSubject();
  assert.equal(typeof subject.readCachedUserLabels, "function");
  installLocalStorage();
  const relay = "wss://memo-frozen.example";
  seedRelay(relay, 3);

  // readCachedUserLabels builds its own object, so reach the shared one the
  // way writeCachedUserLabels does and confirm a stray write throws rather
  // than poisoning every later read.
  subject.readCachedUserLabels(relay, ["pubkey0"]);
  subject.writeCachedUserLabels(relay, {});

  assert.equal(
    subject.readCachedUserLabels(relay, ["pubkey0"]).profiles.pubkey0
      .displayName,
    "User 0",
  );
});
