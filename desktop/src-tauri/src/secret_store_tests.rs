//! Tests for `secret_store.rs`, split out to keep that file under the
//! repo's file-size ratchet. Mirrors the existing `app_state_tests.rs`.

use super::*;

// Test-only constructor: pre-seed the cache without touching the OS keychain.
impl SecretStore {
    fn with_cache(service: &str, cache: Option<HashMap<String, String>>) -> Self {
        SecretStore {
            service: service.to_string(),
            cache: Mutex::new(cache),
        }
    }
}

// ── Chunked blob layout (BUG-013) ──────────────────────────────────────
//
// Windows Credential Manager caps one credential at 2560 UTF-16 chars, so
// the single-entry secret store stopped accepting writes at roughly 18
// agents — taking the user's identity down with it, because it shares the
// entry. These cover the split/reassemble logic and the compatibility rule
// that keeps an existing single-entry blob readable.

/// A realistic blob: one identity plus `agents` agent keys, same shape as
/// the real store (64-hex names, 63-char bech32 secrets).
fn sample_blob(agents: usize) -> String {
    let mut map = HashMap::new();
    map.insert("identity".to_string(), format!("nsec1{}", "0".repeat(58)));
    for index in 0..agents {
        map.insert(
            format!("agent:{:064x}", index),
            format!("nsec1{:058x}", index),
        );
    }
    serde_json::to_string(&map).unwrap()
}

/// The budget the Windows backend actually enforces:
/// `encode_utf16().count() * 2 > CRED_MAX_CREDENTIAL_BLOB_SIZE`, where that
/// constant is 2560 BYTES (`keyring-3.6.3/src/windows.rs:224`). The error
/// text quotes 2560 and says "chars", which is what made the first attempt
/// at this fix pick a chunk size that still failed every write.
const WINDOWS_CRED_MAX_UTF16_UNITS: usize = 2560 / 2;

fn windows_would_accept(value: &str) -> bool {
    value.encode_utf16().count() * 2 <= 2560
}

#[test]
fn every_chunk_is_actually_writable_by_the_windows_backend() {
    // The test that had to exist. A chunk size chosen from the error
    // message rather than the enforced rule produces chunks that are
    // rejected, reproducing the original bug with extra steps.
    assert!(
        MAX_ENTRY_UTF16 <= WINDOWS_CRED_MAX_UTF16_UNITS,
        "chunk budget {MAX_ENTRY_UTF16} exceeds the enforced limit of \
         {WINDOWS_CRED_MAX_UTF16_UNITS} UTF-16 units"
    );
    for agents in [0, 1, 9, 18, 35, 200] {
        let blob = sample_blob(agents);
        for part in split_utf16(&blob, MAX_ENTRY_UTF16) {
            assert!(
                windows_would_accept(part),
                "{agents} agents produced a chunk Windows would reject"
            );
        }
    }
}

#[test]
fn the_real_store_size_that_broke_windows_needs_more_than_one_entry() {
    // 17 agent keys plus an identity is what was on the affected machine.
    let blob = sample_blob(17);
    assert!(!windows_would_accept(&blob), "must not fit one credential");
    let parts = split_utf16(&blob, MAX_ENTRY_UTF16);
    assert!(parts.len() > 1);
    assert_eq!(parts.concat(), blob, "split must round-trip exactly");
}

#[test]
fn the_true_ceiling_was_about_nine_agents_not_eighteen() {
    // Documents the real severity: the enforced budget is half what the
    // error message implies, so the store broke far earlier than it looked.
    let mut last_ok = 0;
    for agents in 0..40 {
        if windows_would_accept(&sample_blob(agents)) {
            last_ok = agents;
        } else {
            break;
        }
    }
    assert!(
        (5..=12).contains(&last_ok),
        "expected the single-entry ceiling near 9 agents, measured {last_ok}"
    );
}

#[test]
fn a_small_blob_still_fits_one_entry() {
    // Two agents is well under the cap, so nothing about the storage layout
    // changes for ordinary users — and macOS keeps one ACL prompt.
    let blob = sample_blob(2);
    assert!(utf16_len(&blob) <= MAX_ENTRY_UTF16);
    assert!(windows_would_accept(&blob));
}

#[test]
fn split_round_trips_and_respects_the_budget() {
    for agents in [0, 1, 18, 35, 200] {
        let blob = sample_blob(agents);
        let parts = split_utf16(&blob, MAX_ENTRY_UTF16);
        assert_eq!(parts.concat(), blob, "{agents} agents");
        for part in &parts {
            assert!(utf16_len(part) <= MAX_ENTRY_UTF16, "{agents} agents");
        }
    }
}

#[test]
fn split_budgets_surrogate_pairs_as_two_units() {
    // An astral-plane character costs 2 UTF-16 units. Counting characters
    // instead would emit chunks at twice the budget — silently rejected.
    let value = "𝄞".repeat(MAX_ENTRY_UTF16 * 2);
    let parts = split_utf16(&value, MAX_ENTRY_UTF16);
    assert_eq!(parts.concat(), value);
    for part in &parts {
        assert!(utf16_len(part) <= MAX_ENTRY_UTF16);
        assert!(windows_would_accept(part));
    }
    // And a 2-unit character is never cut in half.
    assert!(parts.iter().all(|p| p.chars().all(|c| c == '𝄞')));
}

#[test]
fn a_torn_read_is_detected_even_when_the_length_matches() {
    // Swapping one secret for another of the same length leaves the total
    // length unchanged, so length alone cannot detect a read that straddled
    // two writes. The digest can.
    let before = sample_blob(20);
    let after = before.replacen("nsec10", "nsec1f", 1);
    assert_eq!(before.len(), after.len(), "lengths must collide for this test");
    assert_ne!(payload_digest(&before), payload_digest(&after));
}

#[test]
fn a_corrupt_header_cannot_drive_a_huge_allocation() {
    let absurd = format!(
        r#"{{"v":2,"gen":"a","chunks":1,"len":{},"digest":1}}"#,
        u64::MAX
    );
    assert!(ChunkHeader::parse(&absurd).is_none());
    let too_many = r#"{"v":2,"gen":"a","chunks":99999999,"len":10,"digest":1}"#;
    assert!(ChunkHeader::parse(too_many).is_none());
}

#[test]
fn an_existing_single_entry_blob_is_not_mistaken_for_a_header() {
    // The upgrade must read a store written by the old code with no
    // migration step, so a plain JSON map has to parse as "not a header".
    assert!(ChunkHeader::parse(&sample_blob(3)).is_none());
    assert!(ChunkHeader::parse("{}").is_none());
    assert!(ChunkHeader::parse("not json").is_none());
    // A future version must not be read with today's rules.
    assert!(ChunkHeader::parse(r#"{"v":3,"gen":"a","chunks":1,"len":5,"digest":1}"#).is_none());
    // Malformed headers are rejected rather than half-trusted.
    assert!(ChunkHeader::parse(r#"{"v":2,"gen":"z","chunks":1,"len":5,"digest":1}"#).is_none());
    assert!(ChunkHeader::parse(r#"{"v":2,"gen":"a","chunks":1,"digest":1}"#).is_none());
    assert!(ChunkHeader::parse(r#"{"v":2,"gen":"a","chunks":1,"len":5}"#).is_none());
}

#[test]
fn header_round_trips_through_the_exact_string_the_writer_emits() {
    let written = ChunkHeader {
        generation: Generation::B,
        chunks: 3,
        len: 5_000,
        digest: 0xdead_beef,
    }
    .render();
    let header = ChunkHeader::parse(&written).expect("writer output must parse");
    assert_eq!(header.generation, Generation::B);
    assert_eq!(header.chunks, 3);
    assert_eq!(header.len, 5_000);
    assert_eq!(header.digest, 0xdead_beef);
    // A header is always writable in one credential, which is what makes
    // the generation flip atomic.
    assert!(windows_would_accept(&written));
}

#[test]
fn writes_alternate_generations_so_the_live_one_is_never_overwritten() {
    assert_eq!(Generation::A.other(), Generation::B);
    assert_eq!(Generation::B.other(), Generation::A);
    assert_eq!(chunk_key(Generation::A, 0), "secrets.a.0");
    assert_eq!(chunk_key(Generation::B, 12), "secrets.b.12");
    // Chunk names must never collide with the header entry.
    assert_ne!(chunk_key(Generation::A, 0), BLOB_KEY);
}

#[test]
fn probe_returns_present_when_key_in_cache() {
    let mut map = HashMap::new();
    map.insert("identity".to_string(), "nsec1test".to_string());
    let store = SecretStore::with_cache("buzz-test-cache-hit", Some(map));
    // Cache is warm and contains "identity" — probe must return Present
    // without touching the keychain.
    assert_eq!(store.probe("identity"), KeyringProbe::Present);
}

#[test]
fn load_returns_value_when_key_in_cache() {
    let mut map = HashMap::new();
    map.insert("identity".to_string(), "nsec1test".to_string());
    let store = SecretStore::with_cache("buzz-test-load-cache-hit", Some(map));
    // Cache is warm and contains "identity" — load must return the value
    // without touching the keychain.
    assert_eq!(
        store.load("identity").unwrap(),
        Some("nsec1test".to_string())
    );
}

// ── Cross-process race tests (require real OS keychain) ────────────────

#[ignore = "requires real OS keychain (run locally)"]
#[test]
fn test_stale_warm_cache_add_observes_prior_write() {
    // Simulates the cross-process race that stranded Will's agent keys.
    //
    // Setup: two SecretStore instances for the same service (= two
    // "processes" with separate caches). Process A warms its cache to
    // {k1}. Process B then writes {k1, k2}. Without the fix, A's next
    // mutate_blob would build from its stale {k1} cache and write
    // {k1, k3}, silently dropping k2. With the fix, A always re-reads
    // from the keychain inside the lock, so the result is {k1, k2, k3}.
    let svc = "buzz-test-race-stale-cache";

    // Clean state.
    let setup = SecretStore::keyring(svc);
    let _ = setup.delete("k1");
    let _ = setup.delete("k2");
    let _ = setup.delete("k3");

    // Process A: write k1, warming its cache.
    let store_a = SecretStore::keyring(svc);
    store_a.store("k1", "v1").unwrap();

    // Process B: write k2 (separate instance = separate cache).
    let store_b = SecretStore::keyring(svc);
    store_b.store("k2", "v2").unwrap();

    // Process A: write k3. With the fix, A re-reads inside the lock and
    // sees {k1, k2} before appending k3 — result must be {k1, k2, k3}.
    store_a.store("k3", "v3").unwrap();

    // Verify via a third reader (clean cache).
    let reader = SecretStore::keyring(svc);
    assert_eq!(
        reader.load("k1").unwrap(),
        Some("v1".to_string()),
        "k1 must survive"
    );
    assert_eq!(
        reader.load("k2").unwrap(),
        Some("v2".to_string()),
        "k2 must not be dropped"
    );
    assert_eq!(
        reader.load("k3").unwrap(),
        Some("v3".to_string()),
        "k3 must be written"
    );

    // Cleanup.
    let _ = reader.delete("k1");
    let _ = reader.delete("k2");
    let _ = reader.delete("k3");
}

#[ignore = "requires real OS keychain (run locally)"]
#[test]
fn test_concurrent_adds_neither_key_dropped() {
    // Two sequential stores from distinct instances (simulating two
    // processes each adding one key) must both be durably visible.
    let svc = "buzz-test-race-concurrent-add";

    let setup = SecretStore::keyring(svc);
    let _ = setup.delete("agent_a");
    let _ = setup.delete("agent_b");

    let store1 = SecretStore::keyring(svc);
    store1.store("agent_a", "nsec1aaa").unwrap();

    let store2 = SecretStore::keyring(svc);
    store2.store("agent_b", "nsec1bbb").unwrap();

    let reader = SecretStore::keyring(svc);
    assert_eq!(
        reader.load("agent_a").unwrap(),
        Some("nsec1aaa".to_string()),
        "agent_a must not be dropped"
    );
    assert_eq!(
        reader.load("agent_b").unwrap(),
        Some("nsec1bbb".to_string()),
        "agent_b must not be dropped"
    );

    // Cleanup.
    let _ = reader.delete("agent_a");
    let _ = reader.delete("agent_b");
}

#[test]
fn test_blob_lockfile_path_is_in_tmp_with_uid() {
    // The lockfile must be at a deterministic per-user path under /tmp —
    // invariant to $TMPDIR — so both a GUI-launched DMG (env-stripped by
    // launchd) and a terminal-launched dev build resolve the same inode and
    // achieve mutual exclusion.
    let path = blob_lockfile_path("buzz-desktop");
    #[cfg(unix)]
    {
        let uid = unsafe { libc::getuid() };
        assert!(
            path.starts_with("/tmp"),
            "lockfile {path:?} must start with /tmp (not $TMPDIR)"
        );
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        assert!(
            name.contains(&uid.to_string()),
            "lockfile {path:?} must contain uid {uid}"
        );
        assert!(
            name.contains("buzz-keychain"),
            "lockfile name must contain 'buzz-keychain'"
        );
    }
    #[cfg(not(unix))]
    {
        assert!(
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.contains("buzz-keychain")),
            "lockfile name must contain 'buzz-keychain'"
        );
    }
}

#[test]
fn test_blob_lock_acquire_and_release() {
    // Verify the advisory lock can be acquired and released without errors.
    // This exercises the real flock/mutex path on the current platform.
    let guard = acquire_blob_lock("buzz-test-lock-smoke");
    assert!(
        guard.is_ok(),
        "advisory lock acquire must succeed: {:?}",
        guard.err()
    );
    // Drop the guard — lock is released. A second acquire must succeed.
    drop(guard);
    let guard2 = acquire_blob_lock("buzz-test-lock-smoke");
    assert!(
        guard2.is_ok(),
        "advisory lock re-acquire after release must succeed: {:?}",
        guard2.err()
    );
}

#[ignore = "requires real OS keychain (run locally)"]
#[test]
fn mutate_blob_does_not_advance_cache_on_write_failure() {
    // Copy-on-write safety: if `write_blob_raw` fails (denied prompt,
    // transient outage, ACL rejection), the cache must stay at the last
    // known durable state. A subsequent `store()` for the same key/value
    // must NOT be skipped as a no-op — the equality check must compare
    // against the durable cache, not an unpersisted candidate.
    //
    // This is a real-keychain integration test. Run locally with:
    //   cargo test -p buzz-desktop -- --ignored mutate_blob_does_not_advance
    //
    // On a machine with a reachable keychain the `store()` call succeeds
    // (result.is_ok()) and the write-failure branch is skipped — the test
    // still passes. On a machine where the write is denied (e.g., user
    // clicks Deny in the macOS prompt) result.is_err() and the assertions
    // below verify the cache invariant. We verify that after an error:
    //   1. The cache is not advanced (the previously cached key is intact).
    //   2. The failed key is not present (the dirty candidate was discarded).
    let mut map = HashMap::new();
    map.insert("existing".to_string(), "durable_val".to_string());
    let store = SecretStore::with_cache("buzz-test-cow-write-fail", Some(map));

    // Attempt to add a new key — this calls write_blob_raw against the
    // real keychain; with copy-on-write the cache must remain at {existing}
    // if the write fails.
    let result = store.store("new_key", "new_val");

    if result.is_err() {
        // Write failed (e.g., user denied the keychain prompt): confirm
        // cache was not advanced — the existing key is still intact and
        // the new key was never committed to the in-memory state.
        assert_eq!(
            store.load("existing").unwrap(),
            Some("durable_val".to_string()),
            "cache must remain at last durable state after write failure"
        );
        // load("new_key") goes through the unchanged cache (no entry),
        // then attempts migrate_legacy_key which also fails on a denied
        // keychain, returning either Ok(None) or Err — either is correct
        // since the key was never durably stored.
        let after = store.load("new_key");
        assert!(
            matches!(after, Ok(None) | Err(_)),
            "a key whose write failed must not be visible via load: {after:?}"
        );
    }
    // If result.is_ok() the write succeeded — the cache-integrity invariant
    // does not apply to the success path; no assertion needed here.
}

#[test]
fn availability_error_discriminator() {
    assert!(is_keyring_availability_error("dbus connection failed"));
    assert!(is_keyring_availability_error(
        "org.freedesktop.secrets not provided"
    ));
    assert!(is_keyring_availability_error("No Secret Service"));
    assert!(is_keyring_availability_error(
        "Platform secure storage failure"
    ));
    // A plain "not found" is per-entry, not an availability failure.
    assert!(!is_keyring_availability_error("entry not found"));
}

#[cfg(target_os = "macos")]
#[test]
fn dpk_error_discriminators() {
    // errSecMissingEntitlement = -34018 signals unsigned dev build.
    let e = SFError::from_code(-34018);
    assert!(is_dpk_unavailable(&e));
    assert!(!is_not_found(&e));
    // errSecItemNotFound = -25300 is not a DPK-unavailable error.
    let e = SFError::from_code(-25300);
    assert!(is_not_found(&e));
    assert!(!is_dpk_unavailable(&e));
}

// Integration tests that exercise the real OS keychain. Skipped in CI
// (unsigned builds lack keychain entitlements); run locally with:
//   cargo test -p buzz-desktop -- --ignored blob_
//
// Each test uses a unique service name to avoid cross-test pollution.

#[ignore = "requires real OS keychain (run locally)"]
#[test]
fn blob_stores_and_retrieves_multiple_keys() {
    let store = SecretStore::keyring("buzz-test-blob-multi");
    store.store("key_a", "val_a").unwrap();
    store.store("key_b", "val_b").unwrap();
    assert_eq!(store.load("key_a").unwrap(), Some("val_a".to_string()));
    assert_eq!(store.load("key_b").unwrap(), Some("val_b".to_string()));
    assert_eq!(store.load("key_c").unwrap(), None);
    // Cleanup.
    let _ = store.delete("key_a");
    let _ = store.delete("key_b");
}

#[ignore = "requires real OS keychain (run locally)"]
#[test]
fn blob_probe_present_absent_unreachable() {
    let store = SecretStore::keyring("buzz-test-blob-probe");
    // No blob yet — key absent, backend reachable.
    assert_eq!(store.probe("identity"), KeyringProbe::ReachableButEmpty);
    store.store("identity", "nsec1test").unwrap();
    // Key now present.
    assert_eq!(store.probe("identity"), KeyringProbe::Present);
    // Different key — blob exists but key absent.
    assert_eq!(store.probe("other"), KeyringProbe::ReachableButEmpty);
    // Cleanup.
    let _ = store.delete("identity");
}

#[ignore = "requires real OS keychain (run locally)"]
#[test]
fn blob_delete_removes_key_not_others() {
    let store = SecretStore::keyring("buzz-test-blob-delete");
    store.store("keep", "keep_val").unwrap();
    store.store("remove", "remove_val").unwrap();
    store.delete("remove").unwrap();
    assert_eq!(store.load("keep").unwrap(), Some("keep_val".to_string()));
    assert_eq!(store.load("remove").unwrap(), None);
    // Cleanup.
    let _ = store.delete("keep");
}

#[ignore = "requires real OS keychain (run locally)"]
#[test]
fn blob_migration_from_per_key_entry() {
    let svc = "buzz-test-blob-migration";
    let key = "identity";
    let value = "nsec1migrationtest";

    // Seed a per-key entry (old format) — no blob exists.
    let entry = keyring_entry(svc, key).unwrap();
    entry.set_password(value).unwrap();

    // Fresh store — no blob in the keychain yet.
    let store = SecretStore::keyring(svc);

    // probe should find the legacy key.
    assert_eq!(store.probe(key), KeyringProbe::Present);

    // load should migrate it into the blob and return the value.
    assert_eq!(store.load(key).unwrap(), Some(value.to_string()));

    // Old per-key entry should be cleaned up.
    let entry = keyring_entry(svc, key).unwrap();
    assert!(matches!(entry.get_password(), Err(keyring::Error::NoEntry)));

    // Key is now in the blob — probe confirms.
    let store2 = SecretStore::keyring(svc);
    assert_eq!(store2.probe(key), KeyringProbe::Present);
    assert_eq!(store2.load(key).unwrap(), Some(value.to_string()));

    // Cleanup.
    let _ = store2.delete(key);
}

#[ignore = "requires real OS keychain (run locally)"]
#[test]
fn delete_all_with_legacy_cleanup_removes_per_key_identity() {
    let svc = "buzz-test-delete-all-legacy";
    let key = "identity";
    let value = "nsec1legacytest";

    // Seed a legacy per-key entry (old format, pre-blob migration).
    let entry = keyring_entry(svc, key).unwrap();
    entry.set_password(value).unwrap();

    // Also seed a blob with a different key to exercise the full path.
    let store = SecretStore::keyring(svc);
    store.store("agent:abc123", "nsec1agent").unwrap();

    // Legacy per-key identity should be discoverable via probe.
    let store2 = SecretStore::keyring(svc);
    assert_eq!(store2.probe(key), KeyringProbe::Present);

    // Wipe everything via the sign-out path.
    store2.delete_all_with_legacy_cleanup().unwrap();

    // Fresh store — neither the blob nor the per-key entry should remain.
    let store3 = SecretStore::keyring(svc);
    assert_eq!(
        store3.probe(key),
        KeyringProbe::ReachableButEmpty,
        "per-key identity must not survive delete_all_with_legacy_cleanup"
    );
    assert_eq!(
        store3.load(key).unwrap(),
        None,
        "load must not resurrect the legacy per-key identity"
    );
    // Agent key should also be gone.
    assert_eq!(store3.load("agent:abc123").unwrap(), None);
}
