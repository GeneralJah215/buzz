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

/**
 * Most names kept per identity+relay, oldest evicted first.
 *
 * Two reasons for a hard cap. The whole map is one localStorage value that is
 * JSON-parsed and re-stringified on every rename, so an unbounded map turns
 * each keystroke-sized write into a linear cost over every name ever set. And
 * the fallback sidebar list is built from a *capped* activity buffer, so once
 * a thread ages out of that buffer its row disappears and the user can never
 * reach the rename control to delete its name by hand. Nothing else would
 * ever remove the entry.
 */
export const MAX_LOCAL_THREAD_NAMES = 500;

/**
 * Normalise a relay URL for this cache's storage key.
 *
 * DELIBERATELY DIFFERENT from the shared `normalizeRelayUrl` in
 * `features/profile/lib/selfProfileStorage`, which lowercases the entire URL.
 * A relay URL may carry a path (`wss://relay.example.com/Alice`), and RFC 3986
 * paths are case-sensitive: lowercasing the whole URL merges two genuinely
 * different relays into one bucket, and the names set against one then show up
 * against the other. Here only the scheme and authority — the parts that
 * really are case-insensitive — are lowercased, and the path is left alone.
 *
 * The shared helper is not changed because its output is baked into storage
 * keys other features already wrote; altering it would silently move their
 * data. This local copy is the safe place to be stricter.
 */
export function normalizeLocalThreadNameRelay(relayUrl: string): string {
  const trimmed = relayUrl.trim().replace(/\/+$/, "");
  if (trimmed.length === 0) return "";
  // The path begins at the first "/" after the "scheme://" prefix, if any.
  const schemeEnd = trimmed.indexOf("://");
  const authorityStart = schemeEnd >= 0 ? schemeEnd + 3 : 0;
  const pathStart = trimmed.indexOf("/", authorityStart);
  if (pathStart < 0) return trimmed.toLowerCase();
  return trimmed.slice(0, pathStart).toLowerCase() + trimmed.slice(pathStart);
}

export function localThreadNamesKey(pubkey: string, relayUrl: string): string {
  return `${STORAGE_PREFIX}:${normalizeLocalThreadNameRelay(relayUrl)}:${pubkey}`;
}

/**
 * The storage key for a scope, or null when the scope cannot address one.
 *
 * The guard runs on the NORMALISED relay, not the raw string. `"/"`, `"///"`,
 * `"   "` and `"\t"` are all truthy but normalise to the empty string, so a
 * raw-string guard lets every one of them through to the same
 * `prefix::pubkey` key — a shared bucket where unrelated sessions read and
 * overwrite each other's names.
 */
function localThreadNamesScopeKey(
  pubkey: string | null,
  relayUrl: string | null,
): string | null {
  if (!pubkey || pubkey.trim().length === 0) return null;
  if (!relayUrl) return null;
  const relay = normalizeLocalThreadNameRelay(relayUrl);
  if (relay.length === 0) return null;
  return `${STORAGE_PREFIX}:${relay}:${pubkey}`;
}

/** rootId → display name, the shape the sidebar renders from. */
type NameMap = Record<string, string>;

/** What is actually stored: the name plus when it was last written. */
type StoredEntry = { name: string; updatedAt: number };
type StoredMap = Record<string, StoredEntry>;

/**
 * Parse the stored payload, accepting both the current record form and the
 * original bare-string form (`{rootId: "name"}`).
 *
 * Migration happens on read so names written before the cap existed survive
 * the upgrade. They get `updatedAt: 0`, which sorts them oldest — correct,
 * since their real write time was not recorded and they are by definition
 * older than anything this build has written.
 */
function parseStoredMap(parsed: unknown): StoredMap {
  if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) return {};
  const map: StoredMap = {};
  for (const [rootId, value] of Object.entries(
    parsed as Record<string, unknown>,
  )) {
    if (typeof value === "string") {
      if (value.length > 0) map[rootId] = { name: value, updatedAt: 0 };
      continue;
    }
    if (!value || typeof value !== "object" || Array.isArray(value)) continue;
    const entry = value as Partial<StoredEntry>;
    if (typeof entry.name !== "string" || entry.name.length === 0) continue;
    map[rootId] = {
      name: entry.name,
      updatedAt:
        typeof entry.updatedAt === "number" && Number.isFinite(entry.updatedAt)
          ? entry.updatedAt
          : 0,
    };
  }
  return map;
}

/**
 * Read the raw entries for a scope. A corrupt or absent value yields `{}`
 * rather than throwing: a broken cache must not stop the sidebar rendering.
 */
function readStoredMap(key: string): StoredMap {
  try {
    const raw = window.localStorage.getItem(key);
    if (!raw) return {};
    return parseStoredMap(JSON.parse(raw) as unknown);
  } catch {
    return {};
  }
}

function toNameMap(stored: StoredMap): NameMap {
  const map: NameMap = {};
  for (const [rootId, entry] of Object.entries(stored))
    map[rootId] = entry.name;
  return map;
}

/**
 * Drop the oldest entries until the map fits the cap.
 *
 * Ties break on rootId so eviction is deterministic rather than dependent on
 * object key order.
 */
function evictOldest(stored: StoredMap): StoredMap {
  const rootIds = Object.keys(stored);
  if (rootIds.length <= MAX_LOCAL_THREAD_NAMES) return stored;
  const ordered = rootIds.sort(
    (left, right) =>
      stored[left].updatedAt - stored[right].updatedAt ||
      left.localeCompare(right),
  );
  const next: StoredMap = { ...stored };
  for (const rootId of ordered.slice(
    0,
    rootIds.length - MAX_LOCAL_THREAD_NAMES,
  )) {
    delete next[rootId];
  }
  return next;
}

/**
 * Read the whole map. A corrupt or absent entry yields `{}` rather than
 * throwing: a broken cache must not stop the sidebar rendering.
 */
export function loadLocalThreadNames(
  pubkey: string | null,
  relayUrl: string | null,
): NameMap {
  const key = localThreadNamesScopeKey(pubkey, relayUrl);
  if (!key) return {};
  return toNameMap(readStoredMap(key));
}

/**
 * Set or clear one thread's local name.
 *
 * An empty or whitespace-only name clears the entry rather than storing a
 * blank, so "rename to nothing" restores the derived title instead of showing
 * an empty row. Returns the updated map so callers can re-render without a
 * second read.
 *
 * Validation lives HERE, not only on the dialog's Save button. A disabled
 * button is a UI hint, not a guard: the guard belongs at the action layer, so
 * any caller — a future keyboard shortcut, a restored draft, a test — is held
 * to the same rule. An invalid name leaves the map untouched.
 */
export function setLocalThreadName(
  pubkey: string | null,
  relayUrl: string | null,
  rootId: string,
  name: string | null,
): NameMap {
  const key = localThreadNamesScopeKey(pubkey, relayUrl);
  if (!key) return {};
  const current = readStoredMap(key);
  if (!rootId) return toNameMap(current);

  const trimmed = name?.trim() ?? "";
  let next: StoredMap = { ...current };
  if (trimmed.length === 0) {
    delete next[rootId];
  } else {
    // Character rules are checked on the full string: truncating first would
    // let a control character past the guard just by sitting beyond the cap.
    if (!localThreadNameCharactersAreValid(trimmed)) return toNameMap(current);
    // Code points, not UTF-16 units. `slice` would cut an emoji in half and
    // store an unpaired surrogate, which then fails the shared validator and
    // renders as a replacement glyph.
    const capped = Array.from(trimmed)
      .slice(0, MAX_LOCAL_THREAD_NAME_LENGTH)
      .join("");
    // Strictly newer than everything already stored, even when two renames
    // land inside the same clock millisecond, so eviction order is total.
    const updatedAt = Math.max(
      Date.now(),
      ...Object.values(current).map((entry) => entry.updatedAt + 1),
    );
    next[rootId] = { name: capped, updatedAt };
    next = evictOldest(next);
  }

  try {
    if (Object.keys(next).length === 0) {
      window.localStorage.removeItem(key);
    } else {
      window.localStorage.setItem(key, JSON.stringify(next));
    }
  } catch {
    // Quota or private-mode failure. The in-memory result is still returned so
    // the rename appears this session; losing it on reload beats a hard error.
  }
  return toNameMap(next);
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

/**
 * The character rule, kept byte-for-byte in step with
 * `threadDirectoryRenameValidationMessage` in SidebarThreadList.
 *
 * A local name is meant to be promotable to a shared one the day the relay
 * grows thread-directory support, so anything this accepts the shared
 * validator must accept too. The earlier `code < 0x20 || code === 0x7f` test
 * let U+2028, U+2029, U+0085, the C1 range, and lone surrogates through.
 */
function localThreadNameCharactersAreValid(value: string): boolean {
  return !Array.from(value).some((character) => {
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
}

/** Reject control characters and multi-line input, matching the shared rename. */
export function localThreadNameIsValid(value: string): boolean {
  const trimmed = value.trim();
  if (trimmed.length === 0) return true; // clearing is always valid
  // Code points, matching the shared validator: `.length` counts UTF-16 units,
  // so 100 typed emoji measure 200 and the user is told they exceeded 120.
  if (Array.from(trimmed).length > MAX_LOCAL_THREAD_NAME_LENGTH) return false;
  return localThreadNameCharactersAreValid(trimmed);
}
