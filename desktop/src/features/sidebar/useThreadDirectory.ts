import * as React from "react";
import {
  type InfiniteData,
  useInfiniteQuery,
  useMutation,
  useQuery,
  useQueryClient,
} from "@tanstack/react-query";

import { relayClient } from "@/shared/api/relayClient";
import {
  getThreadDirectoryPage,
  parseThreadDirectoryItemOverlay,
  publishThreadDirectoryState,
  type ThreadDirectoryItem,
  type ThreadDirectoryState,
  type ThreadDirectoryStateSnapshot,
} from "@/shared/api/threadDirectory";
import type { RelayEvent } from "@/shared/api/types";
import {
  discardPreviousThreadDirectoryScope,
  mergeThreadDirectoryLiveProjection,
  reconcileThreadDirectoryItems,
  rollbackThreadDirectoryOptimisticProjection,
  threadDirectoryLiveQueryKey,
  threadDirectoryQueryKey,
  threadDirectoryStateSnapshot,
  type StampedThreadDirectoryPage,
  type ThreadDirectoryLiveState,
  type ThreadDirectoryProjection,
  type ThreadDirectoryQueryScope,
} from "./lib/threadDirectory";

export type UseThreadDirectoryOptions = {
  channelId: string | null;
  communityId: string | null;
  relayUrl: string | null;
  pubkey: string | null;
  state?: ThreadDirectoryState;
  /** The caller enables this only while the stream-channel disclosure is open. */
  enabled: boolean;
};

export type ThreadDirectoryMutationInput = {
  rootId: string;
  patch: Partial<ThreadDirectoryStateSnapshot>;
};

type ThreadDirectoryMutationVariables = ThreadDirectoryMutationInput & {
  snapshot: ThreadDirectoryStateSnapshot;
};

type DirectoryData = InfiniteData<StampedThreadDirectoryPage, string | null>;

const EMPTY_LIVE_STATE: ThreadDirectoryLiveState = {
  nextOrder: 0,
  byRootId: new Map(),
};

function reserveSourceOrder(
  queryClient: ReturnType<typeof useQueryClient>,
  liveQueryKey: ReturnType<typeof threadDirectoryLiveQueryKey>,
) {
  let sourceOrder = 1;
  queryClient.setQueryData<ThreadDirectoryLiveState>(
    liveQueryKey,
    (current = EMPTY_LIVE_STATE) => {
      sourceOrder = current.nextOrder + 1;
      return { ...current, nextOrder: sourceOrder };
    },
  );
  return sourceOrder;
}

function currentItem(
  data: DirectoryData | undefined,
  liveState: ThreadDirectoryLiveState,
  state: ThreadDirectoryState,
  rootId: string,
) {
  return reconcileThreadDirectoryItems(
    data?.pages ?? [],
    liveState.byRootId,
    state,
    Math.floor(Date.now() / 1_000),
  ).find((item) => item.rootId === rootId);
}

/**
 * Lazy, community- and identity-isolated directory state for one disclosed
 * stream channel. The relay query remains authoritative; live overlays only
 * patch an already-open list between page refreshes.
 */
export function useThreadDirectory({
  channelId,
  communityId,
  relayUrl,
  pubkey,
  state = "active",
  enabled,
}: UseThreadDirectoryOptions) {
  const queryClient = useQueryClient();
  const queryKey = React.useMemo(
    () =>
      threadDirectoryQueryKey(communityId, relayUrl, pubkey, channelId, state),
    [channelId, communityId, pubkey, relayUrl, state],
  );
  const liveQueryKey = React.useMemo(
    () => threadDirectoryLiveQueryKey(communityId, relayUrl, pubkey, channelId),
    [channelId, communityId, pubkey, relayUrl],
  );
  const previousScopeRef = React.useRef<ThreadDirectoryQueryScope | null>(null);
  React.useEffect(() => {
    const currentScope = { client: queryClient, queryKey, liveQueryKey };
    discardPreviousThreadDirectoryScope(previousScopeRef.current, currentScope);
    previousScopeRef.current = currentScope;
  }, [liveQueryKey, queryClient, queryKey]);
  const liveQuery = useQuery({
    queryKey: liveQueryKey,
    queryFn: () => Promise.resolve(EMPTY_LIVE_STATE),
    initialData: EMPTY_LIVE_STATE,
    enabled: false,
    staleTime: Number.POSITIVE_INFINITY,
  });
  const queryEnabled =
    enabled &&
    channelId !== null &&
    communityId !== null &&
    relayUrl !== null &&
    pubkey !== null;

  const query = useInfiniteQuery<
    StampedThreadDirectoryPage,
    Error,
    DirectoryData,
    typeof queryKey,
    string | null
  >({
    queryKey,
    enabled: queryEnabled,
    initialPageParam: null,
    queryFn: async ({ pageParam }) => {
      if (!channelId)
        throw new Error("A channel is required for its thread directory.");
      const requestOrder = reserveSourceOrder(queryClient, liveQueryKey);
      const page = await getThreadDirectoryPage(channelId, state, pageParam);
      return { ...page, requestOrder };
    },
    getNextPageParam: (lastPage) =>
      lastPage.bounds.hasMore ? lastPage.bounds.nextCursor : undefined,
  });

  React.useEffect(() => {
    if (!queryEnabled || !channelId) return;
    let disposed = false;
    let subscribing = false;
    let unsubscribe: (() => Promise<void>) | null = null;
    let retryDelayMs = 1_000;
    let retryTimer: ReturnType<typeof setTimeout> | null = null;
    const startSubscription = () => {
      if (disposed || subscribing || unsubscribe) return;
      subscribing = true;
      void relayClient
        .subscribeToThreadDirectory(channelId, (event) => {
          let item: ThreadDirectoryItem;
          try {
            item = parseThreadDirectoryItemOverlay(event, channelId);
          } catch (error) {
            console.warn(
              "Ignoring malformed live thread-directory overlay",
              error,
            );
            return;
          }
          queryClient.setQueryData<ThreadDirectoryLiveState>(
            liveQueryKey,
            (current = EMPTY_LIVE_STATE) =>
              mergeThreadDirectoryLiveProjection(current, item, "live"),
          );
        })
        .then((dispose) => {
          retryDelayMs = 1_000;
          if (disposed) {
            void dispose();
          } else {
            unsubscribe = dispose;
          }
        })
        .catch((error) => {
          console.warn(
            "Could not subscribe to live thread-directory overlays",
            error,
          );
          if (!disposed) {
            retryTimer = setTimeout(startSubscription, retryDelayMs);
            retryDelayMs = Math.min(retryDelayMs * 2, 30_000);
          }
        })
        .finally(() => {
          subscribing = false;
        });
    };
    startSubscription();
    const unsubscribeReconnect = relayClient.subscribeToReconnects(() => {
      queryClient.setQueryData<ThreadDirectoryLiveState>(
        liveQueryKey,
        (current = EMPTY_LIVE_STATE) => ({
          nextOrder: current.nextOrder + 1,
          byRootId: new Map(),
        }),
      );
      void queryClient.resetQueries({ queryKey, exact: true });
      if (retryTimer) {
        clearTimeout(retryTimer);
        retryTimer = null;
      }
      startSubscription();
    });
    return () => {
      disposed = true;
      unsubscribeReconnect();
      if (retryTimer) clearTimeout(retryTimer);
      if (unsubscribe) void unsubscribe();
    };
  }, [channelId, liveQueryKey, queryClient, queryEnabled, queryKey]);

  const mutation = useMutation<
    RelayEvent,
    Error,
    ThreadDirectoryMutationVariables,
    {
      rootId: string;
      previousProjection: ThreadDirectoryProjection | undefined;
      optimisticOrder: number | null;
    }
  >({
    mutationFn: async ({ rootId, snapshot }) => {
      if (!channelId)
        throw new Error("A channel is required to update a thread.");
      return publishThreadDirectoryState(channelId, rootId, snapshot);
    },
    onMutate: async ({ rootId, snapshot }) => {
      await queryClient.cancelQueries({ queryKey, exact: true });
      const previous =
        queryClient.getQueryData<ThreadDirectoryLiveState>(liveQueryKey) ??
        EMPTY_LIVE_STATE;
      const item = currentItem(
        queryClient.getQueryData<DirectoryData>(queryKey),
        previous,
        state,
        rootId,
      );
      if (!item) {
        return {
          rootId,
          previousProjection: previous.byRootId.get(rootId),
          optimisticOrder: null,
        };
      }
      const optimistic = {
        ...item,
        titleOverride: snapshot.title,
        pinned: snapshot.pinned,
        archived: snapshot.archived,
      };
      let optimisticOrder: number | null = null;
      queryClient.setQueryData<ThreadDirectoryLiveState>(
        liveQueryKey,
        (current = EMPTY_LIVE_STATE) => {
          const next = mergeThreadDirectoryLiveProjection(
            current,
            optimistic,
            "optimistic",
          );
          optimisticOrder = next.byRootId.get(rootId)?.sourceOrder ?? null;
          return next;
        },
      );
      return {
        rootId,
        previousProjection: previous.byRootId.get(rootId),
        optimisticOrder,
      };
    },
    onError: (_error, _variables, context) => {
      if (!context || context.optimisticOrder === null) return;
      const optimisticOrder = context.optimisticOrder;
      queryClient.setQueryData<ThreadDirectoryLiveState>(
        liveQueryKey,
        (current = EMPTY_LIVE_STATE) =>
          rollbackThreadDirectoryOptimisticProjection(
            current,
            context.rootId,
            optimisticOrder,
            context.previousProjection,
          ),
      );
    },
    onSettled: () => queryClient.invalidateQueries({ queryKey, exact: true }),
  });

  const updateThread = React.useCallback(
    ({ rootId, patch }: ThreadDirectoryMutationInput) => {
      const item = currentItem(
        queryClient.getQueryData<DirectoryData>(queryKey),
        queryClient.getQueryData<ThreadDirectoryLiveState>(liveQueryKey) ??
          EMPTY_LIVE_STATE,
        state,
        rootId,
      );
      if (!item) {
        return Promise.reject(
          new Error("The thread is no longer in this directory."),
        );
      }
      return mutation.mutateAsync({
        rootId,
        patch,
        snapshot: threadDirectoryStateSnapshot(item, patch),
      });
    },
    [liveQueryKey, mutation.mutateAsync, queryClient, queryKey, state],
  );

  const items = React.useMemo(
    () =>
      reconcileThreadDirectoryItems(
        query.data?.pages ?? [],
        liveQuery.data.byRootId,
        state,
        Math.floor(Date.now() / 1_000),
      ),
    [liveQuery.data.byRootId, query.data?.pages, state],
  );

  return {
    ...query,
    items,
    updateThread,
    updateError: mutation.error,
    isUpdating: mutation.isPending,
  };
}
