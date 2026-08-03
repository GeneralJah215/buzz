import type {
  ThreadDirectoryItem,
  ThreadDirectoryPage,
  ThreadDirectoryState,
  ThreadDirectoryStateSnapshot,
} from "@/shared/api/threadDirectory";

export type ThreadDirectoryCachePage = ThreadDirectoryPage & {
  /** Roots removed by a newer live overlay must not be revived by a late page. */
  removedRootIds: ReadonlySet<string>;
};

export function threadDirectoryQueryKey(
  communityId: string | null,
  relayUrl: string | null,
  pubkey: string | null,
  channelId: string | null,
  state: ThreadDirectoryState,
) {
  return [
    "thread-directory",
    communityId,
    relayUrl,
    pubkey?.toLowerCase() ?? null,
    channelId,
    state,
  ] as const;
}

function activityAt(
  item: Pick<ThreadDirectoryItem, "lastReplyAt" | "rootCreatedAt">,
) {
  return item.lastReplyAt || item.rootCreatedAt;
}

/** Apply the relay's active ordering to a copied item list. */
export function sortThreadDirectoryItems<
  T extends Pick<
    ThreadDirectoryItem,
    "rootId" | "pinned" | "lastReplyAt" | "rootCreatedAt"
  >,
>(items: readonly T[]): T[] {
  return [...items].sort((left, right) => {
    if (left.pinned !== right.pinned) return left.pinned ? -1 : 1;
    const activityDifference = activityAt(right) - activityAt(left);
    return activityDifference || left.rootId.localeCompare(right.rootId);
  });
}

/** Resolve the relay-projected title, retaining a defensive empty-title fallback. */
export function resolveThreadDirectoryTitle(
  item: Pick<ThreadDirectoryItem, "title" | "titleOverride">,
): string {
  return item.titleOverride?.trim() || item.title.trim() || "Untitled thread";
}

/** Build the canonical full snapshot required by every shared-state mutation. */
export function threadDirectoryStateSnapshot(
  item: Pick<ThreadDirectoryItem, "titleOverride" | "pinned" | "archived">,
  patch: Partial<ThreadDirectoryStateSnapshot>,
): ThreadDirectoryStateSnapshot {
  const snapshot = {
    title: item.titleOverride,
    pinned: item.pinned,
    archived: item.archived,
    ...patch,
  };
  if (snapshot.pinned && snapshot.archived) {
    throw new Error("A thread cannot be pinned and archived at the same time.");
  }
  return snapshot;
}

/** Merge a relay page without reintroducing roots a newer overlay removed. */
export function mergeThreadDirectoryPage(
  current: Pick<ThreadDirectoryCachePage, "items" | "removedRootIds">,
  incoming: readonly ThreadDirectoryItem[],
): Pick<ThreadDirectoryCachePage, "items" | "removedRootIds"> {
  const byRoot = new Map(current.items.map((item) => [item.rootId, item]));
  for (const item of incoming) {
    if (!current.removedRootIds.has(item.rootId)) byRoot.set(item.rootId, item);
  }
  return {
    items: sortThreadDirectoryItems([...byRoot.values()]),
    removedRootIds: current.removedRootIds,
  };
}

/** Add/replace a live item when it belongs to the currently viewed directory. */
export function mergeLiveThreadDirectoryItem(
  page: ThreadDirectoryCachePage,
  item: ThreadDirectoryItem,
): ThreadDirectoryCachePage {
  const removedRootIds = new Set(page.removedRootIds);
  removedRootIds.delete(item.rootId);
  return {
    ...page,
    ...mergeThreadDirectoryPage({ items: page.items, removedRootIds }, [item]),
  };
}

/** Remove a root after a live transition out of the current active/archived view. */
export function removeThreadDirectoryItem(
  page: ThreadDirectoryCachePage,
  rootId: string,
): ThreadDirectoryCachePage {
  return {
    ...page,
    items: page.items.filter((item) => item.rootId !== rootId),
    removedRootIds: new Set([...page.removedRootIds, rootId]),
  };
}

/** The directory proves a dot; a numeric count is valid only when supplied by loaded thread data. */
export function threadDirectoryUnreadState(
  item: Pick<ThreadDirectoryItem, "lastReplyAt" | "rootCreatedAt">,
  getThreadReadAt: () => number | null,
  loadedUnreadCount: number | null,
): { isUnread: boolean; unreadCount: number | null } {
  const readAt = getThreadReadAt();
  const isUnread = readAt === null || activityAt(item) > readAt;
  return {
    isUnread,
    unreadCount:
      loadedUnreadCount !== null &&
      Number.isSafeInteger(loadedUnreadCount) &&
      loadedUnreadCount >= 0
        ? loadedUnreadCount
        : null,
  };
}
