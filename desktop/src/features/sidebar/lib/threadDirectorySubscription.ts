export type ThreadDirectorySubscriptionControl = {
  reconnect(): void;
  dispose(): void;
};

type RetryTimer = ReturnType<typeof setTimeout>;

export function createRetryingThreadDirectorySubscription({
  subscribe,
  onError,
  scheduleRetry = (callback, delayMs) => setTimeout(callback, delayMs),
  cancelRetry = (timer) => clearTimeout(timer),
}: {
  subscribe: () => Promise<() => Promise<void>>;
  onError: (error: unknown) => void;
  scheduleRetry?: (callback: () => void, delayMs: number) => RetryTimer;
  cancelRetry?: (timer: RetryTimer) => void;
}): ThreadDirectorySubscriptionControl {
  let disposed = false;
  let subscribing = false;
  let unsubscribe: (() => Promise<void>) | null = null;
  let retryDelayMs = 1_000;
  let retryTimer: RetryTimer | null = null;
  let restartRequested = false;

  const start = () => {
    if (disposed || subscribing || unsubscribe) return;
    subscribing = true;
    void subscribe()
      .then((disposeSubscription) => {
        retryDelayMs = 1_000;
        if (disposed) {
          void disposeSubscription();
        } else {
          unsubscribe = disposeSubscription;
        }
      })
      .catch((error) => {
        onError(error);
        if (!disposed) {
          retryTimer = scheduleRetry(() => {
            retryTimer = null;
            start();
          }, retryDelayMs);
          retryDelayMs = Math.min(retryDelayMs * 2, 30_000);
        }
      })
      .finally(() => {
        subscribing = false;
        if (restartRequested) {
          restartRequested = false;
          if (retryTimer) {
            cancelRetry(retryTimer);
            retryTimer = null;
          }
          start();
        }
      });
  };

  start();
  return {
    reconnect() {
      if (retryTimer) {
        cancelRetry(retryTimer);
        retryTimer = null;
      }
      if (subscribing) {
        restartRequested = true;
        return;
      }
      start();
    },
    dispose() {
      disposed = true;
      if (retryTimer) cancelRetry(retryTimer);
      if (unsubscribe) void unsubscribe();
    },
  };
}
