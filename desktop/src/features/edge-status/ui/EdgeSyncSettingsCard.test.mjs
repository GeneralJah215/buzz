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
/** The most recent `EdgeStatus` the harness rendered with. */
let latestStatus = null;

/** Mounts the card with the real poller feeding it. */
function Harness() {
  const status = useEdgeStatus();
  latestStatus = status;
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

/** Fires every armed interval once and lets the poll it starts settle. */
async function tickPollers() {
  const callbacks = [...activeIntervals.values()].map((entry) => entry.fn);
  await act(async () => {
    for (const callback of callbacks) {
      callback();
    }
  });
  await act(async () => {
    for (let index = 0; index < 6; index += 1) {
      await Promise.resolve();
    }
  });
}

beforeEach(() => {
  invokeCalls.length = 0;
  activeIntervals.clear();
  latestStatus = null;
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

function status(overrides) {
  return {
    summary: null,
    waitingAuthors: [],
    unavailable: false,
    error: null,
    hasAnswered: false,
    isLoading: false,
    refresh: () => {},
    ...overrides,
  };
}

test("no sidecar: the settings section itself is hidden, before and after the first reply", async () => {
  // Pre-reply. `unavailable` is still false here, which is exactly why the
  // gate keys on evidence of a sidecar instead: gating on `!unavailable` would
  // show the nav entry to every user for one IPC round-trip and then remove it.
  assert.equal(isEdgeSyncSectionVisible(status({ isLoading: true })), false);
  // Post-reply.
  assert.equal(isEdgeSyncSectionVisible(status({ unavailable: true })), false);
  // A fault DOES open it. `edge_post` answers with the sentinel both when no
  // binding is configured and on any transport failure, so a machine that never
  // opted in cannot produce a non-sentinel error at all -- reaching one means a
  // binding resolved, which means the user set BUZZ_EDGE_RELAY_URL. Hiding here
  // would leave an operator whose sidecar 503s from app start with no surface
  // whatsoever, which is the exact situation this panel is for.
  assert.equal(
    isEdgeSyncSectionVisible(
      status({ error: new Error("relay returned 503 Service Unavailable") }),
    ),
    true,
  );
  // Positive evidence does open it, and stays latched afterwards.
  assert.equal(
    isEdgeSyncSectionVisible(status({ summary: SUMMARY, hasAnswered: true })),
    true,
  );
  assert.equal(
    isEdgeSyncSectionVisible(status({ unavailable: true, hasAnswered: true })),
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
  invokeHandler = (command) => {
    if (command === "edge_delivery_summary") {
      // A sidecar that answered with a payload this build cannot read. That is
      // positive evidence of a sidecar (only a running one produces a body),
      // and an `EdgeStatusShapeError` exists precisely so it is never mistaken
      // for the benign "not running" case.
      return {};
    }
    if (command === "edge_waiting_authors") return [];
    throw new Error("relay returned 503 Service Unavailable");
  };

  await mount();

  const alert = container.querySelector('[role="alert"]');
  assert.ok(alert, "a genuine fault must be visible, not silence");
  assert.match(alert.textContent, /edge_delivery_summary/);
  assert.match(alert.textContent, /'pending'/);
});

test("a sidecar that only ever faults still gets a surface", async () => {
  invokeHandler = () => {
    // A sidecar that is plainly there and 503s from app start, never once
    // succeeding. `hasAnswered` never latches, because no payload comes back.
    // Hiding on that leaves the operator nothing at all -- no list, no banner,
    // one console line -- for the exact situation this panel exists for.
    throw new Error("relay returned 503 Service Unavailable");
  };

  await mount();

  assert.equal(latestStatus.hasAnswered, false, "no payload ever came back");
  assert.notEqual(latestStatus.error, null, "but something answered unhappily");
  assert.equal(
    isEdgeSyncSectionVisible(latestStatus),
    true,
    "a machine that has a sidecar and cannot talk to it must still get the panel",
  );
});

test("a user with no sidecar still sees nothing at all", async () => {
  invokeHandler = () => {
    // The sentinel, exactly. `edge_post` answers with this both when no binding
    // is configured and on any transport failure, so this is what every machine
    // that never opted in produces -- and the only thing it can produce.
    throw new Error("edge sidecar not running");
  };

  await mount();

  assert.equal(latestStatus.hasAnswered, false);
  assert.equal(latestStatus.error, null, "the sentinel is not a fault");
  assert.equal(isEdgeSyncSectionVisible(latestStatus), false);
  assert.equal(container.innerHTML, "");
});

test("the quarantine list reports its own faults instead of swallowing them", async () => {
  // The exact shape of the bug: the summary says two events gave up, and the
  // call that fetches those two rows fails with something that is NOT the
  // sentinel. Swallowing it rendered "Sync failed: 2" beside an empty list --
  // no heading, no alert, no log -- on the one surface whose entire job is
  // answering "is anything stuck?".
  invokeHandler = (command) => {
    if (command === "edge_quarantined_events") {
      throw new Error("sqlite: disk I/O error, store unavailable");
    }
    return healthyHandler(command);
  };
  const warnings = [];
  const originalWarn = console.warn;
  console.warn = (...args) => warnings.push(args);

  try {
    await mount();
  } finally {
    console.warn = originalWarn;
  }

  const alert = container.querySelector('[role="alert"]');
  assert.ok(alert, "a failed quarantine fetch must be reported, not silent");
  assert.match(alert.textContent, /disk I\/O error/);
  // The rest of the card still works -- one failed sub-fetch is not a reason
  // to blank the summary the operator came for.
  assert.ok(container.querySelector('[data-testid="edge-delivery-summary"]'));
  // And it left a log with context, not just a UI string.
  const guardrail = warnings.find(
    (args) =>
      typeof args[0] === "string" &&
      args[0].startsWith("[GUARDRAIL]") &&
      args[0].includes("edge_quarantined_events"),
  );
  assert.ok(guardrail, `expected a [GUARDRAIL] log, got ${warnings.length}`);
  assert.match(String(guardrail[1]), /disk I\/O error/);
});

test("an EdgeStatusShapeError from the quarantine page is reported too", async () => {
  // The shape error's stated purpose is to never be mistaken for the benign
  // case. It used to be swallowed by exactly the same branch.
  invokeHandler = (command) => {
    if (command === "edge_quarantined_events") {
      return [{ eventId: 42 }];
    }
    return healthyHandler(command);
  };
  const originalWarn = console.warn;
  console.warn = () => {};

  try {
    await mount();
  } finally {
    console.warn = originalWarn;
  }

  const alert = container.querySelector('[role="alert"]');
  assert.ok(alert, "a malformed quarantine payload must be reported");
  assert.match(alert.textContent, /edge_quarantined_events/);
});

test("a sidecar blip does not take the section away mid-triage", async () => {
  // One sentinel reply after a healthy poll used to drop `edge-sync` out of
  // `visibleSections`, and SettingsView's fallback effect then navigated the
  // operator to Profile -- during exactly the sidecar restart they had opened
  // this section to watch.
  invokeHandler = healthyHandler;
  await mount();
  assert.equal(isEdgeSyncSectionVisible(latestStatus), true);

  invokeHandler = () => {
    throw new Error(UNAVAILABLE);
  };
  await tickPollers();

  assert.equal(latestStatus.summary, null, "the blip must have landed");
  assert.equal(latestStatus.unavailable, true);
  assert.equal(
    isEdgeSyncSectionVisible(latestStatus),
    true,
    "the section must survive a sidecar restart",
  );
  assert.ok(
    container.querySelector('[data-testid="settings-edge-sync"]'),
    "the panel must stay mounted while the sidecar restarts",
  );
});
