/**
 * Boot-time failure surface and optional-step isolation for `main.tsx`.
 *
 * BUG-054: `main.tsx` ran `void bootstrap()` with no rejection handler and no
 * `vite:preloadError` listener. A single failed dynamic import — in practice
 * the ~160 KB `@/testing/e2eBridge` chunk, which has no `modulepreload` and is
 * routinely still in flight — therefore stopped the entire boot. The bridge
 * never installed AND `renderApp()` never ran, so the window stayed blank with
 * nothing in the console naming the cause. That is how BUG-037's
 * `relay-reconnect` failure presented as a confusing assertion error rather
 * than "a chunk did not load".
 *
 * Two rules shape this module:
 *
 * 1. **The failure surface must not need the thing that failed.** What broke is
 *    the app's ability to load code, so nothing here may import React, the
 *    design system, a CSS module, or any other module — the whole file is
 *    dependency-free DOM built with `document.createElement`, `textContent`,
 *    and inline styles. `mainBootstrap.test.mjs` asserts that this file has
 *    zero imports, static or dynamic, so the property cannot rot.
 * 2. **Optional steps may not gate required ones.** `runOptionalBootstrapStep`
 *    reports a failing optional step and returns, so the caller reaches
 *    `renderApp()` regardless. The error is logged with its stack — it is
 *    reported and continued past by design, never silently dropped.
 */

/** Log prefix for every boot-path diagnostic, per the project log conventions. */
const STARTUP_LOG_PREFIX = "[STARTUP]";

/**
 * The most recent `vite:preloadError`.
 *
 * The rejection a failed dynamic import produces does not reliably name the
 * chunk that could not be fetched; the `vite:preloadError` event does, and it
 * is the only signal that does. Remembering it here lets the failure surface
 * report the chunk URL alongside the rejection that followed from it.
 */
let lastPreloadFailure: { message: string; url: string | null } | null = null;

type ConsoleLike = Pick<Console, "error">;

type BootstrapFailureTarget = {
  console?: ConsoleLike;
  document?: Document;
};

function resolveConsole(
  target: BootstrapFailureTarget | undefined,
): ConsoleLike {
  return target?.console ?? console;
}

function resolveDocument(
  target: BootstrapFailureTarget | undefined,
): Document | null {
  const candidate =
    target?.document ??
    (typeof document === "undefined" ? null : (document as Document));
  return candidate ?? null;
}

/** Human-readable one-liner for an unknown thrown value, stack excluded. */
export function describeBootstrapError(error: unknown): string {
  if (error instanceof Error) {
    return error.message || error.name || "Error";
  }
  if (typeof error === "string" && error.length > 0) return error;
  try {
    return JSON.stringify(error) ?? String(error);
  } catch {
    // A value whose serialisation throws (circular, hostile `toJSON`) must not
    // take the failure surface down with it — describing it is best effort.
    return String(error);
  }
}

/**
 * Extract the chunk URL from a `vite:preloadError` event.
 *
 * Vite dispatches the event with the original error on `payload`. The URL is
 * carried in that error's message (`Failed to fetch dynamically imported
 * module: <url>`), so pull the first absolute URL out of it.
 */
export function extractPreloadChunkUrl(message: string): string | null {
  return (
    /\bhttps?:\/\/\S+/u.exec(message)?.[0]?.replace(/[)\].,]+$/u, "") ?? null
  );
}

/**
 * Record and log a `vite:preloadError`.
 *
 * Exported so `main.tsx` can register it and the tests can drive it directly.
 * It only records and logs — it deliberately does NOT paint the failure
 * surface, because a lazily loaded route chunk can fail long after the app has
 * rendered and blanking a working app would be worse than the bug.
 */
export function handleVitePreloadError(
  event: { payload?: unknown },
  target?: BootstrapFailureTarget,
): void {
  const payload = event.payload;
  const message = describeBootstrapError(payload);
  lastPreloadFailure = { message, url: extractPreloadChunkUrl(message) };
  resolveConsole(target).error(
    `${STARTUP_LOG_PREFIX} vite:preloadError — a JavaScript chunk failed to load: ${
      lastPreloadFailure.url ?? message
    }`,
    payload,
  );
}

/** Test seam: forget any remembered preload failure. */
export function resetPreloadFailureForTests(): void {
  lastPreloadFailure = null;
}

/**
 * Run a bootstrap step whose failure must not stop the app from rendering.
 *
 * The e2e bridge is test-only scaffolding. Awaiting it inline meant its chunk
 * failing to load also skipped `renderApp()`; this decouples the two. The
 * rejection is logged with its stack and named by `label` — an e2e run without
 * the bridge still fails loudly on its own assertions, and a real user never
 * needed it.
 */
export async function runOptionalBootstrapStep(
  label: string,
  step: () => Promise<void> | void,
  target?: BootstrapFailureTarget,
): Promise<void> {
  try {
    await step();
  } catch (error) {
    resolveConsole(target).error(
      `${STARTUP_LOG_PREFIX} optional bootstrap step "${label}" failed; continuing to render: ${describeBootstrapError(
        error,
      )}`,
      error,
    );
  }
}

function styleElement(element: HTMLElement, style: string): void {
  element.setAttribute("style", style);
}

function buildFailureSurface(
  ownerDocument: Document,
  detail: string,
): HTMLElement {
  const container = ownerDocument.createElement("div");
  container.setAttribute("data-testid", "bootstrap-failure");
  container.setAttribute("role", "alert");
  styleElement(
    container,
    [
      "box-sizing:border-box",
      "display:flex",
      "flex-direction:column",
      "gap:12px",
      "align-items:flex-start",
      "justify-content:center",
      "min-height:100vh",
      "padding:32px",
      "font-family:system-ui,-apple-system,'Segoe UI',sans-serif",
      "font-size:0.875rem",
      "line-height:1.5",
      "color:#f5f5f5",
      "background:#1b1b1f",
    ].join(";"),
  );

  const heading = ownerDocument.createElement("h1");
  heading.textContent = "Buzz couldn’t finish starting";
  styleElement(heading, "margin:0;font-size:1.25rem;font-weight:600");
  container.appendChild(heading);

  const explanation = ownerDocument.createElement("p");
  explanation.textContent =
    "Part of the app failed to load, so nothing could be drawn. Reopening Buzz usually fixes it.";
  styleElement(explanation, "margin:0;max-width:60ch;opacity:0.85");
  container.appendChild(explanation);

  const details = ownerDocument.createElement("pre");
  details.setAttribute("data-testid", "bootstrap-failure-detail");
  details.textContent = detail;
  styleElement(
    details,
    [
      "margin:0",
      "max-width:100%",
      "overflow-x:auto",
      "white-space:pre-wrap",
      "word-break:break-word",
      "padding:12px",
      "border-radius:8px",
      "background:#000000",
      "color:#ff9d9d",
      "font-family:ui-monospace,'JetBrains Mono',monospace",
      "font-size:0.75rem",
    ].join(";"),
  );
  container.appendChild(details);

  return container;
}

/**
 * Compose the technical detail line: the rejection, plus the named chunk from
 * `vite:preloadError` when one was seen.
 */
export function composeBootstrapFailureDetail(error: unknown): string {
  const lines = [describeBootstrapError(error)];
  if (lastPreloadFailure) {
    lines.push(
      `Failed chunk: ${lastPreloadFailure.url ?? lastPreloadFailure.message}`,
    );
  }
  return lines.join("\n");
}

/**
 * Paint the boot failure into the DOM and log it.
 *
 * Rendered into `#root` when it exists, `document.body` otherwise. A
 * `console.error` alone helps a developer and not a user; something in the DOM
 * helps both, which is why this does the DOM write too — and why it does the
 * DOM write with nothing but built-ins.
 */
export function renderBootstrapFailure(
  error: unknown,
  target?: BootstrapFailureTarget,
): void {
  const logger = resolveConsole(target);
  const detail = composeBootstrapFailureDetail(error);
  logger.error(`${STARTUP_LOG_PREFIX} bootstrap failed: ${detail}`, error);

  const ownerDocument = resolveDocument(target);
  if (!ownerDocument) return;

  try {
    const host =
      ownerDocument.getElementById("root") ?? ownerDocument.body ?? null;
    if (!host) return;
    host.textContent = "";
    host.appendChild(buildFailureSurface(ownerDocument, detail));
  } catch (renderError) {
    // The DOM write is the last line of defence; if even it fails there is
    // nothing further to fall back to, so report it rather than let a second
    // exception replace the first one in the console.
    logger.error(
      `${STARTUP_LOG_PREFIX} could not paint the bootstrap failure surface`,
      renderError,
    );
  }
}
