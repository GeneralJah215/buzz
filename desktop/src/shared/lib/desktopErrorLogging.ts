import { invoke, isTauri } from "@tauri-apps/api/core";

export const FRONTEND_ERROR_MESSAGE_LIMIT = 4_096;
export const FRONTEND_ERROR_STACK_LIMIT = 16_384;
export const FRONTEND_ERROR_SOURCE_LIMIT = 2_048;

type FrontendErrorReport = {
  kind: "window.error" | "unhandledrejection";
  message: string;
  stack?: string;
  source?: string;
  line?: number;
  column?: number;
};

let installed = false;

export function truncateFrontendErrorField(
  value: string,
  limit: number,
): string {
  return Array.from(value).slice(0, limit).join("");
}

export function normalizeFrontendErrorValue(value: unknown): {
  message: string;
  stack?: string;
} {
  if (value instanceof Error) {
    return {
      message: truncateFrontendErrorField(
        value.message || value.name,
        FRONTEND_ERROR_MESSAGE_LIMIT,
      ),
      stack: value.stack
        ? truncateFrontendErrorField(value.stack, FRONTEND_ERROR_STACK_LIMIT)
        : undefined,
    };
  }

  let message: string;
  if (typeof value === "string") {
    message = value;
  } else {
    try {
      message = JSON.stringify(value) ?? String(value);
    } catch {
      message = String(value);
    }
  }
  return {
    message: truncateFrontendErrorField(message, FRONTEND_ERROR_MESSAGE_LIMIT),
  };
}

function forwardFrontendError(report: FrontendErrorReport) {
  if (!isTauri()) {
    return;
  }
  void invoke("report_frontend_error", { report }).catch(() => {
    // Diagnostics are best-effort. Reporting failure must not cause another
    // unhandled rejection or make a crash loop noisier.
  });
}

export function installDesktopErrorLogging() {
  if (installed) {
    return;
  }
  installed = true;

  window.addEventListener("error", (event) => {
    const normalized = normalizeFrontendErrorValue(
      event.error ?? event.message,
    );
    forwardFrontendError({
      kind: "window.error",
      message: normalized.message,
      stack: normalized.stack,
      source: event.filename
        ? truncateFrontendErrorField(
            event.filename,
            FRONTEND_ERROR_SOURCE_LIMIT,
          )
        : undefined,
      line: event.lineno || undefined,
      column: event.colno || undefined,
    });
  });

  window.addEventListener("unhandledrejection", (event) => {
    const normalized = normalizeFrontendErrorValue(event.reason);
    forwardFrontendError({
      kind: "unhandledrejection",
      message: normalized.message,
      stack: normalized.stack,
    });
  });
}
