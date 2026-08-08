import * as React from "react";

import {
  requeueQuarantinedEvent,
  type EdgeQuarantinedEvent,
} from "@/features/edge-status/api/edgeStatus";
import { cn } from "@/shared/lib/cn";
import { truncatePubkey } from "@/shared/lib/pubkey";
import { Button } from "@/shared/ui/button";
import { PubKey } from "@/shared/ui/PubKey";

export type QuarantineListProps = {
  events: readonly EdgeQuarantinedEvent[];
  /**
   * Overridable for tests and for callers that want to refetch afterwards.
   * Resolves `false` when the sidecar declined the requeue.
   */
  onRetry?: (eventId: string) => Promise<boolean>;
  className?: string;
};

const RETRY_DECLINED_MESSAGE =
  "The sidecar declined the retry — this event is no longer queued.";

/**
 * Events the author-drain gave up on: still readable locally, still missing
 * from canonical history.
 *
 * Every id is truncated through the canonical `truncatePubkey` /
 * `<PubKey>` helpers rather than a hand-rolled `slice` — see
 * `desktop/scripts/check-pubkey-truncation.mjs` for why the repo centralises
 * that (short hex prefixes are forgeable, and the display form fragmented into
 * five variants before the guard existed).
 */
export function QuarantineList({
  events,
  onRetry = requeueQuarantinedEvent,
  className,
}: QuarantineListProps) {
  const [retryingIds, setRetryingIds] = React.useState<ReadonlySet<string>>(
    () => new Set<string>(),
  );
  const [retryErrors, setRetryErrors] = React.useState<
    Readonly<Record<string, string>>
  >({});

  const handleRetry = React.useCallback(
    async (eventId: string) => {
      setRetryingIds((current) => new Set(current).add(eventId));
      setRetryErrors((current) => {
        if (!(eventId in current)) {
          return current;
        }
        const next = { ...current };
        delete next[eventId];
        return next;
      });

      try {
        const requeued = await onRetry(eventId);
        if (!requeued) {
          // A `false` return is a real outcome the operator must see; failing
          // silently here would look identical to a successful retry.
          setRetryErrors((current) => ({
            ...current,
            [eventId]: RETRY_DECLINED_MESSAGE,
          }));
        }
      } catch (error) {
        setRetryErrors((current) => ({
          ...current,
          [eventId]:
            error instanceof Error
              ? `Retry failed: ${error.message}`
              : "Retry failed for an unknown reason.",
        }));
      } finally {
        setRetryingIds((current) => {
          const next = new Set(current);
          next.delete(eventId);
          return next;
        });
      }
    },
    [onRetry],
  );

  if (events.length === 0) {
    return null;
  }

  return (
    <ul className={cn("flex flex-col gap-2", className)}>
      {events.map((event) => {
        const isRetrying = retryingIds.has(event.eventId);
        const retryError = retryErrors[event.eventId];

        return (
          <li
            className="rounded-lg border border-border/70 bg-background/70 p-3"
            key={event.eventId}
          >
            <div className="flex items-start justify-between gap-3">
              <div className="min-w-0 flex-1 space-y-1">
                <div className="flex flex-wrap items-center gap-x-2 gap-y-1 text-xs text-muted-foreground">
                  <span className="font-mono" title={event.eventId}>
                    {truncatePubkey(event.eventId)}
                  </span>
                  <span aria-hidden="true">·</span>
                  <span className="font-mono" title={event.channelId}>
                    {truncatePubkey(event.channelId)}
                  </span>
                  <span aria-hidden="true">·</span>
                  <PubKey className="text-xs" pubkey={event.author} />
                  <span aria-hidden="true">·</span>
                  <span>
                    {event.attempts === 1
                      ? "1 attempt"
                      : `${event.attempts} attempts`}
                  </span>
                </div>
                <p className="break-words text-xs text-foreground">
                  {event.reason}
                </p>
                {retryError ? (
                  <p
                    className="text-xs font-medium text-destructive"
                    role="alert"
                  >
                    {retryError}
                  </p>
                ) : null}
              </div>
              <Button
                aria-label={`Retry sync for event ${truncatePubkey(event.eventId)}`}
                disabled={isRetrying}
                onClick={() => {
                  void handleRetry(event.eventId);
                }}
                size="sm"
                type="button"
                variant="outline"
              >
                {isRetrying ? "Retrying…" : "Retry"}
              </Button>
            </div>
          </li>
        );
      })}
    </ul>
  );
}
