import * as React from "react";

import {
  edgeRequeueOutcomeKey,
  requeueQuarantinedEvent,
  type EdgeQuarantinedEvent,
  type EdgeRequeueResult,
} from "@/features/edge-status/api/edgeStatus";
import { cn } from "@/shared/lib/cn";
import { truncatePubkey } from "@/shared/lib/pubkey";
import { Badge } from "@/shared/ui/badge";
import { Button } from "@/shared/ui/button";
import { PubKey } from "@/shared/ui/PubKey";

export type QuarantineListProps = {
  events: readonly EdgeQuarantinedEvent[];
  /**
   * Overridable for tests and for callers that want to refetch afterwards.
   * Resolves with the sidecar's outcome, which is never a bare boolean: see
   * `EdgeRequeueResult`.
   */
  onRetry?: (eventId: string) => Promise<EdgeRequeueResult>;
  className?: string;
};

/**
 * Shown after a requeue the sidecar accepted, on a row that is still on screen
 * because the list only changes when its owner refetches. Without it the row
 * looks untouched, the operator clicks Retry again, the sidecar answers "not
 * quarantined any more" — and a working retry reads on screen as a failure.
 */
const RETRY_QUEUED_MESSAGE =
  "Sent back to the sync queue. This row clears on the next refresh.";

/**
 * `notFound`. The row is not retryable BY THIS OPERATOR: it left quarantine
 * already, or it belongs to another identity and only its author may move it.
 * Either way a refresh is the next step, not another click.
 */
const RETRY_NOT_FOUND_MESSAGE =
  "The sidecar has no quarantined event with this id for you — it either left quarantine already, or it belongs to another identity. Refresh to see where it went.";

/**
 * `carriedByDigest`. The sidecar refuses this retry on purpose: the edge
 * identity is already carrying the event upstream inside a catch-up digest, so
 * no drain will ever claim it. Before the outcome existed this looked exactly
 * like `notFound`, and the advice for the two is opposite — here there is
 * nothing wrong and nothing to do.
 */
const RETRY_CARRIED_BY_DIGEST_MESSAGE =
  "This machine is already carrying the event upstream inside a catch-up digest, so there is nothing for its author to retry.";

/** A newer sidecar's outcome. Say so plainly rather than guessing at it. */
function unknownOutcomeMessage(outcome: string): string {
  return `The sidecar declined the retry with an outcome this version does not recognise ('${outcome}'). The event has not moved.`;
}

/** Copy for a row the edge is carrying, shown where its Retry button was. */
const CARRIED_BY_DIGEST_NOTE =
  "Carried upstream by this machine in a catch-up digest. Its author cannot retry it.";

type RetryNotice = {
  tone: "success" | "declined";
  message: string;
};

/**
 * Events the author-drain gave up on: still readable locally, still missing
 * from canonical history.
 *
 * A row flagged `carriedByDigest` is a different animal and is drawn as one.
 * The edge identity is already carrying it upstream, the sidecar refuses a
 * requeue on it, and the old list offered it the same Retry button as every
 * other row — a button whose only possible effect was to tell the operator the
 * retry was declined. Such a row gets no retry control at all, plus the reason
 * it was demoted, which is the only part the operator can act on.
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
  const [notices, setNotices] = React.useState<
    Readonly<Record<string, RetryNotice>>
  >({});
  // Ids whose retry must not be offered again: the requeue worked, or the
  // sidecar told us this row can never be requeued.
  const [closedIds, setClosedIds] = React.useState<ReadonlySet<string>>(
    () => new Set<string>(),
  );
  // Rows the sidecar revealed to be on the digest path after the list was
  // fetched. Treated exactly like the flag arriving on the row itself.
  const [carriedIds, setCarriedIds] = React.useState<ReadonlySet<string>>(
    () => new Set<string>(),
  );

  const handleRetry = React.useCallback(
    async (eventId: string) => {
      setRetryingIds((current) => new Set(current).add(eventId));
      setNotices((current) => {
        if (!(eventId in current)) {
          return current;
        }
        const next = { ...current };
        delete next[eventId];
        return next;
      });

      function note(notice: RetryNotice) {
        setNotices((current) => ({ ...current, [eventId]: notice }));
      }

      try {
        const result = await onRetry(eventId);
        // Branch on the outcome, not on the boolean. `notFound` and
        // `carriedByDigest` are the same `false` and opposite advice: one says
        // "look again", the other says "nothing is wrong, stop clicking".
        switch (edgeRequeueOutcomeKey(result.outcome)) {
          case "requeued":
            // Say so, and stop offering a second click: the row stays on
            // screen until its owner refetches, and a second requeue of an
            // event that already left quarantine is declined — a success that
            // would read as a failure.
            setClosedIds((current) => new Set(current).add(eventId));
            note({ tone: "success", message: RETRY_QUEUED_MESSAGE });
            break;
          case "carriedByDigest":
            setCarriedIds((current) => new Set(current).add(eventId));
            setClosedIds((current) => new Set(current).add(eventId));
            note({
              tone: "declined",
              message: RETRY_CARRIED_BY_DIGEST_MESSAGE,
            });
            break;
          case "notFound":
            note({ tone: "declined", message: RETRY_NOT_FOUND_MESSAGE });
            break;
          default:
            note({
              tone: "declined",
              message: unknownOutcomeMessage(result.outcome),
            });
            break;
        }
      } catch (error) {
        note({
          tone: "declined",
          message:
            error instanceof Error
              ? `Retry failed: ${error.message}`
              : "Retry failed for an unknown reason.",
        });
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
        const notice = notices[event.eventId];
        const isClosed = closedIds.has(event.eventId);
        const isCarried =
          event.carriedByDigest || carriedIds.has(event.eventId);
        let retryButtonLabel = "Retry";
        if (isClosed) {
          retryButtonLabel = "Queued";
        } else if (isRetrying) {
          retryButtonLabel = "Retrying…";
        }

        return (
          <li
            className={cn(
              "rounded-lg border p-3",
              isCarried
                ? "border-amber-500/50 bg-amber-500/10"
                : "border-border/70 bg-background/70",
            )}
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
                  {isCarried ? (
                    <Badge variant="warning">Carried by digest</Badge>
                  ) : null}
                </div>
                <p className="break-words text-xs text-foreground">
                  {event.reason}
                </p>
                {event.demotionReason ? (
                  <p className="break-words text-xs text-muted-foreground">
                    Left the direct path: {event.demotionReason}
                  </p>
                ) : null}
                {isCarried ? (
                  <p className="text-xs text-muted-foreground">
                    {CARRIED_BY_DIGEST_NOTE}
                  </p>
                ) : null}
                {notice ? (
                  <p
                    className={cn(
                      "text-xs font-medium",
                      notice.tone === "success"
                        ? "text-foreground"
                        : "text-destructive",
                    )}
                    role={notice.tone === "success" ? "status" : "alert"}
                  >
                    {notice.message}
                  </p>
                ) : null}
              </div>
              {/*
                No retry control on a digest-carried row. A disabled button
                still reads as "this action exists and might come back"; here
                it never can, and the operator's next move is elsewhere.
              */}
              {isCarried ? null : (
                <Button
                  aria-label={`Retry sync for event ${truncatePubkey(event.eventId)}`}
                  disabled={isRetrying || isClosed}
                  onClick={() => {
                    void handleRetry(event.eventId);
                  }}
                  size="sm"
                  type="button"
                  variant="outline"
                >
                  {retryButtonLabel}
                </Button>
              )}
            </div>
          </li>
        );
      })}
    </ul>
  );
}
