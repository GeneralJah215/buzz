import type { AvatarPresentationState } from "@/features/profile/avatarPresentationStore";

/**
 * Decides whether community onboarding may publish its candidate avatar URL.
 *
 * BUG-053: the guard added by 80244f823 read
 *
 * ```ts
 * shouldSaveCandidate = state !== "failed" && state !== "pending";
 * ```
 *
 * `useAvatarPresentation` returns `null` for any URL the presentation store has
 * never tracked, so `state` is `undefined` for an *unknown* avatar — and an
 * unknown avatar sailed through a guard that was written to admit only an
 * avatar the app had watched render. Onboarding then published a URL it had
 * never seen load. It looked green unthrottled only because the profile query
 * usually had not resolved yet, so the candidate was still empty; at 8x CPU
 * throttle the seed lands first and the bug is deterministic.
 *
 * The rule here is GRD-014/GRD-015's, and BUG-052's: **absence of failure is
 * not success.** Only a positive, observed `"ready"` publishes.
 *
 * Two states are deliberately *not* "unknown":
 *
 * - A `data:` avatar (the emoji picker's inline SVG) needs no network fetch to
 *   render, so there is nothing for the presentation store to observe and
 *   nothing to verify. It is rendered by construction.
 * - A URL whose entry has already been evicted. `avatarPresentationStore` drops
 *   a `"ready"` entry 30 s after it settles, so a user who uploads an avatar and
 *   then types slowly would otherwise lose it. `hasObservedReady` carries the
 *   observation the store forgot. A live `"pending"`/`"failed"` still wins over
 *   that memory, so a re-uploaded URL cannot ride an old success.
 */

/**
 * Emoji avatars are `data:image/svg+xml,…`. They are inline markup, not a
 * fetched resource, so a presentation entry never exists for them.
 */
const INLINE_AVATAR_URL_PREFIX = "data:";

export type AvatarSaveDecision = {
  /**
   * Hand off to `registerAvatarWhenReady` so the avatar is published later, once
   * the presentation store confirms the upload landed.
   */
  deferUntilReady: boolean;
  /** Send `avatarUrl` on the profile update happening right now. */
  saveCandidateUrl: boolean;
};

const REFUSE: AvatarSaveDecision = {
  deferUntilReady: false,
  saveCandidateUrl: false,
};

export function isInlineAvatarUrl(candidateAvatarUrl: string): boolean {
  return candidateAvatarUrl.startsWith(INLINE_AVATAR_URL_PREFIX);
}

export function decideAvatarSave({
  candidateAvatarUrl,
  hasObservedReady = false,
  presentationState,
}: {
  /** Already trimmed. */
  candidateAvatarUrl: string;
  /** `"ready"` was seen for this exact URL earlier in this session. */
  hasObservedReady?: boolean;
  /** `undefined`/`null` means the store has never tracked this URL. */
  presentationState: AvatarPresentationState | null | undefined;
}): AvatarSaveDecision {
  if (candidateAvatarUrl.length === 0) return REFUSE;

  // A live negative outranks everything, including a remembered success.
  if (presentationState === "failed" || presentationState === "pending") {
    return { deferUntilReady: true, saveCandidateUrl: false };
  }

  if (presentationState === "ready") {
    return { deferUntilReady: false, saveCandidateUrl: true };
  }

  if (isInlineAvatarUrl(candidateAvatarUrl)) {
    return { deferUntilReady: false, saveCandidateUrl: true };
  }

  // Presentation is null: either an avatar seeded from the relay profile that
  // this session never watched load, or one whose ready entry has aged out.
  // Only the second is a positive observation, and only it may publish.
  if (hasObservedReady) {
    return { deferUntilReady: false, saveCandidateUrl: true };
  }

  // Unknown. Not failed, but not observed ready either — refuse, and do not
  // queue a deferred save: there is no presentation to wait on, so a
  // registration here could only ever resolve to nothing.
  return REFUSE;
}
