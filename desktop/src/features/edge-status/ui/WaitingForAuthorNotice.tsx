import type { EdgeWaitingAuthor } from "@/features/edge-status/api/edgeStatus";
import { formatQueuedAge } from "@/features/edge-status/lib/deliveryState";
import { cn } from "@/shared/lib/cn";
import { useNow } from "@/shared/lib/useNow";
import { PubKey } from "@/shared/ui/PubKey";

/** The age readout only needs minute resolution. */
const WAITING_AGE_TICK_MS = 30_000;

export type WaitingForAuthorNoticeProps = {
  authors: readonly EdgeWaitingAuthor[];
  className?: string;
};

function eventCount(count: number, noun: string): string {
  return count === 1 ? `1 ${noun}` : `${count} ${noun}s`;
}

/**
 * "Waiting for author" indicator — and, deliberately, the two things that look
 * like waiting and are not.
 *
 * Phase 1 has no privileged upstream replay: each authoring identity has to
 * drain and republish its OWN queued events. So when an agent identity goes
 * offline with events still queued, nothing on this machine can push them —
 * they sit until that identity comes back. That is the amber section.
 *
 * The other two counts are not that, and telling the operator to wait for an
 * author would be wrong and unactionable in both cases:
 *
 *   - `ancestorBlocked` rows belong to an author who is present and draining
 *     correctly. `claim_outbox_batch` refuses them because an ancestor of
 *     theirs never reached canonical history, so the drain client runs every
 *     cycle and correctly claims nothing, forever. The actionable thing is the
 *     ancestor, so these get their own section with their own instruction.
 *   - `pendingViaDigest` rows have no author coming for them at all — this
 *     machine's edge identity carries them upstream in a catch-up digest. They
 *     are reported as a footnote and are never added into a waiting figure.
 */
export function WaitingForAuthorNotice({
  authors,
  className,
}: WaitingForAuthorNoticeProps) {
  const now = useNow(WAITING_AGE_TICK_MS);

  const waiting = authors.filter((author) => author.pending > 0);
  const blocked = authors.filter((author) => author.ancestorBlocked > 0);
  const carriedByDigest = authors.reduce(
    (total, author) => total + author.pendingViaDigest,
    0,
  );

  if (waiting.length === 0 && blocked.length === 0 && carriedByDigest === 0) {
    return null;
  }

  return (
    <div className={cn("flex flex-col gap-2", className)}>
      {waiting.length > 0 ? (
        <section
          aria-label="Identities with events waiting to sync"
          className="rounded-lg border border-amber-500/40 bg-amber-500/10 p-3"
        >
          <p className="text-xs font-medium text-amber-600 dark:text-amber-400">
            Waiting for an author to come online
          </p>
          <p className="mt-1 text-xs text-muted-foreground">
            These identities have events delivered locally but not yet in
            canonical history. Only the author can republish them.
          </p>
          <ul className="mt-2 flex flex-col gap-1">
            {waiting.map((author) => (
              <li
                className="flex flex-wrap items-center gap-x-2 gap-y-1 text-xs text-muted-foreground"
                key={author.author}
              >
                <PubKey className="text-xs" pubkey={author.author} />
                <span>{eventCount(author.pending, "event")} queued</span>
                <span aria-hidden="true">·</span>
                <span>
                  oldest waiting {formatQueuedAge(author.oldestPendingAt, now)}
                </span>
              </li>
            ))}
          </ul>
        </section>
      ) : null}

      {blocked.length > 0 ? (
        <section
          aria-label="Events blocked behind an ancestor that never reached canonical history"
          className="rounded-lg border border-destructive/40 bg-destructive/10 p-3"
        >
          <p className="text-xs font-medium text-destructive">
            Blocked behind an earlier event that never synced
          </p>
          <p className="mt-1 text-xs text-muted-foreground">
            Waiting for these authors will not help — their drain client is
            already running and correctly claiming nothing. Each of these events
            replies to an earlier one that never reached canonical history.
            Retry or discard that earlier event to release them.
          </p>
          <ul className="mt-2 flex flex-col gap-1">
            {blocked.map((author) => (
              <li
                className="flex flex-wrap items-center gap-x-2 gap-y-1 text-xs text-muted-foreground"
                key={author.author}
              >
                <PubKey className="text-xs" pubkey={author.author} />
                <span>
                  {eventCount(author.ancestorBlocked, "event")} blocked
                </span>
                <span aria-hidden="true">·</span>
                {/*
                 * NOT "oldest waiting". The sidecar sends one timestamp per
                 * author across all three buckets (BUG-023), so this row's
                 * figure may come from a genuinely pending event rather than a
                 * blocked one -- and this is the section whose entire point is
                 * that these events are not waiting for anybody. The data
                 * cannot be narrowed here, so the label claims only what the
                 * number actually is.
                 */}
                <span>
                  oldest queued event from this author{" "}
                  {formatQueuedAge(author.oldestPendingAt, now)}
                </span>
              </li>
            ))}
          </ul>
        </section>
      ) : null}

      {carriedByDigest > 0 ? (
        <p className="text-xs text-muted-foreground">
          {eventCount(carriedByDigest, "more event")} will be carried to
          canonical history by this machine inside a catch-up digest. No author
          is needed, so {carriedByDigest === 1 ? "it is" : "they are"} not
          counted above.
        </p>
      ) : null}
    </div>
  );
}
