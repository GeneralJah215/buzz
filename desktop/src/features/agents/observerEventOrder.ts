import type { ObserverEvent } from "./ui/agentSessionTypes";

// `Date.parse` on an ISO string is not free, and comparison is the innermost
// loop of every journal sort and every watermark check. Events are immutable
// once decoded, so their parsed time is memoized against the object itself; the
// WeakMap drops the entry when the event is evicted (BUG-065 rank 3).
const parsedTimestampByEvent = new WeakMap<object, number>();

function observerEventTimeMs(event: ObserverEvent): number {
  const cached = parsedTimestampByEvent.get(event);
  if (cached !== undefined) return cached;
  const parsed = Date.parse(event.timestamp);
  parsedTimestampByEvent.set(event, parsed);
  return parsed;
}

/**
 * Test-only counter of `compareObserverEvents` invocations. The archive-window
 * regression asserts on this rather than on elapsed time: a timing threshold
 * can be widened until it passes, a call count cannot (BUG-066).
 */
let comparisonCount = 0;

export function _testGetObserverComparisonCount(): number {
  return comparisonCount;
}

export function _testResetObserverComparisonCount(): void {
  comparisonCount = 0;
}

export function compareObserverEvents(
  left: ObserverEvent,
  right: ObserverEvent,
) {
  comparisonCount += 1;
  const leftTime = observerEventTimeMs(left);
  const rightTime = observerEventTimeMs(right);
  if (Number.isFinite(leftTime) && Number.isFinite(rightTime)) {
    const timeDiff = leftTime - rightTime;
    if (timeDiff !== 0) {
      return timeDiff;
    }
  }

  return left.seq - right.seq;
}

/**
 * Returns true if `candidate` sorts strictly after `stored` using the same
 * two-key ordering as `compareObserverEvents`: later timestamp wins; equal
 * timestamp falls back to higher seq.  Extracted so latest-live advancement
 * cannot drift from transcript ordering.
 */
export function isObserverEventAfter(
  candidate: { timestamp: string; seq: number },
  stored: { timestamp: string; seq: number },
): boolean {
  const candidateTime = Date.parse(candidate.timestamp);
  const storedTime = Date.parse(stored.timestamp);
  if (Number.isFinite(candidateTime) && Number.isFinite(storedTime)) {
    if (candidateTime !== storedTime) {
      return candidateTime > storedTime;
    }
  }
  return candidate.seq > stored.seq;
}
