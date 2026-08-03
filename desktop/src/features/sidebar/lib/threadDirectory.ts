import type {
  ThreadDirectoryItem,
  ThreadDirectoryPage,
  ThreadDirectoryState,
  ThreadDirectoryStateSnapshot,
} from "@/shared/api/threadDirectory";

export type StampedThreadDirectoryPage = ThreadDirectoryPage & {
  /** Monotonic order captured before this page request starts. */
  requestOrder: number;
};

export type ThreadDirectoryProjectionSource = "page" | "live" | "optimistic";

export type ThreadDirectoryProjection = {
  item: ThreadDirectoryItem;
  source: ThreadDirectoryProjectionSource;
  /** Orders page request starts and live observations within one query scope. */
  sourceOrder: number;
};

export type ThreadDirectoryLiveState = {
  nextOrder: number;
  byRootId: ReadonlyMap<string, ThreadDirectoryProjection>;
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

export function threadDirectoryLiveQueryKey(
  communityId: string | null,
  relayUrl: string | null,
  pubkey: string | null,
  channelId: string | null,
) {
  return [
    "thread-directory-live",
    communityId,
    relayUrl,
    pubkey?.toLowerCase() ?? null,
    channelId,
  ] as const;
}

type DirectoryQueryKey =
  | ReturnType<typeof threadDirectoryQueryKey>
  | ReturnType<typeof threadDirectoryLiveQueryKey>;

export function sameThreadDirectoryQueryKey(
  left: DirectoryQueryKey,
  right: DirectoryQueryKey,
): boolean {
  return (
    left.length === right.length &&
    left.every((value, index) => value === right[index])
  );
}

type DirectoryScopeClient = {
  removeQueries(options: { queryKey: DirectoryQueryKey; exact: true }): unknown;
};

export type ThreadDirectoryQueryScope = {
  client: DirectoryScopeClient;
  queryKey: ReturnType<typeof threadDirectoryQueryKey>;
  liveQueryKey: ReturnType<typeof threadDirectoryLiveQueryKey>;
};

/** Drop only an obsolete exact scope; sibling channels remain untouched. */
export function discardPreviousThreadDirectoryScope(
  previous: ThreadDirectoryQueryScope | null,
  current: ThreadDirectoryQueryScope,
) {
  if (!previous) return;
  if (
    previous.client !== current.client ||
    !sameThreadDirectoryQueryKey(previous.queryKey, current.queryKey)
  ) {
    void previous.client.removeQueries({
      queryKey: previous.queryKey,
      exact: true,
    });
  }
  if (
    previous.client !== current.client ||
    !sameThreadDirectoryQueryKey(previous.liveQueryKey, current.liveQueryKey)
  ) {
    void previous.client.removeQueries({
      queryKey: previous.liveQueryKey,
      exact: true,
    });
  }
}

function activityAt(
  item: Pick<ThreadDirectoryItem, "lastReplyAt" | "rootCreatedAt">,
) {
  return item.lastReplyAt || item.rootCreatedAt;
}

function compareStateRevision(
  left: ThreadDirectoryItem,
  right: ThreadDirectoryItem,
): number {
  if (left.stateCreatedAt !== right.stateCreatedAt) {
    return left.stateCreatedAt - right.stateCreatedAt;
  }
  if (left.stateEventId === right.stateEventId) return 0;
  if (left.stateEventId === null) return -1;
  if (right.stateEventId === null) return 1;
  return right.stateEventId.localeCompare(left.stateEventId);
}

function compareProjectionVector(
  left: ThreadDirectoryItem,
  right: ThreadDirectoryItem,
): number | null {
  const state = compareStateRevision(left, right);
  const activity = Math.sign(activityAt(left) - activityAt(right));
  if (state >= 0 && activity >= 0) return state || activity;
  if (state <= 0 && activity <= 0) return state || activity;
  return null;
}

/**
 * Choose one atomic relay projection for a root. Page/live source order is a
 * causal fence: an overlay observed after a request started beats that late
 * page, while a page started afterward is relay-authoritative.
 */
export function chooseThreadDirectoryProjection(
  left: ThreadDirectoryProjection,
  right: ThreadDirectoryProjection,
): ThreadDirectoryProjection {
  if (left.source !== right.source && left.sourceOrder !== right.sourceOrder) {
    return left.sourceOrder > right.sourceOrder ? left : right;
  }

  if (left.item.projectionCreatedAt !== right.item.projectionCreatedAt) {
    return left.item.projectionCreatedAt > right.item.projectionCreatedAt
      ? left
      : right;
  }

  if (left.sourceOrder !== right.sourceOrder) {
    return left.sourceOrder > right.sourceOrder ? left : right;
  }

  const vectorOrder = compareProjectionVector(left.item, right.item);
  if (vectorOrder !== null && vectorOrder !== 0) {
    return vectorOrder > 0 ? left : right;
  }

  if (left.item.projectionEventId !== right.item.projectionEventId) {
    return left.item.projectionEventId < right.item.projectionEventId
      ? left
      : right;
  }
  if (left.item.present !== right.item.present) {
    return left.item.present ? right : left;
  }
  return left;
}

/** Apply the relay's active/archived membership predicate to one projection. */
export function isThreadDirectoryItemInState(
  item: Pick<
    ThreadDirectoryItem,
    | "present"
    | "archived"
    | "pinned"
    | "titleOverride"
    | "descendantCount"
    | "lastReplyAt"
    | "rootCreatedAt"
  >,
  state: ThreadDirectoryState,
  asOfSeconds: number,
): boolean {
  if (!item.present || item.descendantCount === 0) return false;
  if (state === "archived") return item.archived;
  return (
    !item.archived &&
    (item.titleOverride !== null ||
      item.pinned ||
      (item.descendantCount >= 3 &&
        activityAt(item) >= asOfSeconds - 30 * 24 * 60 * 60))
  );
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

export function threadDirectoryProjection(
  item: ThreadDirectoryItem,
  source: ThreadDirectoryProjectionSource,
  sourceOrder: number,
): ThreadDirectoryProjection {
  return { item, source, sourceOrder };
}

/** Reconcile every page and live overlay before filtering or global ordering. */
export function reconcileThreadDirectoryItems(
  pages: readonly StampedThreadDirectoryPage[],
  liveByRootId: ReadonlyMap<string, ThreadDirectoryProjection>,
  state: ThreadDirectoryState,
  asOfSeconds: number,
): ThreadDirectoryItem[] {
  const byRoot = new Map<string, ThreadDirectoryProjection>();
  const add = (projection: ThreadDirectoryProjection) => {
    const current = byRoot.get(projection.item.rootId);
    byRoot.set(
      projection.item.rootId,
      current
        ? chooseThreadDirectoryProjection(current, projection)
        : projection,
    );
  };
  for (const page of pages) {
    for (const item of page.items) {
      add(threadDirectoryProjection(item, "page", page.requestOrder));
    }
  }
  for (const projection of liveByRootId.values()) add(projection);
  return sortThreadDirectoryItems(
    [...byRoot.values()]
      .map((projection) => projection.item)
      .filter((item) => isThreadDirectoryItemInState(item, state, asOfSeconds)),
  );
}

export function mergeThreadDirectoryLiveProjection(
  current: ThreadDirectoryLiveState,
  item: ThreadDirectoryItem,
  source: Extract<ThreadDirectoryProjectionSource, "live" | "optimistic">,
): ThreadDirectoryLiveState {
  const sourceOrder = current.nextOrder + 1;
  const projection = threadDirectoryProjection(item, source, sourceOrder);
  const byRootId = new Map(current.byRootId);
  const previous = byRootId.get(item.rootId);
  byRootId.set(
    item.rootId,
    previous
      ? chooseThreadDirectoryProjection(previous, projection)
      : projection,
  );
  return { nextOrder: sourceOrder, byRootId };
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
      isUnread &&
      loadedUnreadCount !== null &&
      Number.isSafeInteger(loadedUnreadCount) &&
      loadedUnreadCount > 0
        ? loadedUnreadCount
        : null,
  };
}
