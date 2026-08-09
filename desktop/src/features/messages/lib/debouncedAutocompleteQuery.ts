/**
 * The one place the composer's autocomplete hooks are allowed to schedule a
 * trigger-query update, and the one place the "closing is never debounced"
 * invariant lives.
 *
 * ── The defect this exists to make unrepresentable (BUG-042, BUG-048) ────────
 * `useChannelLinks`, `useEmojiAutocomplete` and `useMentions` each ran their
 * trigger detection (`#name`, `:shortcode`, `@name`) behind a 120 ms debounce,
 * and each debounced the CLOSE as well as the OPEN. `MessageComposer` ORs the
 * three open flags into a single `isAutocompleteOpen` ref that gates Enter in
 * the editor, so for up to 120 ms after the trigger text was already gone from
 * the document, a plain Enter was handed to a suggestion list that should not
 * have been showing — inserting a channel chip / emoji / mention instead of
 * submitting. Typing a replacement reset the timer on every keystroke, so the
 * stale list did not merely close late; it never closed at all.
 *
 * ── The remedy ───────────────────────────────────────────────────────────────
 * Detection runs SYNCHRONOUSLY on every call, and "no query at the cursor"
 * closes immediately. Only the OPEN stays behind the debounce, which is the only
 * side with a cost worth deferring: it re-filters a candidate list and re-renders
 * a dropdown. Closing only ever removes a list, so there is nothing to defer.
 * The pending timer is replaced, never merely cancelled — see below.
 *
 * Centralising it here rather than repeating the branch in each hook is
 * deliberate. The invariant is not "these three hooks close synchronously", it
 * is "a debounced autocomplete query closes synchronously" — a fourth
 * autocomplete gets the property by calling this, instead of by remembering a
 * rule written in someone else's file.
 *
 * ── Why `detect` is a thunk, called twice, and the timer always runs ─────────
 * The timer re-runs detection instead of reusing the synchronous result, and it
 * is scheduled even when the synchronous result was `null`. Both are
 * load-bearing for the prefix hooks: `detectPrefixQuery`'s multi-word path
 * resolves a candidate against a list of known names (channel names, mention
 * display names) that is fetched asynchronously, and it returns `null` outright
 * while that list is empty. So between one keystroke and the timer firing,
 * `@First Last` can go from undetectable to detectable with no further input —
 * and the callers read the list through a ref for exactly this reason. Passing
 * a thunk keeps the late re-resolution structural: there is no already-computed
 * value here to "helpfully" reuse, and no early return to drop the re-check.
 *
 * The consequence is that `onClose` may run twice for one call (once now, once
 * from the timer). Every caller closes by setting its query state to `null`, so
 * the second is a no-op React bails out of.
 */

export type DebouncedAutocompleteQueryOptions<TQuery> = {
  /** Owned by the calling hook so it can also cancel on insert/clear/unmount. */
  debounceTimerRef: React.MutableRefObject<ReturnType<
    typeof setTimeout
  > | null>;
  /** How long to defer the OPEN. The close is never deferred. */
  delayMs: number;
  /**
   * Re-read the editor state and report the trigger query at the cursor, or
   * exactly `null` when there is none (not `undefined`). Must be pure and
   * cheap: it runs once per call and once more when the timer fires.
   */
  detect: () => TQuery | null;
  /**
   * Drop the suggestion list. Runs synchronously when `detect` finds nothing,
   * and again from the timer if it still finds nothing. Must be idempotent.
   */
  onClose: () => void;
  /** Show/refresh the suggestion list. Only ever runs from the timer. */
  onOpen: (query: TQuery) => void;
};

/**
 * Close immediately if the trigger text at the cursor is gone, then schedule
 * the debounced re-check that may open (or re-close) the list.
 */
export function updateDebouncedAutocompleteQuery<TQuery>({
  debounceTimerRef,
  delayMs,
  detect,
  onClose,
  onOpen,
}: DebouncedAutocompleteQueryOptions<TQuery>): void {
  if (debounceTimerRef.current !== null) {
    clearTimeout(debounceTimerRef.current);
    debounceTimerRef.current = null;
  }

  // The close, and only the close, happens now.
  if (detect() === null) {
    onClose();
  }

  // Scheduled either way — a synchronous close must NOT also cancel the
  // re-check. `detectPrefixQuery`'s multi-word path returns null outright while
  // the known-name list is still loading, so "no query here" can turn into "a
  // query after all" without the user touching the keyboard again. Returning
  // early after `onClose` would delete that, and typing `@First Last` just as
  // the member list lands would leave the dropdown shut until the next
  // keystroke. Re-closing from the timer is a no-op for every caller (they set
  // their query state to null, which React bails out of).
  debounceTimerRef.current = setTimeout(() => {
    debounceTimerRef.current = null;
    const query = detect();
    if (query === null) {
      onClose();
      return;
    }
    onOpen(query);
  }, delayMs);
}
