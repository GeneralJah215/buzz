import * as React from "react";
import {
  type InfiniteData,
  useInfiniteQuery,
  useMutation,
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
  mergeLiveThreadDirectoryItem,
  removeThreadDirectoryItem,
  threadDirectoryQueryKey,
  threadDirectoryStateSnapshot,
  type ThreadDirectoryCachePage,
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

type DirectoryData = InfiniteData<ThreadDirectoryCachePage>;

function cachePage(
  page: Awaited<ReturnType<typeof getThreadDirectoryPage>>,
): ThreadDirectoryCachePage {
  return { ...page, removedRootIds: new Set<string>() };
}

function currentItem(data: DirectoryData | undefined, rootId: string) {
  return data?.pages
    .flatMap((page) => page.items)
    .find((item) => item.rootId === rootId);
}

function isInState(item: ThreadDirectoryItem, state: ThreadDirectoryState) {
  return state === "archived" ? item.archived : !item.archived;
}

function mergeLiveItem(
  data: DirectoryData | undefined,
  item: ThreadDirectoryItem,
  state: ThreadDirectoryState,
): DirectoryData | undefined {
  if (!data) return data;
  const matchingState = isInState(item, state);
  const alreadyPresent = data.pages.some((page) =>
    page.items.some((current) => current.rootId === item.rootId),
  );
  return {
    ...data,
    pages: data.pages.map((page, index) => {
      const hasItem = page.items.some(
        (current) => current.rootId === item.rootId,
      );
      if (!matchingState) return removeThreadDirectoryItem(page, item.rootId);
      if (hasItem || (index === 0 && !alreadyPresent)) {
        return mergeLiveThreadDirectoryItem(page, item);
      }
      return page;
    }),
  };
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
  const queryEnabled =
    enabled &&
    channelId !== null &&
    communityId !== null &&
    relayUrl !== null &&
    pubkey !== null;

  const query = useInfiniteQuery<
    ThreadDirectoryCachePage,
    Error,
    DirectoryData,
    typeof queryKey,
    string | null
  >({
    queryKey,
    enabled: queryEnabled,
    initialPageParam: null,
    queryFn: ({ pageParam }) => {
      if (!channelId)
        throw new Error("A channel is required for its thread directory.");
      return getThreadDirectoryPage(channelId, state, pageParam).then(
        cachePage,
      );
    },
    getNextPageParam: (lastPage) =>
      lastPage.bounds.hasMore ? lastPage.bounds.nextCursor : undefined,
  });

  React.useEffect(() => {
    if (!queryEnabled || !channelId) return;
    let disposed = false;
    let unsubscribe: (() => Promise<void>) | null = null;
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
        queryClient.setQueryData<DirectoryData>(queryKey, (current) =>
          mergeLiveItem(current, item, state),
        );
      })
      .then((dispose) => {
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
      });
    const unsubscribeReconnect = relayClient.subscribeToReconnects(() => {
      void queryClient.invalidateQueries({ queryKey, exact: true });
    });
    return () => {
      disposed = true;
      unsubscribeReconnect();
      if (unsubscribe) void unsubscribe();
    };
  }, [channelId, queryClient, queryEnabled, queryKey, state]);

  const mutation = useMutation<
    RelayEvent,
    Error,
    ThreadDirectoryMutationVariables,
    { previous: DirectoryData | undefined }
  >({
    mutationFn: async ({ rootId, snapshot }) => {
      if (!channelId)
        throw new Error("A channel is required to update a thread.");
      return publishThreadDirectoryState(channelId, rootId, snapshot);
    },
    onMutate: async ({ rootId, snapshot }) => {
      await queryClient.cancelQueries({ queryKey, exact: true });
      const previous = queryClient.getQueryData<DirectoryData>(queryKey);
      const item = currentItem(previous, rootId);
      if (!item) return { previous };
      const optimistic = {
        ...item,
        titleOverride: snapshot.title,
        pinned: snapshot.pinned,
        archived: snapshot.archived,
      };
      queryClient.setQueryData<DirectoryData>(queryKey, (current) =>
        mergeLiveItem(current, optimistic, state),
      );
      return { previous };
    },
    onError: (_error, _variables, context) => {
      queryClient.setQueryData(queryKey, context?.previous);
    },
    onSettled: () => queryClient.invalidateQueries({ queryKey, exact: true }),
  });

  const updateThread = React.useCallback(
    ({ rootId, patch }: ThreadDirectoryMutationInput) => {
      const item = currentItem(
        queryClient.getQueryData<DirectoryData>(queryKey),
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
    [mutation.mutateAsync, queryClient, queryKey],
  );

  return {
    ...query,
    items: query.data?.pages.flatMap((page) => page.items) ?? [],
    updateThread,
    updateError: mutation.error,
    isUpdating: mutation.isPending,
  };
}
