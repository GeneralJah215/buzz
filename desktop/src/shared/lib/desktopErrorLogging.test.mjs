import assert from "node:assert/strict";
import test from "node:test";
import {
  FRONTEND_ERROR_MESSAGE_LIMIT,
  FRONTEND_ERROR_STACK_LIMIT,
  normalizeFrontendErrorValue,
  truncateFrontendErrorField,
} from "./desktopErrorLogging.ts";

test("normalizes Error rejections with bounded message and stack", () => {
  const error = new Error("boom");
  error.stack = "s".repeat(FRONTEND_ERROR_STACK_LIMIT + 20);

  const normalized = normalizeFrontendErrorValue(error);

  assert.equal(normalized.message, "boom");
  assert.equal(normalized.stack?.length, FRONTEND_ERROR_STACK_LIMIT);
});

test("normalizes arbitrary rejection values without throwing", () => {
  const circular = {};
  circular.self = circular;

  assert.equal(normalizeFrontendErrorValue("plain").message, "plain");
  assert.equal(normalizeFrontendErrorValue({ code: 7 }).message, '{"code":7}');
  assert.equal(
    normalizeFrontendErrorValue(circular).message,
    "[object Object]",
  );
});

test("truncates oversized frontend fields without splitting code points", () => {
  const value = "🐝".repeat(FRONTEND_ERROR_MESSAGE_LIMIT + 1);
  const truncated = truncateFrontendErrorField(
    value,
    FRONTEND_ERROR_MESSAGE_LIMIT,
  );

  assert.equal(Array.from(truncated).length, FRONTEND_ERROR_MESSAGE_LIMIT);
  assert.equal(Array.from(truncated).at(-1), "🐝");
});
