//! Chunked layout for the keychain secret store.
//!
//! Windows Credential Manager caps one credential at 1280 UTF-16 units, which
//! made the single-entry store a hard ceiling of about 8 agents (BUG-013).
//! These types split the map across entries with an atomic generation flip.
//!
//! Extracted from `secret_store.rs` to keep that file under the repo's
//! file-size ratchet; the logic is unchanged.

use serde::Deserialize;

/// Largest payload written to one keychain entry, in **UTF-16 code units**.
///
/// The unit matters and is easy to get wrong. Windows' constant
/// `CRED_MAX_CREDENTIAL_BLOB_SIZE` is 2560 **bytes**, and the keyring crate
/// rejects a password when `encode_utf16().count() * 2` exceeds it
/// (`keyring-3.6.3/src/windows.rs:224`). So the real ceiling is **1280 UTF-16
/// units**, not 2560 — the crate's own maximum-length test uses
/// `CRED_MAX_CREDENTIAL_BLOB_SIZE / 2`.
///
/// The error text the backend produces says "longer than platform limit of
/// 2560 chars", quoting the byte constant. Believing that number yields a
/// chunk size that still fails every write.
///
/// 1200 leaves margin under the 1280 ceiling. (The entry name is charged
/// against a separate, far larger limit and does not compete for this budget.)
pub(super) const MAX_ENTRY_UTF16: usize = 1_200;

/// Upper bound on a header's declared length, so a corrupt header cannot drive
/// a huge allocation. Generous: far beyond any plausible secret store.
pub(super) const MAX_ASSEMBLED_BYTES: usize = MAX_CHUNKS * MAX_ENTRY_UTF16 * 4;

/// Cost of a string against the backend's budget.
pub(super) fn utf16_len(value: &str) -> usize {
    value.encode_utf16().count()
}

/// Upper bound on chunks scanned when clearing an old generation. Far beyond
/// any plausible agent count, and it stops a corrupt header driving an endless
/// scan.
pub(super) const MAX_CHUNKS: usize = 4_096;

/// Which set of chunk entries is live. Writes always target the other one, so
/// the previous state stays readable until the header flip commits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Generation {
    A,
    B,
}

impl Generation {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Generation::A => "a",
            Generation::B => "b",
        }
    }

    pub(super) fn other(self) -> Self {
        match self {
            Generation::A => Generation::B,
            Generation::B => Generation::A,
        }
    }

    pub(super) fn parse(value: &str) -> Option<Self> {
        match value {
            "a" => Some(Generation::A),
            "b" => Some(Generation::B),
            _ => None,
        }
    }
}

pub(super) fn chunk_key(generation: Generation, index: usize) -> String {
    format!("{}.{}.{index}", super::BLOB_KEY, generation.as_str())
}

/// Distinguishes a backend fault from a read that raced a concurrent write.
///
/// The distinction drives the retry in `read_blob_raw_keyring`: a torn read is
/// transient and worth repeating, a backend fault is not.
///
/// Note `probe` still maps BOTH arms to `Unreachable` — failing closed on any
/// unreadable store is deliberate. The retry, not the classification, is what
/// keeps a raced read from booting the app on an ephemeral identity.
#[cfg(feature = "system-keyring")]
pub(super) enum BlobReadError {
    Backend(String),
    Torn(String),
}

#[cfg(feature = "system-keyring")]
impl BlobReadError {
    pub(super) fn into_message(self) -> String {
        match self {
            BlobReadError::Backend(message) => message,
            BlobReadError::Torn(message) => message,
        }
    }
}

/// Split so that no piece exceeds `max_utf16` UTF-16 code units, cutting only
/// on character boundaries.
///
/// Counting in UTF-16 units rather than characters is what keeps the pieces
/// actually writable: an astral-plane character costs two units, so a
/// character-counted split could hand the backend a piece twice its budget.
/// `store()` is a public API taking arbitrary values, so this has to hold for
/// more than today's ASCII payload.
pub(super) fn split_utf16(value: &str, max_utf16: usize) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut counted = 0;
    for (offset, character) in value.char_indices() {
        let cost = character.len_utf16();
        if counted + cost > max_utf16 {
            parts.push(&value[start..offset]);
            start = offset;
            counted = 0;
        }
        counted += cost;
    }
    parts.push(&value[start..]);
    parts
}

/// Cheap non-cryptographic digest of the assembled payload.
///
/// Guards against a **torn read**: reads are not serialised against writes, so
/// a reader can take the header from one generation and a chunk from the next.
/// Length alone does not catch that — swapping one secret for another of the
/// same length leaves the total unchanged — so the header carries a digest of
/// the exact bytes it describes.
pub(super) fn payload_digest(value: &str) -> u64 {
    // FNV-1a. Not security-relevant: this detects accidental mismatch, and an
    // attacker who can write the credential store has already won.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Header stored under [`BLOB_KEY`] once the map is chunked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ChunkHeader {
    pub(super) generation: Generation,
    pub(super) chunks: usize,
    pub(super) len: usize,
    pub(super) digest: u64,
}

impl ChunkHeader {
    /// Returns `None` for anything that is not a chunk header — including the
    /// original layout, where this entry holds the JSON map. A map serialises
    /// every value as a JSON string, and `v` is required to be a number, so no
    /// key name can make a legitimate map parse as a header.
    pub(super) fn parse(raw: &str) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(raw).ok()?;
        let object = value.as_object()?;
        if object.get("v")?.as_u64()? != 2 {
            return None;
        }
        let len = usize::try_from(object.get("len")?.as_u64()?).ok()?;
        if len > MAX_ASSEMBLED_BYTES {
            return None;
        }
        let chunks = usize::try_from(object.get("chunks")?.as_u64()?).ok()?;
        if chunks > MAX_CHUNKS {
            return None;
        }
        Some(ChunkHeader {
            generation: Generation::parse(object.get("gen")?.as_str()?)?,
            chunks,
            len,
            digest: object.get("digest")?.as_u64()?,
        })
    }

    pub(super) fn render(&self) -> String {
        format!(
            r#"{{"v":2,"gen":"{}","chunks":{},"len":{},"digest":{}}}"#,
            self.generation.as_str(),
            self.chunks,
            self.len,
            self.digest
        )
    }
}
