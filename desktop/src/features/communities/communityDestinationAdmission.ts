/**
 * Decides whether a community switch may pre-navigate straight into the channel
 * the user last had open in the target community.
 *
 * BUG-052. Switching communities used to write `#/channels/<remembered>` into
 * the hash the instant the transition began, guarded by nothing but "the stored
 * destination is a channel". Whether that channel exists in the target
 * community was not merely unverified — it was UNTRACKED, and an untracked
 * state slipped past the guard exactly the way a failed one would not. When the
 * target relay's `get_channels` rejected, no live validation ever succeeded,
 * nothing repaired the route, and the app sat inside a channel it had never
 * confirmed.
 *
 * The rule here is the one GRD-014/GRD-015 already impose on status surfaces:
 * **absence of failure is not success.** Only a positive, observed record of the
 * channel admits the navigation. "Never observed", "observation still in
 * flight", and "observation failed" are all treated identically — refused. A
 * refusal is cheap: the switch lands on Home, and `AppShell`'s restore effect
 * navigates into the channel once a live `get_channels` actually succeeds.
 *
 * The evidence is the per-relay channel snapshot, which is only ever written
 * from a completed `get_channels` for that relay (`channelSnapshot.ts`), so its
 * presence is a real past observation rather than an assumption.
 */

import { readChannelSnapshot } from "@/features/channels/channelSnapshot";
import type { CommunityDestination } from "@/features/communities/communityNavigationStorage";
import type { Channel } from "@/shared/api/types";

/**
 * What we actually know about the remembered channel in the target community.
 * `unobserved` is deliberately its own state rather than being folded into
 * `unavailable`: the two refuse for different reasons, and collapsing them is
 * how "not failed" gets mistaken for "succeeded".
 */
export type RememberedChannelObservation =
  | "observed-available"
  | "observed-unavailable"
  | "unobserved";

export function observeRememberedChannel(
  channelId: string,
  observedChannels: readonly Channel[] | null | undefined,
): RememberedChannelObservation {
  // No snapshot at all: this relay's channel list has never been read to
  // completion on this machine. Nothing failed — and nothing succeeded either.
  if (!observedChannels) {
    return "unobserved";
  }

  const match = observedChannels.find((channel) => channel?.id === channelId);
  if (!match) {
    return "observed-unavailable";
  }
  // Mirrors the live availability rule in AppShell's restore effect: a channel
  // the user is not in, or one that has been archived, is not somewhere we may
  // drop them without warning.
  if (match.isMember !== true || match.archivedAt !== null) {
    return "observed-unavailable";
  }
  return "observed-available";
}

/**
 * The channel id the switch may pre-navigate into, or `null` to stay on Home.
 * Pure: takes the evidence rather than reading it, so the refusal path is
 * unit-testable without storage.
 */
export function admitRememberedChannelRoute(
  destination: CommunityDestination | null | undefined,
  observedChannels: readonly Channel[] | null | undefined,
): string | null {
  if (destination?.kind !== "channel") {
    return null;
  }
  return observeRememberedChannel(destination.channelId, observedChannels) ===
    "observed-available"
    ? destination.channelId
    : null;
}

/**
 * Call-site convenience: reads the target relay's observed channel list and
 * applies {@link admitRememberedChannelRoute}.
 */
export function admitCommunityDestinationRoute(
  destination: CommunityDestination | null | undefined,
  relayUrl: string | null | undefined,
): string | null {
  if (destination?.kind !== "channel") {
    return null;
  }
  const observedChannels = relayUrl ? readChannelSnapshot(relayUrl) : null;
  return admitRememberedChannelRoute(destination, observedChannels);
}
