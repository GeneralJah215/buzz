import { invokeTauri, signRelayEvent } from "@/shared/api/tauri";
import { relayClient } from "@/shared/api/relayClient";
import type { RelayEvent } from "@/shared/api/types";
import {
  KIND_THREAD_DIRECTORY_BOUNDS,
  KIND_THREAD_DIRECTORY_ITEM,
  KIND_THREAD_DIRECTORY_STATE,
} from "@/shared/constants/kinds";

export type ThreadDirectoryState = "active" | "archived";

export type ThreadDirectoryItem = {
  rootId: string;
  channelId: string;
  title: string;
  titleOverride: string | null;
  rootAuthor: string;
  rootCreatedAt: number;
  replyCount: number;
  descendantCount: number;
  lastReplyAt: number;
  participants: string[];
  pinned: boolean;
  archived: boolean;
  present: boolean;
  stateCreatedAt: number;
  stateEventId: string | null;
  projectionCreatedAt: number;
  projectionEventId: string;
};

export type ThreadDirectoryBounds = {
  channelId: string;
  state: ThreadDirectoryState;
  hasMore: boolean;
  nextCursor: string | null;
};

export type ThreadDirectoryPage = {
  items: ThreadDirectoryItem[];
  bounds: ThreadDirectoryBounds;
};

export type ThreadDirectoryStateSnapshot = {
  title: string | null;
  pinned: boolean;
  archived: boolean;
};

/** The connected relay cannot serve the thread-directory protocol. */
export class ThreadDirectoryUnsupportedError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "ThreadDirectoryUnsupportedError";
  }
}

export function isThreadDirectoryUnsupportedError(
  error: unknown,
): error is ThreadDirectoryUnsupportedError {
  return error instanceof ThreadDirectoryUnsupportedError;
}

function isNonNegativeInteger(value: unknown): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
}

function asRecord(value: unknown, label: string): Record<string, unknown> {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    throw new Error(`Thread directory ${label} must be an object.`);
  }
  return value as Record<string, unknown>;
}

function parseJson(content: string, label: string): Record<string, unknown> {
  try {
    return asRecord(JSON.parse(content), label);
  } catch {
    throw new Error(`Thread directory ${label} contains invalid JSON.`);
  }
}

function exactlyOneTagValue(
  event: RelayEvent,
  name: string,
  label: string,
): string {
  const tags = event.tags.filter((tag) => tag[0] === name);
  if (
    tags.length !== 1 ||
    tags[0].length !== 2 ||
    typeof tags[0][1] !== "string" ||
    !tags[0][1]
  ) {
    throw new Error(
      `Thread directory ${label} must have exactly one ${name} tag.`,
    );
  }
  return tags[0][1];
}

function isCanonicalHexIdentity(value: unknown): value is string {
  return typeof value === "string" && /^[0-9a-f]{64}$/.test(value);
}

function requireExactTagNames(
  event: RelayEvent,
  expectedNames: readonly string[],
  label: string,
) {
  if (
    event.tags.length !== expectedNames.length ||
    event.tags.some((tag) => !expectedNames.includes(tag[0]))
  ) {
    throw new Error(
      `Thread directory ${label} must contain only canonical tags.`,
    );
  }
}

function parseItem(event: RelayEvent, channelId: string): ThreadDirectoryItem {
  if (event.kind !== KIND_THREAD_DIRECTORY_ITEM) {
    throw new Error(
      "Thread directory response contains an unexpected item kind.",
    );
  }
  if (
    !isCanonicalHexIdentity(event.id) ||
    !isNonNegativeInteger(event.created_at)
  ) {
    throw new Error("Thread directory item has invalid projection metadata.");
  }
  const rootId = exactlyOneTagValue(event, "e", "item");
  if (!isCanonicalHexIdentity(rootId)) {
    throw new Error("Thread directory item has an invalid root id.");
  }
  if (exactlyOneTagValue(event, "d", "item") !== rootId) {
    throw new Error("Thread directory item root tags do not match.");
  }
  if (exactlyOneTagValue(event, "h", "item") !== channelId) {
    throw new Error("Thread directory item is scoped to another channel.");
  }
  requireExactTagNames(event, ["e", "d", "h"], "item");

  const content = parseJson(event.content, "item");
  const stringFields = ["title", "root_author"] as const;
  for (const field of stringFields) {
    if (typeof content[field] !== "string" || content[field].length === 0) {
      throw new Error(`Thread directory item has an invalid ${field}.`);
    }
  }
  if (!isCanonicalHexIdentity(content.root_author)) {
    throw new Error("Thread directory item has an invalid root_author.");
  }
  if (
    content.title_override !== null &&
    (typeof content.title_override !== "string" ||
      content.title_override.length === 0)
  ) {
    throw new Error("Thread directory item has an invalid title override.");
  }
  const numberFields = [
    "root_created_at",
    "reply_count",
    "descendant_count",
    "last_reply_at",
    "state_created_at",
  ] as const;
  for (const field of numberFields) {
    if (!isNonNegativeInteger(content[field])) {
      throw new Error(`Thread directory item has an invalid ${field}.`);
    }
  }
  if (
    typeof content.pinned !== "boolean" ||
    typeof content.archived !== "boolean" ||
    (content.pinned === true && content.archived === true)
  ) {
    throw new Error("Thread directory item has invalid shared state.");
  }
  if (content.present !== undefined && typeof content.present !== "boolean") {
    throw new Error("Thread directory item has an invalid present value.");
  }
  if (
    !Array.isArray(content.participants) ||
    content.participants.some(
      (participant) => !isCanonicalHexIdentity(participant),
    )
  ) {
    throw new Error("Thread directory item has invalid participants.");
  }
  if (
    content.state_event_id !== null &&
    !isCanonicalHexIdentity(content.state_event_id)
  ) {
    throw new Error("Thread directory item has an invalid state event id.");
  }
  if ((content.state_created_at === 0) !== (content.state_event_id === null)) {
    throw new Error(
      "Thread directory item has inconsistent state revision metadata.",
    );
  }

  return {
    rootId,
    channelId,
    title: content.title as string,
    titleOverride: content.title_override as string | null,
    rootAuthor: content.root_author as string,
    rootCreatedAt: content.root_created_at as number,
    replyCount: content.reply_count as number,
    descendantCount: content.descendant_count as number,
    lastReplyAt: content.last_reply_at as number,
    participants: content.participants as string[],
    pinned: content.pinned as boolean,
    archived: content.archived as boolean,
    present: (content.present as boolean | undefined) ?? true,
    stateCreatedAt: content.state_created_at as number,
    stateEventId: content.state_event_id as string | null,
    projectionCreatedAt: event.created_at,
    projectionEventId: event.id,
  };
}

/** Parse one live directory-item overlay without accepting timeline rows. */
export function parseThreadDirectoryItemOverlay(
  event: RelayEvent,
  channelId: string,
): ThreadDirectoryItem {
  return parseItem(event, channelId);
}

function parseBounds(
  event: RelayEvent,
  channelId: string,
  state: ThreadDirectoryState,
  cursor: string | null,
): ThreadDirectoryBounds {
  if (event.kind !== KIND_THREAD_DIRECTORY_BOUNDS) {
    throw new Error(
      "Thread directory response contains an unexpected bounds kind.",
    );
  }
  if (exactlyOneTagValue(event, "h", "bounds") !== channelId) {
    throw new Error("Thread directory bounds are scoped to another channel.");
  }
  const requestTag = exactlyOneTagValue(event, "d", "bounds");
  if (requestTag !== `${channelId}:${state}:${cursor ?? "head"}`) {
    throw new Error("Thread directory bounds do not match the requested page.");
  }
  requireExactTagNames(event, ["d", "h"], "bounds");
  const content = parseJson(event.content, "bounds");
  if (typeof content.has_more !== "boolean") {
    throw new Error("Thread directory bounds have an invalid has_more value.");
  }
  if (
    content.next_cursor !== null &&
    (typeof content.next_cursor !== "string" ||
      content.next_cursor.length === 0)
  ) {
    throw new Error("Thread directory bounds have an invalid next cursor.");
  }
  if (
    (content.has_more && content.next_cursor === null) ||
    (!content.has_more && content.next_cursor !== null)
  ) {
    throw new Error("Thread directory bounds have an inconsistent cursor.");
  }
  return {
    channelId,
    state,
    hasMore: content.has_more,
    nextCursor: content.next_cursor,
  };
}

/** Parse one flat bridge response into directory items and its required bounds. */
export function parseThreadDirectoryPage(
  events: RelayEvent[],
  channelId: string,
  state: ThreadDirectoryState,
  cursor: string | null = null,
): ThreadDirectoryPage {
  const items: ThreadDirectoryItem[] = [];
  const bounds: RelayEvent[] = [];
  for (const event of events) {
    if (event.kind === KIND_THREAD_DIRECTORY_ITEM) {
      items.push(parseItem(event, channelId));
    } else if (event.kind === KIND_THREAD_DIRECTORY_BOUNDS) {
      bounds.push(event);
    } else {
      throw new Error(
        "Thread directory response contains an unexpected event kind.",
      );
    }
  }
  if (bounds.length === 0) {
    throw new ThreadDirectoryUnsupportedError(
      "The relay does not support the thread directory.",
    );
  }
  if (bounds.length !== 1) {
    throw new Error(
      "Thread directory response must contain exactly one bounds overlay.",
    );
  }
  return { items, bounds: parseBounds(bounds[0], channelId, state, cursor) };
}

/** Fetch a lazy, relay-authoritative directory page for one channel. */
export async function getThreadDirectoryPage(
  channelId: string,
  state: ThreadDirectoryState,
  cursor: string | null = null,
  limitRows = 25,
): Promise<ThreadDirectoryPage> {
  let events: RelayEvent[];
  try {
    events = await invokeTauri<RelayEvent[]>("get_thread_directory", {
      channelId,
      directoryState: state,
      cursor,
      limitRows,
    });
  } catch {
    throw new ThreadDirectoryUnsupportedError(
      "The relay does not support the thread directory.",
    );
  }
  return parseThreadDirectoryPage(events, channelId, state, cursor);
}

/** Publish the complete signed shared-state snapshot for a directory root. */
export async function publishThreadDirectoryState(
  channelId: string,
  rootId: string,
  snapshot: ThreadDirectoryStateSnapshot,
): Promise<RelayEvent> {
  const event = await signRelayEvent({
    kind: KIND_THREAD_DIRECTORY_STATE,
    content: JSON.stringify(snapshot),
    tags: [
      ["h", channelId],
      ["e", rootId, "", "root"],
    ],
  });
  return relayClient.publishEvent(
    event,
    "Timed out publishing thread directory state.",
    "Failed to publish thread directory state.",
  );
}
