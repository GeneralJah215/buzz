import { normalizeRelayUrl } from "@/features/profile/lib/selfProfileStorage";
import type {
  UserProfileSummary,
  UsersBatchResponse,
} from "@/shared/api/types";
import { setLocalStorageItemWithRecovery } from "@/shared/lib/localStorageQuota";

const STORAGE_KEY_PREFIX = "buzz-user-labels.v1";
const MAX_CACHED_LABELS = 1_000;

type CachedUserLabel = {
  displayName: string | null;
  name: string | null;
  nip05Handle: string | null;
  updatedAt: number;
};

type UserLabelCache = {
  version: 1;
  profiles: Record<string, CachedUserLabel>;
};

export function userLabelCacheKey(relayUrl: string): string {
  return `${STORAGE_KEY_PREFIX}:${normalizeRelayUrl(relayUrl)}`;
}

function nullableString(value: unknown): string | null | undefined {
  if (value === null || value === undefined) return null;
  return typeof value === "string" ? value : undefined;
}

function parseCachedUserLabel(value: unknown): CachedUserLabel | null {
  if (typeof value !== "object" || value === null) return null;
  const raw = value as Record<string, unknown>;
  const displayName = nullableString(raw.displayName);
  const name = nullableString(raw.name);
  const nip05Handle = nullableString(raw.nip05Handle);
  if (
    displayName === undefined ||
    name === undefined ||
    nip05Handle === undefined
  ) {
    return null;
  }
  if (![displayName, name, nip05Handle].some((label) => label?.trim())) {
    return null;
  }
  return {
    displayName,
    name,
    nip05Handle,
    updatedAt:
      typeof raw.updatedAt === "number" && Number.isFinite(raw.updatedAt)
        ? raw.updatedAt
        : 0,
  };
}

// Memoised parse. readCache is reached from resolveUserLabelPlaceholderData,
// which React Query invokes on EVERY result computation — once per render per
// observer, and every rendered username is an observer. Each uncached call
// JSON.parses up to MAX_CACHED_LABELS entries and rebuilds the whole map. A
// live CPU profile of the minimized app put this function at the top of the JS
// self-time list, with the garbage collector second (BUG-076).
//
// The memo key is the RAW STRING, not a dirty flag. Any write invalidates it
// automatically — ours, another window's, or a devtools edit — because the
// string changes. A flag has to be remembered at every write site and goes
// stale the first time someone forgets one. A string compare cannot.
let memoKey: string | null = null;
let memoRaw: string | null = null;
let memoValue: UserLabelCache | null = null;

function readCache(relayUrl: string): UserLabelCache | null {
  try {
    const cacheKey = userLabelCacheKey(relayUrl);
    const raw = window.localStorage.getItem(cacheKey);
    if (!raw) return null;
    if (cacheKey === memoKey && raw === memoRaw) return memoValue;
    const parsed = JSON.parse(raw) as unknown;
    if (typeof parsed !== "object" || parsed === null) {
      return remember(cacheKey, raw, null);
    }
    const payload = parsed as Record<string, unknown>;
    if (
      payload.version !== 1 ||
      typeof payload.profiles !== "object" ||
      payload.profiles === null
    ) {
      return remember(cacheKey, raw, null);
    }

    const profiles: Record<string, CachedUserLabel> = {};
    for (const [pubkey, value] of Object.entries(
      payload.profiles as Record<string, unknown>,
    )) {
      const label = parseCachedUserLabel(value);
      if (label) profiles[pubkey.toLowerCase()] = label;
    }
    // Frozen because this object is now shared with every caller instead of
    // being rebuilt per call. An in-place mutation would silently corrupt every
    // later read; frozen, it throws at the mutation site instead.
    return remember(cacheKey, raw, {
      version: 1,
      profiles: Object.freeze(profiles),
    });
  } catch {
    return null;
  }
}

function remember(
  cacheKey: string,
  raw: string,
  value: UserLabelCache | null,
): UserLabelCache | null {
  memoKey = cacheKey;
  memoRaw = raw;
  // A malformed payload is memoised too. It sits on the same per-render path as
  // a good one, so re-parsing it every render would cost exactly as much.
  memoValue = value === null ? null : Object.freeze(value);
  return memoValue;
}

export function readCachedUserLabels(
  relayUrl: string,
  pubkeys: string[],
): UsersBatchResponse | undefined {
  const cache = readCache(relayUrl);
  if (!cache) return undefined;

  const profiles: UsersBatchResponse["profiles"] = {};
  for (const pubkey of pubkeys) {
    const normalizedPubkey = pubkey.toLowerCase();
    const cached = cache.profiles[normalizedPubkey];
    if (!cached) continue;
    profiles[normalizedPubkey] = {
      displayName: cached.displayName,
      name: cached.name,
      avatarUrl: null,
      nip05Handle: cached.nip05Handle,
      ownerPubkey: null,
    };
  }

  return Object.keys(profiles).length > 0
    ? { profiles, missing: [] }
    : undefined;
}

export function resolveUserLabelPlaceholderData(
  previousData: UsersBatchResponse | undefined,
  relayUrl: string,
  pubkeys: string[],
): UsersBatchResponse | undefined {
  return (
    previousData ??
    (relayUrl ? readCachedUserLabels(relayUrl, pubkeys) : undefined)
  );
}

export function writeCachedUserLabels(
  relayUrl: string,
  profiles: Record<string, UserProfileSummary>,
  missing: string[] = [],
): void {
  try {
    const now = Date.now();
    const merged = { ...(readCache(relayUrl)?.profiles ?? {}) };
    for (const [pubkey, profile] of Object.entries(profiles)) {
      const label = parseCachedUserLabel({
        displayName: profile.displayName,
        name: profile.name,
        nip05Handle: profile.nip05Handle,
        updatedAt: now,
      });
      const normalizedPubkey = pubkey.toLowerCase();
      if (label) {
        merged[normalizedPubkey] = label;
      } else {
        delete merged[normalizedPubkey];
      }
    }
    for (const pubkey of missing) {
      delete merged[pubkey.toLowerCase()];
    }

    const boundedProfiles = Object.fromEntries(
      Object.entries(merged)
        .sort(([, left], [, right]) => right.updatedAt - left.updatedAt)
        .slice(0, MAX_CACHED_LABELS),
    );
    setLocalStorageItemWithRecovery(
      userLabelCacheKey(relayUrl),
      JSON.stringify({
        version: 1,
        profiles: boundedProfiles,
      } satisfies UserLabelCache),
    );
  } catch {
    // Storage access failures are non-fatal.
  }
}

export function removeUserLabelCacheForRelay(relayUrl: string): void {
  try {
    window.localStorage.removeItem(userLabelCacheKey(relayUrl));
  } catch {
    // Storage access failures are non-fatal.
  }
}
