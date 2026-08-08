/**
 * The settings half of SPEC-2026-08-05 acceptance item 18: the quarantine list
 * and the waiting-for-author indicator, driven by real sidecar responses over
 * the real IPC boundary rather than by props a test handed straight to the
 * presentational components.
 *
 * The card is mounted with the real `useEdgeStatus` poller underneath it, so
 * the payloads travel the whole route -- `__TAURI_INTERNALS__.invoke` ->
 * `invokeTauri` -> the shape validators in `api/edgeStatus.ts` -> the
 * components. A malformed field or a renamed command fails here.
 *
 * The first test is the one that matters most: on the machine everyone has,
 * where `BUZZ_EDGE_RELAY_URL` is unset and every command rejects with the
 * sentinel, the card must contribute NOTHING to the DOM. Not an empty state,
 * not a banner, not a heading with zero rows under it.
 */

import assert from "node:assert/strict";
import { afterEach, beforeEach, test } from "node:test";

import { JSDOM } from "jsdom";

const dom = new JSDOM("<!doctype html><html><body></body></html>", {
  pretendToBeVisual: true,
  url: "http://localhost",
});

// The poller arms an interval; recording it keeps request counts deterministic.
const activeIntervals = new Map();
let nextIntervalId = 1;
dom.window.setInterval = (fn, ms) => {
  const id = nextIntervalId++;
  activeIntervals.set(id, { fn, ms });
  return id;
};
dom.window.clearInterval = (id) => {
  activeIntervals.delete(id);
};

let invokeHandler = () => {
  throw new Error("no invoke handler installed");
};
const invokeCalls = [];

dom.window.__TAURI_INTERNALS__ = {
  invoke: async (command, args) => {
    invokeCalls.push({ command, args });
    return invokeHandler(command, args);
  },
};

Object.assign(globalThis, {
  document: dom.window.document,
  Event: dom.window.Event,
  HTMLElement: dom.window.HTMLElement,
  IS_REACT_ACT_ENVIRONMENT: true,
  window: dom.window,
});

const React = (await import("react")).default;
const { act } = await import("react");
const { createRoot } = await import("react-dom/client");
const { useEdgeStatus } = await import("../hooks.ts");
const { EdgeSyncSettingsCard, isEdgeSyncSectionVisible } = await import(
  "./EdgeSyncSettingsCard.tsx"
);

const UNAVAILABLE = "edge sidecar not running";

const AUTHOR_A =
  "44b8e82baaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const AUTHOR_B =
  "9c1177ccbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const QUARANTINED_ID =
  "8e39cba681211b3782d0e4483e9343719b9b7be66515252da5491f26421896b1";
const CARRIED_ID =
  "1111111122222222333333334444444455555555666666667777777788888888";

const SUMMARY = {
  pending: 2,
  pendingViaDigest: 4,
  claimed: 1,
  syncedExact: 9,
  syncedViaDigest: 3,
  quarantined: 2,
};

const WAITING_AUTHORS = [
  {
    // Genuinely waiting: this identity is offline and only it can republish.
    author: AUTHOR_A,
    pending: 2,
    ancestorBlocked: 0,
    pendingViaDigest: 4,
    oldestPendingAt: 1_780_000_000,
  },
  {
    // NOT waiting for anyone: present, draining, and correctly claiming
    // nothing because an ancestor never reached canonical history.
    author: AUTHOR_B,
    pending: 0,
    ancestorBlocked: 3,
    pendingViaDigest: 0,
    oldestPendingAt: 1_780_000_500,
  },
];

const QUARANTINED = [
  {
    eventId: QUARANTINED_ID,
    channelId:
      "0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f",
    author: AUTHOR_A,
    createdAt: 1_780_000_000,
    attempts: 4,
    reason: "upstream rejected: created_at outside ingest window",
    carriedByDigest: false,
    demotionReason: null,
    updatedAt: 1_780_000_600,
  },
  {
    eventId: CARRIED_ID,
    channelId:
      "0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f",
    author: AUTHOR_B,
    createdAt: 1_779_000_000,
    attempts: 7,
    reason: "upstream rejected: created_at outside ingest window",
    carriedByDigest: true,
    demotionReason: "older than the relay drift window",
    updatedAt: 1_780_000_900,
  },
];

function healthyHandler(command) {
  if (command === "edge_delivery_summary") return { ...SUMMARY };
  if (command === "edge_waiting_authors")
    return WAITING_AUTHORS.map((row) => ({ ...row }));
  if (command === "edge_quarantined_events")
    return QUARANTINED.map((row) => ({ ...row }));
  throw new Error(`unexpected command ${command}`);
}

let root = null;
let container = null;

/** Mounts the card with the real poller feeding it. */
function Harness() {
  const status = useEdgeStatus();
  return React.createElement(EdgeSyncSettingsCard, { status });
}

async function mount() {
  container = dom.window.document.createElement("div");
  dom.window.document.body.appendChild(container);
  root = createRoot(container);
  await act(async () => {
    root.render(React.createElement(Harness));
  });
  // Let the summary poll settle, then the quarantine fetch it triggers.
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();
  });
}

beforeEach(() => {
  invokeCalls.length = 0;
  activeIntervals.clear();
});

afterEach(async () => {
  if (root) {
    const current = root;
    await act(async () => {
      current.unmount();
    });
    root = null;
  }
  container?.remove();
  container = null;
});

test("no sidecar: the settings surface contributes nothing to the DOM", async () => {
  invokeHandler = () => {
    throw new Error(UNAVAILABLE);
  };

  await mount();

  assert.equal(container.innerHTML, "");
  assert.equal(container.textContent, "");
  // And it never even asked for the quarantine page: there is nothing to
  // quarantine on a machine with no local store.
  assert.equal(
    invokeCalls.filter((call) => call.command === "edge_quarantined_events")
      .length,
    0,
  );
});

test("no sidecar: the settings section itself is hidden, before and after the first reply", async () => {
  // Pre-reply. `unavailable` is still false here, which is exactly why the
  // gate keys on evidence of a sidecar instead: gating on `!unavailable` would
  // show the nav entry to every user for one IPC round-trip and then remove it.
  assert.equal(
    isEdgeSyncSectionVisible({
      summary: null,
      waitingAuthors: [],
      unavailable: false,
      error: null,
      isLoading: true,
      refresh: () => {},
    }),
    false,
  );
  // Post-reply.
  assert.equal(
    isEdgeSyncSectionVisible({
      summary: null,
      waitingAuthors: [],
      unavailable: true,
      error: null,
      isLoading: false,
      refresh: () => {},
    }),
    false,
  );
  // A real fault on a machine that DOES have a sidecar must reach the operator.
  assert.equal(
    isEdgeSyncSectionVisible({
      summary: null,
      waitingAuthors: [],
      unavailable: false,
      error: new Error("relay returned 503 Service Unavailable"),
      isLoading: false,
      refresh: () => {},
    }),
    true,
  );
});

test("a running sidecar drives the quarantine list from its own response", async () => {
  invokeHandler = healthyHandler;

  await mount();

  const rows = [...container.querySelectorAll("li")];
  const quarantineRows = rows.filter((row) =>
    row.textContent.includes("upstream rejected"),
  );
  assert.equal(quarantineRows.length, 2);

  // The digest-carried row must NOT offer a retry: the sidecar refuses it, so
  // the button's only possible effect is to report its own failure.
  const carriedRow = quarantineRows.find((row) =>
    row.textContent.includes("Carried by digest"),
  );
  assert.ok(carriedRow, "the carriedByDigest row should be marked as such");
  // Scoped to the retry affordance by its label: the row also carries a
  // `<PubKey>` copy button, and counting every button would pass whether or not
  // the retry control was there.
  assert.equal(
    carriedRow.querySelectorAll('[aria-label^="Retry sync for event"]').length,
    0,
    "a digest-carried row must present no retry control, not even a disabled one",
  );
  assert.match(carriedRow.textContent, /older than the relay drift window/);

  // The ordinary row does.
  const retryableRow = quarantineRows.find(
    (row) => !row.textContent.includes("Carried by digest"),
  );
  const retryButtons = [
    ...retryableRow.querySelectorAll('[aria-label^="Retry sync for event"]'),
  ];
  assert.equal(retryButtons.length, 1);
  assert.equal(retryButtons[0].textContent, "Retry");
  assert.equal(retryButtons[0].disabled, false);
});

test("waiting-for-author separates waiting, ancestor-blocked, and digest-carried", async () => {
  invokeHandler = healthyHandler;

  await mount();

  const waitingSection = container.querySelector(
    '[aria-label="Identities with events waiting to sync"]',
  );
  assert.ok(waitingSection, "the waiting-for-author indicator should render");
  assert.match(waitingSection.textContent, /2 events queued/);

  // The ancestor-blocked identity gets its OWN section with its own advice.
  // Telling the operator to wait for that author would be wrong: the drain is
  // running and correctly claiming nothing.
  const blockedSection = container.querySelector(
    '[aria-label="Events blocked behind an ancestor that never reached canonical history"]',
  );
  assert.ok(blockedSection, "ancestor-blocked rows need their own section");
  assert.match(blockedSection.textContent, /3 events blocked/);
  assert.match(blockedSection.textContent, /will not help/);

  // The blocked identity must not be counted as waiting for itself.
  assert.ok(
    !waitingSection.textContent.includes("3 events"),
    "ancestor-blocked rows must not be added into the waiting figure",
  );

  // The four pendingViaDigest rows are a footnote with no author attached, and
  // are never summed into either figure above.
  assert.match(
    container.textContent,
    /4 more events will be carried to canonical history by this machine/,
  );
});

test("the summary keeps the two delivery axes on separate labels", async () => {
  invokeHandler = healthyHandler;

  await mount();

  const summary = container.querySelector(
    '[data-testid="edge-delivery-summary"]',
  );
  assert.ok(summary);
  // "Delivered locally" and "Synced to history" are different rows with
  // different counts. SPEC-2026-08-05 criterion (3) forbids collapsing them,
  // and a single "Sent: 11" row would be exactly that.
  assert.match(summary.textContent, /Delivered locally/);
  assert.match(summary.textContent, /Synced to history/);
  assert.ok(
    !/\bSent\b/.test(summary.textContent),
    "the two axes must never be merged into one word",
  );
  // Every state gets its own tile, including the two that are easy to lose.
  assert.match(summary.textContent, /Sync deferred/);
  assert.match(summary.textContent, /Synced via digest/);
});

test("a genuine fault is reported instead of being hidden as 'no sidecar'", async () => {
  invokeHandler = () => {
    // A real failure that merely contains the word "unavailable". Substring
    // sniffing used to swallow these, leaving the status surface silent -- the
    // worst possible answer to "is anything stuck?".
    throw new Error("relay returned 503 Service Unavailable");
  };

  await mount();

  const alert = container.querySelector('[role="alert"]');
  assert.ok(alert, "a genuine fault must be visible, not silence");
  assert.match(alert.textContent, /503 Service Unavailable/);
});
