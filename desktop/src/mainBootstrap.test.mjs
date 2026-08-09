/**
 * BUG-054 guardrails: a failed bootstrap must be visible, and an optional
 * bootstrap step must never be able to stop the app from rendering.
 *
 * `main.tsx` itself cannot be imported here (it pulls in React, fontsource CSS
 * and the whole app tree), so the behaviour lives in `mainBootstrap.ts` and the
 * structural facts about `main.tsx` are asserted against its source. Those
 * source assertions are the ones that fail if someone restores `void
 * bootstrap();`.
 */
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { afterEach, beforeEach, test } from "node:test";

import { JSDOM } from "jsdom";

import {
  composeBootstrapFailureDetail,
  describeBootstrapError,
  extractPreloadChunkUrl,
  handleVitePreloadError,
  renderBootstrapFailure,
  resetPreloadFailureForTests,
  runOptionalBootstrapStep,
} from "./mainBootstrap.ts";

const __dirname = path.dirname(fileURLToPath(import.meta.url));

function newDocument() {
  return new JSDOM(
    '<!doctype html><html><body><div id="root"></div></body></html>',
    { url: "http://localhost" },
  ).window.document;
}

function recordingConsole() {
  const calls = [];
  return { calls, error: (...args) => calls.push(args) };
}

let target;

beforeEach(() => {
  resetPreloadFailureForTests();
  target = { console: recordingConsole(), document: newDocument() };
});

afterEach(() => {
  resetPreloadFailureForTests();
});

// ---------------------------------------------------------------------------
// Visible: the failure reaches the DOM, not just the console
// ---------------------------------------------------------------------------

test("bootstrap_failure_renders_an_alert_into_the_root_element", () => {
  renderBootstrapFailure(
    new Error("Failed to fetch dynamically imported module"),
    target,
  );

  const surface = target.document.querySelector(
    '[data-testid="bootstrap-failure"]',
  );
  assert.ok(surface, "expected a bootstrap failure surface in the DOM");
  assert.equal(surface.getAttribute("role"), "alert");
  assert.equal(surface.parentElement.id, "root");
});

test("bootstrap_failure_surface_names_the_cause", () => {
  renderBootstrapFailure(new Error("chunk 4f2a exploded"), target);

  const detail = target.document.querySelector(
    '[data-testid="bootstrap-failure-detail"]',
  );
  assert.match(detail.textContent, /chunk 4f2a exploded/u);
});

test("bootstrap_failure_also_logs_to_the_console_with_the_error_object", () => {
  const error = new Error("boom");
  renderBootstrapFailure(error, target);

  const logged = target.console.calls.at(0);
  assert.match(logged[0], /^\[STARTUP\] bootstrap failed:/u);
  assert.equal(logged[1], error, "the original error must reach the console");
});

test("bootstrap_failure_falls_back_to_the_body_when_root_is_missing", () => {
  const document = new JSDOM("<!doctype html><html><body></body></html>").window
    .document;
  renderBootstrapFailure(new Error("no root"), { ...target, document });

  assert.ok(
    document.querySelector('[data-testid="bootstrap-failure"]'),
    "expected the surface to fall back to document.body",
  );
});

test("bootstrap_failure_replaces_whatever_was_in_root", () => {
  target.document.getElementById("root").textContent = "half-rendered";
  renderBootstrapFailure(new Error("boom"), target);

  assert.equal(
    target.document.getElementById("root").children.length,
    1,
    "the failure surface must not stack under leftover boot markup",
  );
});

test("a_failing_dom_write_is_reported_not_swallowed", () => {
  const document = newDocument();
  document.getElementById("root").appendChild = () => {
    throw new Error("dom is gone too");
  };
  renderBootstrapFailure(new Error("original"), { ...target, document });

  const messages = target.console.calls.map(([message]) => message);
  assert.ok(
    messages.some((message) => /bootstrap failed/u.test(message)),
    "the original failure must still be reported",
  );
  assert.ok(
    messages.some((message) =>
      /could not paint the bootstrap failure surface/u.test(message),
    ),
    "the secondary failure must be reported too",
  );
});

// ---------------------------------------------------------------------------
// The failure surface cannot depend on the thing that failed
// ---------------------------------------------------------------------------

test("mainBootstrap_module_has_no_imports_of_any_kind", () => {
  // The thing that failed is the app's ability to load code. A failure surface
  // that needs another module can fail the same way the app just did.
  const source = fs.readFileSync(
    path.join(__dirname, "mainBootstrap.ts"),
    "utf8",
  );
  const code = source
    .replace(/\/\*[\s\S]*?\*\//gu, "")
    .replace(/^\s*\/\/.*$/gmu, "");

  assert.equal(
    /^\s*import\s/mu.test(code),
    false,
    "mainBootstrap.ts must not statically import anything",
  );
  assert.equal(
    /\bimport\s*\(/u.test(code),
    false,
    "mainBootstrap.ts must not dynamically import anything",
  );
  assert.equal(
    /\brequire\s*\(/u.test(code),
    false,
    "mainBootstrap.ts must not require anything",
  );
});

// ---------------------------------------------------------------------------
// vite:preloadError names the chunk
// ---------------------------------------------------------------------------

test("preload_error_is_logged_with_the_startup_prefix", () => {
  handleVitePreloadError(
    {
      payload: new Error(
        "Failed to fetch dynamically imported module: http://localhost/assets/e2eBridge-a1b2.js",
      ),
    },
    target,
  );

  assert.match(
    target.console.calls.at(0)[0],
    /^\[STARTUP\] vite:preloadError/u,
  );
});

test("preload_error_chunk_url_reaches_the_bootstrap_failure_surface", () => {
  handleVitePreloadError(
    {
      payload: new Error(
        "Failed to fetch dynamically imported module: http://localhost/assets/e2eBridge-a1b2.js",
      ),
    },
    target,
  );
  renderBootstrapFailure(new Error("bootstrap gave up"), target);

  const detail = target.document.querySelector(
    '[data-testid="bootstrap-failure-detail"]',
  ).textContent;
  assert.match(detail, /bootstrap gave up/u);
  assert.match(detail, /e2eBridge-a1b2\.js/u);
});

test("extractPreloadChunkUrl_pulls_the_url_out_of_the_message", () => {
  assert.equal(
    extractPreloadChunkUrl(
      "Failed to fetch dynamically imported module: https://cdn.example/assets/x-9f.js",
    ),
    "https://cdn.example/assets/x-9f.js",
  );
  assert.equal(extractPreloadChunkUrl("no url here"), null);
});

test("composeBootstrapFailureDetail_omits_the_chunk_line_when_none_was_seen", () => {
  assert.equal(composeBootstrapFailureDetail(new Error("plain")), "plain");
});

test("describeBootstrapError_handles_non_error_throws", () => {
  assert.equal(describeBootstrapError("just a string"), "just a string");
  assert.equal(describeBootstrapError(new Error("")), "Error");
  const circular = {};
  circular.self = circular;
  assert.equal(typeof describeBootstrapError(circular), "string");
});

// ---------------------------------------------------------------------------
// Decoupling: an optional step cannot stop the render
// ---------------------------------------------------------------------------

test("a_rejecting_optional_step_does_not_reject_the_caller", async () => {
  await runOptionalBootstrapStep(
    "e2e bridge",
    () => Promise.reject(new Error("chunk never arrived")),
    target,
  );
});

test("work_after_a_rejecting_optional_step_still_runs", async () => {
  const ran = [];
  await runOptionalBootstrapStep(
    "e2e bridge",
    () => Promise.reject(new Error("chunk never arrived")),
    target,
  );
  ran.push("renderApp");

  assert.deepEqual(ran, ["renderApp"]);
});

test("a_rejecting_optional_step_is_reported_with_its_label_and_error", async () => {
  const error = new Error("chunk never arrived");
  await runOptionalBootstrapStep(
    "e2e bridge",
    () => Promise.reject(error),
    target,
  );

  const logged = target.console.calls.at(0);
  assert.match(
    logged[0],
    /^\[STARTUP\] optional bootstrap step "e2e bridge" failed/u,
  );
  assert.match(logged[0], /chunk never arrived/u);
  assert.equal(logged[1], error, "the failure must not be swallowed");
});

test("a_synchronously_throwing_optional_step_is_also_contained", async () => {
  await runOptionalBootstrapStep(
    "e2e bridge",
    () => {
      throw new Error("threw before returning a promise");
    },
    target,
  );
  assert.equal(target.console.calls.length, 1);
});

test("a_succeeding_optional_step_logs_nothing", async () => {
  await runOptionalBootstrapStep("e2e bridge", () => Promise.resolve(), target);
  assert.equal(target.console.calls.length, 0);
});

// ---------------------------------------------------------------------------
// Structural guardrails on main.tsx itself
// ---------------------------------------------------------------------------

function readMainSource() {
  return fs.readFileSync(path.join(__dirname, "main.tsx"), "utf8");
}

test("main_bootstrap_rejection_is_caught", () => {
  // `void bootstrap();` with no handler is the defect. Restoring it fails here.
  const source = readMainSource();
  assert.match(
    source,
    /bootstrap\(\)\s*\.catch\(/u,
    "main.tsx must attach a rejection handler to bootstrap()",
  );
  assert.equal(
    /void\s+bootstrap\(\)\s*;/u.test(source),
    false,
    "main.tsx must not fire bootstrap() with no rejection handler",
  );
});

test("main_bootstrap_rejection_renders_the_failure_surface", () => {
  assert.match(
    readMainSource(),
    /\.catch\([\s\S]{0,120}renderBootstrapFailure\(/u,
    "the bootstrap catch must paint the failure surface, not just log",
  );
});

test("main_registers_a_vite_preloadError_listener", () => {
  assert.match(
    readMainSource(),
    /addEventListener\(\s*"vite:preloadError"/u,
    "main.tsx must listen for vite:preloadError",
  );
});

test("main_does_not_await_the_e2e_bridge_import_directly", () => {
  // renderApp() must not sit behind the optional chunk. The bridge install has
  // to go through runOptionalBootstrapStep.
  const source = readMainSource();
  assert.match(
    source,
    /runOptionalBootstrapStep\(\s*"e2e bridge",\s*installE2eBridgeIfConfigured\s*\)/u,
    "the e2e bridge install must be isolated by runOptionalBootstrapStep",
  );
  assert.equal(
    /await\s+installE2eBridgeIfConfigured\(\)/u.test(source),
    false,
    "awaiting the bridge install directly re-couples renderApp() to it",
  );
});
