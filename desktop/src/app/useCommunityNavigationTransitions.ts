import { useRouter } from "@tanstack/react-router";
import * as React from "react";

import type { deriveShellRoute } from "@/app/AppShell.helpers";
import type { useAppNavigation } from "@/app/navigation/useAppNavigation";
import {
  replaceCommunityDestinationRoute,
  runCommunityViewTransition,
} from "@/app/communityViewTransition";
import { admitCommunityDestinationRoute } from "@/features/communities/communityDestinationAdmission";
import {
  loadCommunityDestination,
  markPendingCommunityRestore,
  saveCommunityDestination,
} from "@/features/communities/communityNavigationStorage";
import type { useCommunities } from "@/features/communities/useCommunities";

type Communities = ReturnType<typeof useCommunities>;
type ShellRoute = ReturnType<typeof deriveShellRoute>;
type GoHome = ReturnType<typeof useAppNavigation>["goHome"];

export function useCommunityNavigationTransitions({
  communities,
  goHome,
  selectedChannelId,
  selectedView,
}: {
  communities: Communities;
  goHome: GoHome;
  selectedChannelId: ShellRoute["selectedChannelId"];
  selectedView: ShellRoute["selectedView"];
}) {
  const router = useRouter();
  const saveActiveDestination = React.useCallback(() => {
    const activeCommunityId = communities.activeCommunity?.id;
    if (!activeCommunityId) return;
    saveCommunityDestination(
      activeCommunityId,
      selectedView === "channel" && selectedChannelId
        ? { kind: "channel", channelId: selectedChannelId }
        : { kind: "home" },
    );
  }, [communities.activeCommunity?.id, selectedChannelId, selectedView]);

  // Home is a teardown barrier: the outgoing channel must unmount before the
  // relay changes, or its read effect can advance markers on the wrong relay.
  const switchCommunity = React.useCallback(
    async (id: string) => {
      const activeCommunityId = communities.activeCommunity?.id;
      if (id === activeCommunityId) return;
      if (!activeCommunityId) {
        communities.switchCommunity(id);
        return;
      }

      const target = communities.communities.find(
        (community) => community.id === id,
      );

      await runCommunityViewTransition(async () => {
        saveActiveDestination();
        await goHome({ replace: true });
        markPendingCommunityRestore(id);
        // BUG-052: only a positive, observed record of the channel in the
        // TARGET community admits this pre-navigation. Unobserved, in-flight
        // and failed reads all stay on the Home barrier, where AppShell's
        // restore effect picks the route up after a live read succeeds.
        const admittedChannelId = admitCommunityDestinationRoute(
          loadCommunityDestination(id),
          target?.relayUrl,
        );
        if (admittedChannelId !== null) {
          replaceCommunityDestinationRoute(admittedChannelId, router.history);
        }
        communities.switchCommunity(id);
      });
    },
    [communities, goHome, router.history, saveActiveDestination],
  );

  const removeCommunity = React.useCallback(
    async (id: string) => {
      if (id !== communities.activeCommunity?.id) {
        communities.removeCommunity(id);
        return;
      }
      const fallback = communities.communities.find(
        (community) => community.id !== id,
      );
      if (!fallback) return;

      await runCommunityViewTransition(async () => {
        saveActiveDestination();
        await goHome({ replace: true });
        markPendingCommunityRestore(fallback.id);
        // Same admission rule as switchCommunity (BUG-052).
        const admittedChannelId = admitCommunityDestinationRoute(
          loadCommunityDestination(fallback.id),
          fallback.relayUrl,
        );
        if (admittedChannelId !== null) {
          replaceCommunityDestinationRoute(admittedChannelId, router.history);
        }
        communities.removeCommunity(id);
      });
    },
    [communities, goHome, router.history, saveActiveDestination],
  );

  return { removeCommunity, switchCommunity };
}
