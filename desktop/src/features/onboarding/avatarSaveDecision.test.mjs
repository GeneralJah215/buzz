/**
 * BUG-053 guardrail: community onboarding must publish only an avatar URL it
 * has positively observed render.
 *
 * These are pure-logic tests on purpose. The bug hid behind timing — the e2e
 * spec passed unthrottled only because the profile seed had not resolved yet —
 * so a timing test would go green on a fast machine for the wrong reason. What
 * has to be nailed down is the *predicate*: an unknown (null/undefined)
 * presentation state is refused, not admitted.
 */
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

import { decideAvatarSave, isInlineAvatarUrl } from "./avatarSaveDecision.ts";

const __dirname = path.dirname(fileURLToPath(import.meta.url));

const REMOTE_URL = "https://mock.relay/media/community-avatar.png";
const EMOJI_URL =
  "data:image/svg+xml,%3Csvg%20xmlns%3D%22http%3A%2F%2Fwww.w3.org%2F2000%2Fsvg%22%3E%3C%2Fsvg%3E";

// ---------------------------------------------------------------------------
// The bug itself: unknown is not ready
// ---------------------------------------------------------------------------

test("unknown_presentation_state_is_refused_when_undefined", () => {
  assert.deepEqual(
    decideAvatarSave({
      candidateAvatarUrl: REMOTE_URL,
      presentationState: undefined,
    }),
    { deferUntilReady: false, saveCandidateUrl: false },
  );
});

test("unknown_presentation_state_is_refused_when_null", () => {
  assert.deepEqual(
    decideAvatarSave({
      candidateAvatarUrl: REMOTE_URL,
      presentationState: null,
    }),
    { deferUntilReady: false, saveCandidateUrl: false },
  );
});

test("unknown_presentation_state_does_not_queue_a_deferred_save", () => {
  // There is no presentation to wait on, so a registration could only ever
  // resolve to nothing. Refusing outright keeps the no-op registration out.
  assert.equal(
    decideAvatarSave({
      candidateAvatarUrl: REMOTE_URL,
      presentationState: null,
    }).deferUntilReady,
    false,
  );
});

test("a_relay_seeded_avatar_this_session_never_watched_is_not_republished", () => {
  // Exactly the onboarding.spec.ts case: getProfile() seeds avatarUrl, nothing
  // ever calls beginAvatarPresentation for it, so the store knows nothing.
  const decision = decideAvatarSave({
    candidateAvatarUrl:
      "https://mock.relay/media/existing-community-avatar.png",
    hasObservedReady: false,
    presentationState: undefined,
  });
  assert.equal(decision.saveCandidateUrl, false);
});

// ---------------------------------------------------------------------------
// Only a positive, observed "ready" publishes
// ---------------------------------------------------------------------------

test("ready_presentation_state_publishes_immediately", () => {
  assert.deepEqual(
    decideAvatarSave({
      candidateAvatarUrl: REMOTE_URL,
      presentationState: "ready",
    }),
    { deferUntilReady: false, saveCandidateUrl: true },
  );
});

test("pending_presentation_state_defers_instead_of_publishing", () => {
  assert.deepEqual(
    decideAvatarSave({
      candidateAvatarUrl: REMOTE_URL,
      presentationState: "pending",
    }),
    { deferUntilReady: true, saveCandidateUrl: false },
  );
});

test("failed_presentation_state_defers_instead_of_publishing", () => {
  assert.deepEqual(
    decideAvatarSave({
      candidateAvatarUrl: REMOTE_URL,
      presentationState: "failed",
    }),
    { deferUntilReady: true, saveCandidateUrl: false },
  );
});

test("every_non_ready_state_refuses_to_publish", () => {
  for (const presentationState of ["failed", "pending", null, undefined]) {
    assert.equal(
      decideAvatarSave({ candidateAvatarUrl: REMOTE_URL, presentationState })
        .saveCandidateUrl,
      false,
      `state ${String(presentationState)} must not publish`,
    );
  }
});

// ---------------------------------------------------------------------------
// The remembered observation, and what may not override it
// ---------------------------------------------------------------------------

test("an_evicted_ready_entry_still_publishes_via_the_remembered_observation", () => {
  // avatarPresentationStore deletes a ready entry 30 s after it settles.
  assert.deepEqual(
    decideAvatarSave({
      candidateAvatarUrl: REMOTE_URL,
      hasObservedReady: true,
      presentationState: null,
    }),
    { deferUntilReady: false, saveCandidateUrl: true },
  );
});

test("a_live_pending_state_outranks_a_remembered_ready", () => {
  assert.deepEqual(
    decideAvatarSave({
      candidateAvatarUrl: REMOTE_URL,
      hasObservedReady: true,
      presentationState: "pending",
    }),
    { deferUntilReady: true, saveCandidateUrl: false },
  );
});

test("a_live_failed_state_outranks_a_remembered_ready", () => {
  assert.deepEqual(
    decideAvatarSave({
      candidateAvatarUrl: REMOTE_URL,
      hasObservedReady: true,
      presentationState: "failed",
    }),
    { deferUntilReady: true, saveCandidateUrl: false },
  );
});

test("hasObservedReady_defaults_to_false", () => {
  assert.equal(
    decideAvatarSave({
      candidateAvatarUrl: REMOTE_URL,
      presentationState: undefined,
    }).saveCandidateUrl,
    false,
  );
});

// ---------------------------------------------------------------------------
// Inline (emoji) avatars need no presentation
// ---------------------------------------------------------------------------

test("an_emoji_data_url_publishes_without_a_presentation_entry", () => {
  // The emoji picker never uploads, so beginAvatarPresentation is never called.
  // Requiring "ready" for inline markup would silently drop emoji avatars.
  assert.deepEqual(
    decideAvatarSave({
      candidateAvatarUrl: EMOJI_URL,
      presentationState: undefined,
    }),
    { deferUntilReady: false, saveCandidateUrl: true },
  );
});

test("isInlineAvatarUrl_only_matches_data_urls", () => {
  assert.equal(isInlineAvatarUrl(EMOJI_URL), true);
  assert.equal(isInlineAvatarUrl(REMOTE_URL), false);
  assert.equal(isInlineAvatarUrl(""), false);
});

// ---------------------------------------------------------------------------
// Empty candidate
// ---------------------------------------------------------------------------

test("an_empty_candidate_neither_publishes_nor_defers", () => {
  assert.deepEqual(
    decideAvatarSave({ candidateAvatarUrl: "", presentationState: "ready" }),
    { deferUntilReady: false, saveCandidateUrl: false },
  );
});

// ---------------------------------------------------------------------------
// Wiring: the flow must route its decision through this predicate
// ---------------------------------------------------------------------------

function readCommunityOnboardingFlowSource() {
  return fs.readFileSync(
    path.join(__dirname, "ui", "CommunityOnboardingFlow.tsx"),
    "utf8",
  );
}

test("the_community_onboarding_flow_routes_its_avatar_decision_through_decideAvatarSave", () => {
  const source = readCommunityOnboardingFlowSource();
  assert.match(source, /decideAvatarSave\(\{/u);
  assert.match(
    source,
    /avatarUrl:\s*saveCandidateUrl\s*\?\s*candidateAvatarUrl\s*:\s*undefined/u,
    "updateProfile must only carry the candidate when the decision says so",
  );
});

test("the_community_onboarding_flow_no_longer_carries_the_not_failed_not_pending_predicate", () => {
  // The exact shape of the BUG-053 defect. Re-introducing it inline, bypassing
  // decideAvatarSave, fails here.
  const source = readCommunityOnboardingFlowSource();
  assert.equal(
    /presentationState\s*!==\s*"pending"/u.test(source),
    false,
    'the "not pending" half of the old guard must not come back',
  );
  assert.equal(
    /presentationState\s*!==\s*"failed"/u.test(source),
    false,
    'the "not failed" half of the old guard must not come back',
  );
});

test("the_community_onboarding_flow_remembers_an_observed_ready_avatar", () => {
  assert.match(
    readCommunityOnboardingFlowSource(),
    /hasObservedReady:\s*observedReadyAvatarUrlsRef\.current\.has\(/u,
    "the evicted-entry memory must be fed into the decision",
  );
});
