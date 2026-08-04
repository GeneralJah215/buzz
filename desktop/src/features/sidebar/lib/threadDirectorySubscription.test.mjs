import assert from "node:assert/strict";
import test from "node:test";

import { createRetryingThreadDirectorySubscription } from "./threadDirectorySubscription.ts";

const flushPromises = () => new Promise((resolve) => setImmediate(resolve));

test("failed directory subscriptions retry and do not duplicate an active subscription", async () => {
  let subscribeCalls = 0;
  let disposeCalls = 0;
  let scheduled = null;
  const cancelled = [];
  const errors = [];
  const control = createRetryingThreadDirectorySubscription({
    subscribe() {
      subscribeCalls += 1;
      if (subscribeCalls === 1) return Promise.reject(new Error("offline"));
      return Promise.resolve(async () => {
        disposeCalls += 1;
      });
    },
    onError(error) {
      errors.push(error);
    },
    scheduleRetry(callback, delayMs) {
      scheduled = callback;
      assert.equal(delayMs, 1_000);
      return 7;
    },
    cancelRetry(timer) {
      cancelled.push(timer);
    },
  });

  await flushPromises();
  assert.equal(subscribeCalls, 1);
  assert.equal(errors.length, 1);
  assert.equal(typeof scheduled, "function");

  scheduled();
  await flushPromises();
  assert.equal(subscribeCalls, 2);

  control.reconnect();
  await flushPromises();
  assert.equal(subscribeCalls, 2);
  assert.deepEqual(cancelled, []);

  control.dispose();
  await flushPromises();
  assert.equal(disposeCalls, 1);
});

test("reconnect replaces a pending retry immediately", async () => {
  let subscribeCalls = 0;
  const cancelled = [];
  const control = createRetryingThreadDirectorySubscription({
    subscribe() {
      subscribeCalls += 1;
      return subscribeCalls === 1
        ? Promise.reject(new Error("offline"))
        : Promise.resolve(async () => {});
    },
    onError() {},
    scheduleRetry() {
      return 11;
    },
    cancelRetry(timer) {
      cancelled.push(timer);
    },
  });

  await flushPromises();
  control.reconnect();
  await flushPromises();
  assert.equal(subscribeCalls, 2);
  assert.deepEqual(cancelled, [11]);
  control.dispose();
});
