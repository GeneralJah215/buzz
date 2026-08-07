import { KIND_STREAM_MESSAGE } from "@/shared/constants/kinds";
import { invokeTauri } from "@/shared/api/tauri";
import type { RelaySubscriptionFilter } from "@/shared/api/relayClientShared";

/**
 * The `(edge endpoint, canonical origin, community)` triple the backend
 * resolved for this workspace, or `null` when edge routing is off — an unset
 * `BUZZ_EDGE_RELAY_URL`, no bound community, or a rejected edge URL all yield
 * `null`, and `null` means today's canonical-only routing.
 */
export interface EdgeRelayBinding {
  relayUrl: string;
  httpUrl: string;
  canonicalOrigin: string;
  communityId: string;
}

export function getEdgeRelayBinding(): Promise<EdgeRelayBinding | null> {
  return invokeTauri<EdgeRelayBinding | null>("get_edge_relay_binding");
}

export interface SplitEdgeFilter {
  edge: RelaySubscriptionFilter;
  canonical: RelaySubscriptionFilter | null;
}

export function splitEdgeMessageFilter(
  filter: RelaySubscriptionFilter,
): SplitEdgeFilter | null {
  const channels = filter["#h"];
  const kinds = filter.kinds;
  if (
    !Array.isArray(channels) ||
    channels.length === 0 ||
    !channels.every(isUuid) ||
    !Array.isArray(kinds) ||
    !kinds.includes(KIND_STREAM_MESSAGE)
  ) {
    return null;
  }

  const canonicalKinds = kinds.filter((kind) => kind !== KIND_STREAM_MESSAGE);
  return {
    edge: { ...filter, kinds: [KIND_STREAM_MESSAGE] },
    canonical:
      canonicalKinds.length > 0 ? { ...filter, kinds: canonicalKinds } : null,
  };
}

function isUuid(value: unknown): value is string {
  return (
    typeof value === "string" &&
    /^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(
      value,
    )
  );
}
