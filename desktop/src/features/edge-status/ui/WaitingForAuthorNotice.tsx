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

/**
 * "Waiting for author" indicator.
 *
 * Phase 1 has no privileged upstream replay: each authoring identity has to
 * drain and republish its OWN queued events. So when an agent identity goes
 * offline with events still queued, nothing on this machine can push them —
 * they sit until that identity comes back. This notice names the identities in
 * that state and how long their oldest event has waited, so the operator knows
 * the stall is a missing author rather than a broken relay.
 */
export function WaitingForAuthorNotice({
  authors,
  className,
}: WaitingForAuthorNoticeProps) {
  const now = useNow(WAITING_AGE_TICK_MS);

  if (authors.length === 0) {
    return null;
  }

  return (
    <section
      aria-label="Identities with events waiting to sync"
      className={cn(
        "rounded-lg border border-amber-500/40 bg-amber-500/10 p-3",
        className,
      )}
    >
      <p className="text-xs font-medium text-amber-600 dark:text-amber-400">
        Waiting for an author to come online
      </p>
      <p className="mt-1 text-xs text-muted-foreground">
        These identities have events delivered locally but not yet in canonical
        history. Only the author can republish them.
      </p>
      <ul className="mt-2 flex flex-col gap-1">
        {authors.map((author) => (
          <li
            className="flex flex-wrap items-center gap-x-2 gap-y-1 text-xs text-muted-foreground"
            key={author.author}
          >
            <PubKey className="text-xs" pubkey={author.author} />
            <span>
              {author.pending === 1
                ? "1 event queued"
                : `${author.pending} events queued`}
            </span>
            <span aria-hidden="true">·</span>
            <span>
              oldest waiting {formatQueuedAge(author.oldestPendingAt, now)}
            </span>
          </li>
        ))}
      </ul>
    </section>
  );
}
