import * as React from "react";
import {
  Archive,
  ArchiveRestore,
  ChevronRight,
  EllipsisVertical,
  Pin,
  PinOff,
  Pencil,
} from "lucide-react";

import { useAppNavigation } from "@/app/navigation/useAppNavigation";
import { useAppShell } from "@/app/AppShellContext";
import type { ThreadActivityItem } from "@/features/channels/threadActivityStorage";
import { useCommunities } from "@/features/communities/useCommunities";
import { getThreadReference } from "@/features/messages/lib/threading";
import {
  resolveThreadDirectoryTitle,
  threadDirectoryUnreadState,
} from "@/features/sidebar/lib/threadDirectory";
import { useThreadDirectory } from "@/features/sidebar/useThreadDirectory";
import { formatRelativeTime } from "@/features/forum/lib/time";
import { useIdentityQuery } from "@/shared/api/hooks";
import type {
  ThreadDirectoryItem,
  ThreadDirectoryState,
} from "@/shared/api/threadDirectory";
import type { Channel } from "@/shared/api/types";
import { cn } from "@/shared/lib/cn";
import { Button } from "@/shared/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/shared/ui/dialog";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/shared/ui/dropdown-menu";
import { Input } from "@/shared/ui/input";
import { SidebarMenuAction } from "@/shared/ui/sidebar";

type RenameDraft = {
  item: ThreadDirectoryItem;
  title: string;
};

type LegacyThreadItem = {
  rootId: string;
  title: string;
  lastReplyAt: number;
};

/**
 * Degraded thread list for relays without the thread-directory server half.
 *
 * The only channel-scoped thread data the desktop holds locally is the
 * notification activity buffer, which is deliberately partial: replies the user
 * was notified about, capped across all channels. This list is therefore a
 * recent-activity view, not the complete set of threads — the empty state and
 * the section copy say so rather than claiming a channel has no threads.
 *
 * The title comes from the OLDEST known reply in each thread, not the newest:
 * the row navigates to the thread root, so a label that changes every time
 * somebody replies would not identify the thread it opens.
 */
export function legacySidebarThreadItems(
  activityItems: readonly ThreadActivityItem[],
  channelId: string,
): LegacyThreadItem[] {
  const byRootId = new Map<string, LegacyThreadItem & { titleAt: number }>();
  for (const item of activityItems) {
    if (item.channelId !== channelId) continue;
    const rootId = getThreadReference(item.tags).rootId;
    if (!rootId) continue;
    const current = byRootId.get(rootId);
    const title = item.content.trim().split(/\r?\n/, 1)[0] || "Thread";
    if (!current) {
      byRootId.set(rootId, {
        rootId,
        title,
        titleAt: item.createdAt,
        lastReplyAt: item.createdAt,
      });
      continue;
    }
    if (item.createdAt < current.titleAt) {
      current.title = title;
      current.titleAt = item.createdAt;
    }
    if (item.createdAt > current.lastReplyAt) {
      current.lastReplyAt = item.createdAt;
    }
  }
  return [...byRootId.values()]
    .map(({ rootId, title, lastReplyAt }) => ({ rootId, title, lastReplyAt }))
    .sort(
      (left, right) =>
        right.lastReplyAt - left.lastReplyAt ||
        left.rootId.localeCompare(right.rootId),
    );
}

export function LegacySidebarThreadList({
  items,
  onNavigate,
}: {
  items: LegacyThreadItem[];
  onNavigate: (rootId: string) => void;
}) {
  if (items.length === 0) {
    return (
      <p className="px-2 py-1 text-xs text-sidebar-foreground/55">
        No recent thread activity
      </p>
    );
  }
  return (
    <ul
      className="flex min-w-0 flex-col gap-0.5"
      data-testid="legacy-thread-list"
    >
      {items.map((item) => (
        <li key={item.rootId}>
          <button
            className="flex h-8 w-full min-w-0 items-center gap-1.5 rounded-md px-2 py-1 text-left text-xs text-sidebar-foreground/80 outline-hidden transition-colors hover:bg-sidebar-accent hover:text-sidebar-accent-foreground focus-visible:ring-2 focus-visible:ring-sidebar-ring"
            onClick={() => onNavigate(item.rootId)}
            type="button"
          >
            <span className="min-w-0 flex-1 truncate">{item.title}</span>
            <span
              className="shrink-0 text-3xs text-sidebar-foreground/45"
              title={new Date(item.lastReplyAt * 1_000).toLocaleString()}
            >
              {formatRelativeTime(item.lastReplyAt)}
            </span>
          </button>
        </li>
      ))}
    </ul>
  );
}

function activityAt(item: ThreadDirectoryItem) {
  return item.lastReplyAt || item.rootCreatedAt;
}

export function threadDirectoryActionPatch(
  action: "archive" | "pin" | "restore" | "unpin",
) {
  switch (action) {
    case "archive":
      return { archived: true, pinned: false } as const;
    case "pin":
      return { pinned: true } as const;
    case "restore":
      return { archived: false } as const;
    case "unpin":
      return { pinned: false } as const;
  }
}

export function threadDirectoryCanTogglePin(
  item: Pick<ThreadDirectoryItem, "archived">,
) {
  return !item.archived;
}

export function threadDirectoryRenameValidationMessage(value: string) {
  const title = value.trim();
  const scalars = Array.from(title);
  if (scalars.length === 0) return null;
  if (scalars.length > 120) {
    return "Thread names must be 120 characters or fewer.";
  }
  const hasInvalidCharacter = scalars.some((character) => {
    const codePoint = character.codePointAt(0);
    return (
      codePoint === undefined ||
      (codePoint >= 0xd800 && codePoint <= 0xdfff) ||
      codePoint <= 0x1f ||
      (codePoint >= 0x7f && codePoint <= 0x9f) ||
      codePoint === 0x2028 ||
      codePoint === 0x2029
    );
  });
  return hasInvalidCharacter
    ? "Thread names must be a single line without control characters."
    : null;
}

export function threadDirectoryRenameIsValid(value: string) {
  return (
    value.trim().length > 0 &&
    threadDirectoryRenameValidationMessage(value) === null
  );
}

export function threadDirectoryNavigationSearch(rootId: string) {
  return { messageId: rootId, threadRootId: rootId };
}

export function stopSidebarThreadInteraction(event: {
  stopPropagation: () => void;
}) {
  event.stopPropagation();
}

export function SidebarThreadDisclosure({ channel }: { channel: Channel }) {
  const [expanded, setExpanded] = React.useState(false);
  const contentId = `thread-directory-${channel.id}`;

  if (channel.channelType !== "stream") return null;

  return (
    <>
      <SidebarMenuAction
        aria-controls={contentId}
        aria-expanded={expanded}
        aria-label={`${expanded ? "Hide" : "Show"} threads for ${channel.name}`}
        data-testid={`thread-directory-disclosure-${channel.id}`}
        onClick={(event) => {
          stopSidebarThreadInteraction(event);
          setExpanded((current) => !current);
        }}
        onPointerDown={stopSidebarThreadInteraction}
        type="button"
      >
        <ChevronRight
          className={cn(
            "transition-transform",
            expanded ? "rotate-90" : "rotate-0",
          )}
        />
      </SidebarMenuAction>
      {expanded ? (
        <SidebarThreadList
          channelId={channel.id}
          channelName={channel.name}
          contentId={contentId}
        />
      ) : null}
    </>
  );
}

export function ThreadDirectoryRow({
  channelId,
  item,
  isUpdating,
  onNavigate,
  onRename,
  onUpdate,
}: {
  channelId: string;
  item: ThreadDirectoryItem;
  isUpdating: boolean;
  onNavigate: (rootId: string) => void;
  onRename: (item: ThreadDirectoryItem) => void;
  onUpdate: (
    item: ThreadDirectoryItem,
    action: "archive" | "pin" | "restore" | "unpin",
  ) => void;
}) {
  const { getThreadReadAt } = useAppShell();
  const title = resolveThreadDirectoryTitle(item);
  const unread = threadDirectoryUnreadState(
    item,
    () => getThreadReadAt(item.rootId, channelId),
    null,
  );

  return (
    <li
      className="group/thread-item relative min-w-0"
      data-testid="thread-directory-item"
    >
      <button
        className="flex h-8 w-full min-w-0 items-center gap-1.5 rounded-md py-1 pl-2 pr-8 text-left text-xs text-sidebar-foreground/80 outline-hidden transition-colors hover:bg-sidebar-accent hover:text-sidebar-accent-foreground focus-visible:ring-2 focus-visible:ring-sidebar-ring"
        onClick={() => onNavigate(item.rootId)}
        type="button"
      >
        {unread.isUnread ? (
          <span
            aria-label="Unread thread"
            className="h-1.5 w-1.5 shrink-0 rounded-full bg-primary"
            role="img"
          />
        ) : (
          <span aria-hidden="true" className="w-1.5 shrink-0" />
        )}
        <span className="min-w-0 flex-1 truncate">{title}</span>
        {item.pinned ? (
          <span className="shrink-0 text-3xs font-semibold uppercase tracking-wide text-primary">
            Pinned
          </span>
        ) : null}
        <span
          className="shrink-0 text-3xs text-sidebar-foreground/45"
          title={new Date(activityAt(item) * 1_000).toLocaleString()}
        >
          {formatRelativeTime(activityAt(item))}
        </span>
      </button>
      <DropdownMenu>
        <DropdownMenuTrigger asChild>
          <button
            aria-label={`More actions for ${title}`}
            className="absolute right-1 top-1 flex size-6 items-center justify-center rounded-md text-sidebar-foreground/50 outline-hidden transition-colors hover:bg-sidebar-accent hover:text-sidebar-foreground focus-visible:ring-2 focus-visible:ring-sidebar-ring"
            disabled={isUpdating}
            onClick={stopSidebarThreadInteraction}
            onPointerDown={stopSidebarThreadInteraction}
            type="button"
          >
            <EllipsisVertical className="size-3.5" />
          </button>
        </DropdownMenuTrigger>
        <DropdownMenuContent align="end">
          <DropdownMenuItem onSelect={() => onRename(item)}>
            <Pencil />
            <span>Rename thread</span>
          </DropdownMenuItem>
          {threadDirectoryCanTogglePin(item) ? (
            item.pinned ? (
              <DropdownMenuItem onSelect={() => onUpdate(item, "unpin")}>
                <PinOff />
                <span>Unpin thread</span>
              </DropdownMenuItem>
            ) : (
              <DropdownMenuItem onSelect={() => onUpdate(item, "pin")}>
                <Pin />
                <span>Pin thread</span>
              </DropdownMenuItem>
            )
          ) : null}
          {item.archived ? (
            <DropdownMenuItem onSelect={() => onUpdate(item, "restore")}>
              <ArchiveRestore />
              <span>Restore thread</span>
            </DropdownMenuItem>
          ) : (
            <DropdownMenuItem onSelect={() => onUpdate(item, "archive")}>
              <Archive />
              <span>Archive thread</span>
            </DropdownMenuItem>
          )}
        </DropdownMenuContent>
      </DropdownMenu>
    </li>
  );
}

export function ThreadDirectoryResults({
  channelId,
  directoryState,
  error,
  isPending,
  isUpdating,
  items,
  onNavigate,
  onRename,
  onUpdate,
}: {
  channelId: string;
  directoryState: ThreadDirectoryState;
  error: Error | null;
  isPending: boolean;
  isUpdating: boolean;
  items: ThreadDirectoryItem[];
  onNavigate: (rootId: string) => void;
  onRename: (item: ThreadDirectoryItem) => void;
  onUpdate: (
    item: ThreadDirectoryItem,
    action: "archive" | "pin" | "restore" | "unpin",
  ) => void;
}) {
  if (isPending) {
    return (
      <p className="px-2 py-1 text-xs text-sidebar-foreground/55">
        Loading threads…
      </p>
    );
  }
  if (error) {
    return (
      <p className="px-2 py-1 text-xs text-destructive" role="alert">
        {error.message || "Could not load threads."}
      </p>
    );
  }
  if (items.length === 0) {
    return (
      <p className="px-2 py-1 text-xs text-sidebar-foreground/55">
        No {directoryState} threads
      </p>
    );
  }
  return (
    <ul className="flex min-w-0 flex-col gap-0.5">
      {items.map((item) => (
        <ThreadDirectoryRow
          channelId={channelId}
          isUpdating={isUpdating}
          item={item}
          key={item.rootId}
          onNavigate={onNavigate}
          onRename={onRename}
          onUpdate={onUpdate}
        />
      ))}
    </ul>
  );
}

export function SidebarThreadList({
  channelId,
  channelName,
  contentId,
}: {
  channelId: string;
  channelName: string;
  contentId: string;
}) {
  const { activeCommunity } = useCommunities();
  const identityQuery = useIdentityQuery();
  const { goChannel } = useAppNavigation();
  const { threadActivityItems } = useAppShell();
  const [directoryState, setDirectoryState] =
    React.useState<ThreadDirectoryState>("active");
  const [renameDraft, setRenameDraft] = React.useState<RenameDraft | null>(
    null,
  );
  const [renameError, setRenameError] = React.useState<string | null>(null);
  const renameInputId = React.useId();
  const renameValidationId = React.useId();
  const directory = useThreadDirectory({
    channelId,
    communityId: activeCommunity?.id ?? null,
    relayUrl: activeCommunity?.relayUrl ?? null,
    pubkey: identityQuery.data?.pubkey ?? null,
    state: directoryState,
    enabled: true,
  });

  const handleUpdate = React.useCallback(
    (
      item: ThreadDirectoryItem,
      action: "archive" | "pin" | "restore" | "unpin",
    ) => {
      void directory
        .updateThread({
          rootId: item.rootId,
          patch: threadDirectoryActionPatch(action),
        })
        .catch(() => {});
    },
    [directory.updateThread],
  );

  const handleSaveRename = React.useCallback(() => {
    if (!renameDraft) return;
    const title = renameDraft.title.trim();
    if (!threadDirectoryRenameIsValid(title)) return;
    setRenameError(null);
    void directory
      .updateThread({
        rootId: renameDraft.item.rootId,
        patch: { title },
      })
      .then(() => setRenameDraft(null))
      .catch((error: unknown) => {
        setRenameError(
          error instanceof Error ? error.message : "Could not rename thread.",
        );
      });
  }, [directory.updateThread, renameDraft]);

  const handleClearRename = React.useCallback(() => {
    if (!renameDraft || renameDraft.item.titleOverride === null) return;
    setRenameError(null);
    void directory
      .updateThread({
        rootId: renameDraft.item.rootId,
        patch: { title: null },
      })
      .then(() => setRenameDraft(null))
      .catch((error: unknown) => {
        setRenameError(
          error instanceof Error
            ? error.message
            : "Could not restore the generated title.",
        );
      });
  }, [directory.updateThread, renameDraft]);

  const renameIsValid = threadDirectoryRenameIsValid(renameDraft?.title ?? "");
  const renameValidationMessage = threadDirectoryRenameValidationMessage(
    renameDraft?.title ?? "",
  );
  const legacyItems = React.useMemo(
    () => legacySidebarThreadItems(threadActivityItems, channelId),
    [channelId, threadActivityItems],
  );

  if (directory.isUnsupported) {
    return (
      <div
        className="ml-5 border-l border-sidebar-border/60 pl-2 pr-1 pt-0.5"
        id={contentId}
      >
        {/* Names the degraded list for what it is. The relay cannot serve the
            directory, so this shows local activity only — not every thread. */}
        <p className="px-2 pb-0.5 text-3xs text-sidebar-foreground/45">
          Recent activity
        </p>
        <LegacySidebarThreadList
          items={legacyItems}
          onNavigate={(rootId) => {
            void goChannel(channelId, threadDirectoryNavigationSearch(rootId));
          }}
        />
      </div>
    );
  }

  return (
    <div
      className="ml-5 border-l border-sidebar-border/60 pl-2 pr-1 pt-0.5"
      id={contentId}
    >
      <fieldset className="mb-1 flex gap-1">
        <legend className="sr-only">Thread views for {channelName}</legend>
        <Button
          aria-pressed={directoryState === "active"}
          onClick={() => setDirectoryState("active")}
          size="xs"
          type="button"
          variant={directoryState === "active" ? "secondary" : "ghost"}
        >
          Active threads
        </Button>
        <Button
          aria-pressed={directoryState === "archived"}
          onClick={() => setDirectoryState("archived")}
          size="xs"
          type="button"
          variant={directoryState === "archived" ? "secondary" : "ghost"}
        >
          Archived threads
        </Button>
      </fieldset>

      <div data-testid={`thread-directory-${directoryState}-${channelId}`}>
        <ThreadDirectoryResults
          channelId={channelId}
          directoryState={directoryState}
          error={directory.error}
          isPending={directory.isPending}
          isUpdating={directory.isUpdating}
          items={directory.items}
          onNavigate={(rootId) => {
            void goChannel(channelId, threadDirectoryNavigationSearch(rootId));
          }}
          onRename={(nextItem) => {
            setRenameError(null);
            setRenameDraft({
              item: nextItem,
              title: nextItem.titleOverride ?? "",
            });
          }}
          onUpdate={handleUpdate}
        />

        {directory.hasNextPage ? (
          <Button
            className="mt-1 w-full justify-start"
            disabled={directory.isFetchingNextPage}
            onClick={() => void directory.fetchNextPage()}
            size="xs"
            type="button"
            variant="ghost"
          >
            {directory.isFetchingNextPage ? "Loading…" : "More threads"}
          </Button>
        ) : null}

        {directory.updateError ? (
          <p className="px-2 py-1 text-xs text-destructive" role="alert">
            {directory.updateError.message || "Could not update thread."}
          </p>
        ) : null}
      </div>

      <Dialog
        onOpenChange={(open) => {
          if (!open) {
            setRenameDraft(null);
            setRenameError(null);
          }
        }}
        open={renameDraft !== null}
      >
        <DialogContent className="max-w-md">
          <DialogHeader>
            <DialogTitle>Rename thread</DialogTitle>
            <DialogDescription>
              This shared name is visible to everyone in the channel.
            </DialogDescription>
          </DialogHeader>
          <label className="space-y-1.5 text-sm" htmlFor={renameInputId}>
            <span>Thread name</span>
            <Input
              aria-describedby={
                renameValidationMessage ? renameValidationId : undefined
              }
              aria-invalid={renameValidationMessage ? true : undefined}
              id={renameInputId}
              onChange={(event) =>
                setRenameDraft((current) =>
                  current ? { ...current, title: event.target.value } : current,
                )
              }
              placeholder={renameDraft?.item.title ?? "Thread name"}
              value={renameDraft?.title ?? ""}
            />
          </label>
          {renameValidationMessage ? (
            <p
              className="text-sm text-destructive"
              id={renameValidationId}
              role="alert"
            >
              {renameValidationMessage}
            </p>
          ) : null}
          {renameError ? (
            <p className="text-sm text-destructive" role="alert">
              {renameError}
            </p>
          ) : null}
          {renameDraft?.item.titleOverride !== null ? (
            <Button
              disabled={directory.isUpdating}
              onClick={handleClearRename}
              type="button"
              variant="outline"
            >
              Use generated title
            </Button>
          ) : null}
          <DialogFooter>
            <Button
              onClick={() => setRenameDraft(null)}
              type="button"
              variant="ghost"
            >
              Cancel
            </Button>
            <Button
              disabled={!renameIsValid || directory.isUpdating}
              onClick={handleSaveRename}
              type="button"
            >
              Save
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}
