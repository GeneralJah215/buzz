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
 * An age for a bucket, or `null` when the sidecar reported none.
 *
 * `null` is a real answer here rather than a gap to paper over: it means the
 * bucket is empty, and a section only renders when its own count is above zero,
 * so it should not happen. If it ever does — a sidecar sending a count without
 * its age — the row drops the age and keeps the count, which is the half the
 * operator acts on. Substituting `oldestPendingAt` is precisely what this stopped
 * doing: that figure spans all three buckets and can be an age no row in THIS
 * one has (BUG-023).
 */
function bucketAge(oldestAt: number | null, nowMs: number): string | null {
  return oldestAt === null ? null : formatQueuedAge(oldestAt, nowMs);
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
            {waiting.map((author) => {
              // The age of the oldest CLAIMABLE row, not the author's oldest
              // row of any kind. Those differ whenever the author also has a
              // digest-carried or ancestor-blocked row that is older, and the
              // aggregate overstated the wait by exactly that gap.
              const age = bucketAge(author.oldestClaimableAt, now);
              return (
                <li
                  className="flex flex-wrap items-center gap-x-2 gap-y-1 text-xs text-muted-foreground"
                  key={author.author}
                >
                  <PubKey className="text-xs" pubkey={author.author} />
                  <span>{eventCount(author.pending, "event")} queued</span>
                  {age === null ? null : (
                    <>
                      <span aria-hidden="true">·</span>
                      <span>oldest waiting {age}</span>
                    </>
                  )}
                </li>
              );
            })}
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
            {blocked.map((author) => {
              // The sidecar now sends this bucket's own oldest arrival time, so
              // the label can be precise again. It used to be one timestamp per
              // author across all three buckets, which meant the figure beside
              // "blocked" could belong to a genuinely claimable row -- in the
              // section whose entire point is that these events are NOT waiting
              // for anybody. The copy was softened to "oldest queued event from
              // this author" because that was all the data supported (BUG-023).
              const age = bucketAge(author.oldestAncestorBlockedAt, now);
              return (
                <li
                  className="flex flex-wrap items-center gap-x-2 gap-y-1 text-xs text-muted-foreground"
                  key={author.author}
                >
                  <PubKey className="text-xs" pubkey={author.author} />
                  <span>
                    {eventCount(author.ancestorBlocked, "event")} blocked
                  </span>
                  {age === null ? null : (
                    <>
                      <span aria-hidden="true">·</span>
                      <span>oldest blocked {age}</span>
                    </>
                  )}
                </li>
              );
            })}
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
