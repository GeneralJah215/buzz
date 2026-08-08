/**
 * Retry-button behaviour for the quarantine list.
 *
 * A quarantined event is one canonical history never received. The retry
 * affordance therefore has three hard requirements: it must not be clickable
 * twice while a requeue is in flight, a requeue that fails (or that the sidecar
 * declines) must say so on screen, and the three ways the sidecar can decline
 * must not all render as the same sentence. The last one is the point of the
 * outcome field: "there is no such row of yours" and "this machine is already
 * carrying the event upstream" are the same boolean and opposite advice.
 *
 * All of it is asserted against the real component mounted in jsdom.
 */

import assert from "node:assert/strict";
import { afterEach, beforeEach, test } from "node:test";

import { JSDOM } from "jsdom";

const dom = new JSDOM("<!doctype html><html><body></body></html>", {
  pretendToBeVisual: true,
  url: "http://localhost",
});

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
const { QuarantineList } = await import("./QuarantineList.tsx");

const EVENT_ID =
  "8e39cba681211b3782d0e4483e9343719b9b7be66515252da5491f26421896b1";
const OTHER_EVENT_ID =
  "1111111122222222333333334444444455555555666666667777777788888888";

/** The sidecar's three outcomes, in the shape `requeueQuarantinedEvent` returns. */
const REQUEUED = { requeued: true, outcome: "requeued" };
const NOT_FOUND = { requeued: false, outcome: "notFound" };
const CARRIED_BY_DIGEST = { requeued: false, outcome: "carriedByDigest" };

function quarantinedEvent(overrides = {}) {
  return {
    eventId: EVENT_ID,
    channelId:
      "0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f",
    author: "44b8e82baaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    createdAt: 1_780_000_000,
    attempts: 4,
    reason: "upstream rejected: created_at outside ingest window",
    carriedByDigest: false,
    demotionReason: null,
    updatedAt: 1_780_000_600,
    ...overrides,
  };
}

function deferred() {
  let resolve;
  let reject;
  const promise = new Promise((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

let root = null;
let container = null;

async function render(element) {
  container = dom.window.document.createElement("div");
  dom.window.document.body.appendChild(container);
  root = createRoot(container);
  await act(async () => {
    root.render(element);
  });
}

async function unmount() {
  if (!root) return;
  const current = root;
  root = null;
  await act(async () => {
    current.unmount();
  });
  container?.remove();
  container = null;
}

function retryButtons() {
  return [...container.querySelectorAll("button")].filter((button) =>
    (button.getAttribute("aria-label") ?? "").startsWith("Retry sync"),
  );
}

function alertText() {
  return [...container.querySelectorAll('[role="alert"]')]
    .map((node) => node.textContent)
    .join(" | ");
}

function statusText() {
  return [...container.querySelectorAll('[role="status"]')]
    .map((node) => node.textContent)
    .join(" | ");
}

beforeEach(() => {
  root = null;
  container = null;
});

afterEach(async () => {
  await unmount();
});

// ── Rendering ────────────────────────────────────────────────────────────────

test("renders nothing when there is nothing quarantined", async () => {
  await render(React.createElement(QuarantineList, { events: [] }));
  assert.equal(container.textContent, "");
});

test("shows the reason and attempt count for each row", async () => {
  await render(
    React.createElement(QuarantineList, {
      events: [
        quarantinedEvent(),
        quarantinedEvent({ eventId: OTHER_EVENT_ID, attempts: 1 }),
      ],
    }),
  );

  assert.match(container.textContent, /created_at outside ingest window/);
  assert.match(container.textContent, /4 attempts/);
  assert.match(container.textContent, /1 attempt(?!s)/);
  assert.equal(retryButtons().length, 2);
});

test("event ids are truncated through the canonical helper, never shown raw", async () => {
  await render(
    React.createElement(QuarantineList, { events: [quarantinedEvent()] }),
  );

  // `truncatePubkey` renders `<first 8>…<last 4>`.
  assert.match(container.textContent, /8e39cba6…96b1/);
  assert.ok(
    !container.textContent.includes(EVENT_ID),
    "the full 64-char id must not be rendered inline",
  );
});

/**
 * The demotion reason is the actionable half of a demoted row: "older than the
 * relay drift window" is routine and "permanently rejected upstream" is not,
 * and the row cannot be triaged from the state name alone.
 */
test("a demotion reason is shown when the sidecar sent one", async () => {
  await render(
    React.createElement(QuarantineList, {
      events: [
        quarantinedEvent({
          demotionReason: "permanently rejected upstream",
        }),
      ],
    }),
  );
  assert.match(container.textContent, /permanently rejected upstream/);
});

test("a row that was never demoted shows no reason line", async () => {
  await render(
    React.createElement(QuarantineList, {
      events: [quarantinedEvent({ demotionReason: null })],
    }),
  );
  assert.ok(
    !/Left the direct path/.test(container.textContent),
    "an undemoted row must not sprout an empty explanation",
  );
});

// ── Rows the digest already carries ──────────────────────────────────────────

/**
 * The defect this pins: a `carriedByDigest` row used to render an ordinary
 * Retry button. The sidecar refuses that requeue outright — the edge identity
 * is already carrying the event upstream — so the button's only possible
 * effect was to tell the operator their retry was declined. It must not be
 * presented as an available action at all.
 */
test("a digest-carried row offers no retry control", async () => {
  let calls = 0;
  await render(
    React.createElement(QuarantineList, {
      events: [
        quarantinedEvent({
          carriedByDigest: true,
          demotionReason: "older than the relay drift window",
        }),
        quarantinedEvent({ eventId: OTHER_EVENT_ID }),
      ],
      onRetry: () => {
        calls += 1;
        return Promise.resolve(REQUEUED);
      },
    }),
  );

  const buttons = retryButtons();
  assert.equal(
    buttons.length,
    1,
    "only the ordinary row gets a retry button, and it is the other one",
  );
  assert.match(
    buttons[0].getAttribute("aria-label"),
    /1111111…8888/,
    "the surviving button belongs to the row that can actually be retried",
  );
  assert.equal(calls, 0);
});

test("a digest-carried row is visibly distinguished and says why", async () => {
  await render(
    React.createElement(QuarantineList, {
      events: [
        quarantinedEvent({
          carriedByDigest: true,
          demotionReason: "older than the relay drift window",
        }),
      ],
    }),
  );

  assert.match(container.textContent, /Carried by digest/);
  assert.match(container.textContent, /catch-up digest/);
  assert.match(
    container.textContent,
    /older than the relay drift window/,
    "the operator must be told why the row left the direct path",
  );
  // The marking is structural, not only prose: the row itself is styled apart
  // from an ordinary quarantine row.
  const [row] = container.querySelectorAll("li");
  assert.match(row.className, /amber/);
});

test("an ordinary quarantine row is not marked as carried", async () => {
  await render(
    React.createElement(QuarantineList, { events: [quarantinedEvent()] }),
  );
  assert.ok(!/Carried by digest/.test(container.textContent));
  assert.equal(retryButtons().length, 1);
  const [row] = container.querySelectorAll("li");
  assert.ok(!/amber/.test(row.className));
});

// ── In-flight disabling ──────────────────────────────────────────────────────

test("the retry button disables while the requeue is in flight", async () => {
  const pending = deferred();
  await render(
    React.createElement(QuarantineList, {
      events: [quarantinedEvent()],
      onRetry: () => pending.promise,
    }),
  );

  const [button] = retryButtons();
  assert.equal(button.disabled, false, "enabled before the click");

  await act(async () => {
    button.click();
  });

  const [inFlight] = retryButtons();
  assert.equal(inFlight.disabled, true, "disabled while in flight");
  assert.match(inFlight.textContent, /Retrying/);

  // Not found, so the row is still on screen and the operator may look again.
  await act(async () => {
    pending.resolve(NOT_FOUND);
  });

  const [settled] = retryButtons();
  assert.equal(settled.disabled, false, "re-enabled once settled");
  assert.match(settled.textContent, /^Retry$/);
});

test("a second click while in flight cannot issue a second requeue", async () => {
  const pending = deferred();
  let calls = 0;
  await render(
    React.createElement(QuarantineList, {
      events: [quarantinedEvent()],
      onRetry: () => {
        calls += 1;
        return pending.promise;
      },
    }),
  );

  const [button] = retryButtons();
  await act(async () => {
    button.click();
  });
  await act(async () => {
    retryButtons()[0].click();
  });

  assert.equal(calls, 1, "the disabled button must not fire again");

  await act(async () => {
    pending.resolve(REQUEUED);
  });
});

test("only the row being retried is disabled", async () => {
  const pending = deferred();
  await render(
    React.createElement(QuarantineList, {
      events: [
        quarantinedEvent(),
        quarantinedEvent({ eventId: OTHER_EVENT_ID }),
      ],
      onRetry: () => pending.promise,
    }),
  );

  await act(async () => {
    retryButtons()[0].click();
  });

  const buttons = retryButtons();
  assert.equal(buttons[0].disabled, true);
  assert.equal(buttons[1].disabled, false);

  await act(async () => {
    pending.resolve(REQUEUED);
  });
});

// ── Outcome reporting ────────────────────────────────────────────────────────

test("a rejected retry is surfaced, not swallowed", async () => {
  await render(
    React.createElement(QuarantineList, {
      events: [quarantinedEvent()],
      onRetry: () => Promise.reject(new Error("edge sidecar not running")),
    }),
  );

  await act(async () => {
    retryButtons()[0].click();
  });

  assert.match(alertText(), /Retry failed: edge sidecar not running/);
  assert.equal(retryButtons()[0].disabled, false, "the button recovers");
});

test("a notFound requeue is surfaced too", async () => {
  await render(
    React.createElement(QuarantineList, {
      events: [quarantinedEvent()],
      onRetry: () => Promise.resolve(NOT_FOUND),
    }),
  );

  await act(async () => {
    retryButtons()[0].click();
  });

  assert.match(alertText(), /no quarantined event with this id for you/);
});

/**
 * The defect-8 pin. Before the outcome existed, `carriedByDigest` and
 * `notFound` were both a bare `false` and rendered the same "the sidecar
 * declined the retry — this event is no longer queued" — which is wrong advice
 * for a row that is on its way upstream right now. A build that collapses them
 * again fails here.
 */
test("carriedByDigest and notFound are reported differently", async () => {
  const messages = [];
  for (const result of [NOT_FOUND, CARRIED_BY_DIGEST]) {
    await render(
      React.createElement(QuarantineList, {
        events: [quarantinedEvent()],
        onRetry: () => Promise.resolve(result),
      }),
    );
    await act(async () => {
      retryButtons()[0].click();
    });
    messages.push(`${alertText()} ${statusText()}`.trim());
    await unmount();
  }

  const [notFound, carried] = messages;
  assert.ok(
    notFound.length > 0 && carried.length > 0,
    "both must say something",
  );
  assert.notEqual(
    notFound,
    carried,
    "two different outcomes must not render one sentence",
  );
  assert.match(carried, /catch-up digest/);
  assert.ok(
    !/no quarantined event with this id/.test(carried),
    "a row on its way upstream must not be reported as missing",
  );
});

/**
 * A retry the sidecar answered `carriedByDigest` has told us the row is on the
 * digest path after all. Leaving the button up would invite the operator to
 * keep pressing something that can never work.
 */
test("a carriedByDigest answer retires the retry button", async () => {
  let calls = 0;
  await render(
    React.createElement(QuarantineList, {
      events: [quarantinedEvent()],
      onRetry: () => {
        calls += 1;
        return Promise.resolve(CARRIED_BY_DIGEST);
      },
    }),
  );

  await act(async () => {
    retryButtons()[0].click();
  });

  assert.equal(calls, 1);
  assert.equal(
    retryButtons().length,
    0,
    "the control disappears once we know the digest carries this row",
  );
  assert.match(container.textContent, /Carried by digest/);
});

/** A newer sidecar's outcome must name itself rather than be guessed at. */
test("an unrecognised outcome is reported as unrecognised", async () => {
  await render(
    React.createElement(QuarantineList, {
      events: [quarantinedEvent()],
      onRetry: () =>
        Promise.resolve({ requeued: false, outcome: "deferredUntilMonday" }),
    }),
  );

  await act(async () => {
    retryButtons()[0].click();
  });

  assert.match(alertText(), /does not recognise/);
  assert.match(alertText(), /deferredUntilMonday/);
  assert.equal(
    retryButtons()[0].disabled,
    false,
    "an outcome we cannot read must not silently retire the control",
  );
});

/**
 * The row is owned by the caller, so it stays on screen after a successful
 * requeue until something refetches. Before this, that row looked untouched:
 * the operator clicked Retry again, the sidecar answered "not quarantined any
 * more", and a retry that WORKED reported itself as a failure.
 */
test("a successful retry says so and stops offering a second click", async () => {
  let calls = 0;
  await render(
    React.createElement(QuarantineList, {
      events: [quarantinedEvent()],
      // Exactly what the sidecar does: the second requeue of an event that has
      // already left quarantine is declined.
      onRetry: () => {
        calls += 1;
        return Promise.resolve(calls === 1 ? REQUEUED : NOT_FOUND);
      },
    }),
  );

  await act(async () => {
    retryButtons()[0].click();
  });

  assert.match(statusText(), /Sent back to the sync queue/);
  assert.equal(alertText(), "", "a working retry must not read as a failure");

  const [button] = retryButtons();
  assert.equal(button.disabled, true, "no second click on a queued row");
  assert.match(button.textContent, /Queued/);

  await act(async () => {
    button.click();
  });
  assert.equal(calls, 1, "the sidecar is not asked twice");
  assert.equal(alertText(), "", "and no declined-retry alert appears");
});

test("a successful retry on one row leaves the others clickable", async () => {
  await render(
    React.createElement(QuarantineList, {
      events: [
        quarantinedEvent(),
        quarantinedEvent({ eventId: OTHER_EVENT_ID }),
      ],
      onRetry: () => Promise.resolve(REQUEUED),
    }),
  );

  await act(async () => {
    retryButtons()[0].click();
  });

  const buttons = retryButtons();
  assert.equal(buttons[0].disabled, true);
  assert.equal(buttons[1].disabled, false, "the other row is untouched");
  assert.equal(
    container.querySelectorAll('[role="status"]').length,
    1,
    "exactly one row reports success",
  );
});

test("a successful retry leaves no error behind", async () => {
  let result = NOT_FOUND;
  await render(
    React.createElement(QuarantineList, {
      events: [quarantinedEvent()],
      onRetry: () => Promise.resolve(result),
    }),
  );

  await act(async () => {
    retryButtons()[0].click();
  });
  assert.match(alertText(), /no quarantined event with this id for you/);

  result = REQUEUED;
  await act(async () => {
    retryButtons()[0].click();
  });
  assert.equal(alertText(), "", "the stale failure must clear on a good retry");
});

test("a failure on one row does not mark another row failed", async () => {
  await render(
    React.createElement(QuarantineList, {
      events: [
        quarantinedEvent(),
        quarantinedEvent({ eventId: OTHER_EVENT_ID }),
      ],
      onRetry: (eventId) =>
        eventId === EVENT_ID
          ? Promise.reject(new Error("boom"))
          : Promise.resolve(REQUEUED),
    }),
  );

  await act(async () => {
    retryButtons()[0].click();
  });
  await act(async () => {
    retryButtons()[1].click();
  });

  assert.equal(
    container.querySelectorAll('[role="alert"]').length,
    1,
    "exactly one row reports a failure",
  );
});
