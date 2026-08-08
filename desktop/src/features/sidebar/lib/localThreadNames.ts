import { normalizeRelayUrl } from "@/features/profile/lib/selfProfileStorage";

/**
 * Machine-local custom names for threads.
 *
 * Shared thread names live on the relay as a kind-39007 state event, which
 * only relays running the thread-directory server half accept. On a relay
 * without it the sidebar falls back to a plain activity list with no rename
 * at all, so there is no way to label a thread.
 *
 * These names fill that gap. They are visible only on this machine and are
 * never published. When the relay grows thread-directory support, a shared
 * rename supersedes the local one — {@link resolveLocalThreadName} prefers the
 * shared title, so promoting a name is a no-op rather than a conflict.
 *
 * Scoped by relay and identity, matching the other local caches: a name set
 * while signed in as one identity must not leak into another's sidebar.
 */
const STORAGE_PREFIX = "buzz-local-thread-names.v1";

/** Same bound the shared rename enforces, so a local name can be promoted. */
export const MAX_LOCAL_THREAD_NAME_LENGTH = 120;

export function localThreadNamesKey(pubkey: string, relayUrl: string): string {
  return `${STORAGE_PREFIX}:${normalizeRelayUrl(relayUrl)}:${pubkey}`;
}

type NameMap = Record<string, string>;

/**
 * Read the whole map. A corrupt or absent entry yields `{}` rather than
 * throwing: a broken cache must not stop the sidebar rendering.
 */
export function loadLocalThreadNames(
  pubkey: string | null,
  relayUrl: string | null,
): NameMap {
  if (!pubkey || !relayUrl) return {};
  try {
    const raw = window.localStorage.getItem(
      localThreadNamesKey(pubkey, relayUrl),
    );
    if (!raw) return {};
    const parsed: unknown = JSON.parse(raw);
    if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
      return {};
    }
    const map: NameMap = {};
    for (const [rootId, name] of Object.entries(parsed as NameMap)) {
      if (typeof name === "string" && name.length > 0) map[rootId] = name;
    }
    return map;
  } catch {
    return {};
  }
}

/**
 * Set or clear one thread's local name.
 *
 * An empty or whitespace-only name clears the entry rather than storing a
 * blank, so "rename to nothing" restores the derived title instead of showing
 * an empty row. Returns the updated map so callers can re-render without a
 * second read.
 */
export function setLocalThreadName(
  pubkey: string | null,
  relayUrl: string | null,
  rootId: string,
  name: string | null,
): NameMap {
  const current = loadLocalThreadNames(pubkey, relayUrl);
  if (!pubkey || !relayUrl) return current;

  const trimmed = name?.trim() ?? "";
  const next: NameMap = { ...current };
  if (trimmed.length === 0) {
    delete next[rootId];
  } else {
    next[rootId] = trimmed.slice(0, MAX_LOCAL_THREAD_NAME_LENGTH);
  }

  try {
    const key = localThreadNamesKey(pubkey, relayUrl);
    if (Object.keys(next).length === 0) {
      window.localStorage.removeItem(key);
    } else {
      window.localStorage.setItem(key, JSON.stringify(next));
    }
  } catch {
    // Quota or private-mode failure. The in-memory result is still returned so
    // the rename appears this session; losing it on reload beats a hard error.
  }
  return next;
}

/**
 * Pick the name to display for a thread.
 *
 * A shared name always wins. It is the one everyone in the channel sees, so
 * letting a stale local override mask it would show this machine something
 * different from every other member.
 */
export function resolveLocalThreadName(
  sharedTitle: string | null,
  localName: string | undefined,
  fallbackTitle: string,
): string {
  if (sharedTitle && sharedTitle.trim().length > 0) return sharedTitle;
  if (localName && localName.trim().length > 0) return localName;
  return fallbackTitle;
}

/** Reject control characters and multi-line input, matching the shared rename. */
export function localThreadNameIsValid(value: string): boolean {
  const trimmed = value.trim();
  if (trimmed.length === 0) return true; // clearing is always valid
  if (trimmed.length > MAX_LOCAL_THREAD_NAME_LENGTH) return false;
  return ![...trimmed].some((character) => {
    const code = character.codePointAt(0) ?? 0;
    return code < 0x20 || code === 0x7f;
  });
}
