/**
 * Retry-button behaviour for the quarantine list.
 *
 * A quarantined event is one canonical history never received. The retry
 * affordance therefore has two hard requirements: it must not be clickable
 * twice while a requeue is in flight, and a requeue that fails (or that the
 * sidecar declines) must say so on screen. Both are asserted against the real
 * component mounted in jsdom.
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

function quarantinedEvent(overrides = {}) {
  return {
    eventId: EVENT_ID,
    channelId:
      "0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f",
    author: "44b8e82baaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    createdAt: 1_780_000_000,
    attempts: 4,
    reason: "upstream rejected: created_at outside ingest window",
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

  await act(async () => {
    pending.resolve(true);
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
    pending.resolve(true);
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
    pending.resolve(true);
  });
});

// ── Failure reporting ────────────────────────────────────────────────────────

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

test("a declined requeue (resolves false) is surfaced too", async () => {
  await render(
    React.createElement(QuarantineList, {
      events: [quarantinedEvent()],
      onRetry: () => Promise.resolve(false),
    }),
  );

  await act(async () => {
    retryButtons()[0].click();
  });

  assert.match(alertText(), /declined the retry/);
});

test("a successful retry leaves no error behind", async () => {
  let outcome = false;
  await render(
    React.createElement(QuarantineList, {
      events: [quarantinedEvent()],
      onRetry: () => Promise.resolve(outcome),
    }),
  );

  await act(async () => {
    retryButtons()[0].click();
  });
  assert.match(alertText(), /declined the retry/);

  outcome = true;
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
          : Promise.resolve(true),
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
