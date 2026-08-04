//! Shared secret authenticating external (non-UI) agent control requests.
//!
//! A local process that wants the app to restart a managed agent invokes
//! `buzz://restart-agent?pubkey=…&token=…`. Deep links are not a trusted
//! channel — any program on the machine can fire one, and on some platforms so
//! can a web page — so the request must prove it can read a file only the
//! user's account can read. This token is that proof: 32 bytes of OS entropy,
//! written `0o600` next to the managed-agent store, minted once at first launch
//! and never rotated automatically (an external caller's copy must stay valid
//! across restarts).
//!
//! The token is never logged, never emitted to the frontend, and compared in
//! constant time so a caller cannot recover it byte by byte from timing.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tauri::AppHandle;

use super::storage::{atomic_write_json_restricted, managed_agents_base_dir};

/// File name of the control token, inside the managed-agents app-data dir.
const CONTROL_TOKEN_FILE: &str = "control-token.json";

/// Entropy per token. 32 bytes renders as the 64 hex chars callers paste.
const CONTROL_TOKEN_BYTES: usize = 32;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ControlTokenFile {
    token: String,
}

/// Path of the control-token file. Creates the agents dir if missing.
pub(crate) fn control_token_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(managed_agents_base_dir(app)?.join(CONTROL_TOKEN_FILE))
}

/// Read the control token, minting one if the file does not exist yet.
///
/// Called once during setup so the token is on disk before the first deep link
/// can arrive. An existing file is never overwritten — not even a malformed
/// one, which surfaces as an error instead. Silently reminting would revoke a
/// token external callers already hold, turning a transient read failure into a
/// permanent authentication break.
pub(crate) fn load_or_create_control_token(app: &AppHandle) -> Result<String, String> {
    load_or_create_control_token_at(&control_token_path(app)?)
}

/// Whether `presented` matches the token on disk.
///
/// Returns `false` for every failure mode (no file, unreadable, malformed) so
/// the caller has a single fail-closed answer: a missing token file must reject
/// control requests, never admit them.
pub(crate) fn verify_control_token(app: &AppHandle, presented: &str) -> bool {
    match control_token_path(app) {
        Ok(path) => verify_control_token_at(&path, presented),
        Err(error) => {
            eprintln!("buzz-desktop: cannot resolve agent control token path: {error}");
            false
        }
    }
}

fn load_or_create_control_token_at(path: &Path) -> Result<String, String> {
    if path.exists() {
        return read_control_token_at(path);
    }
    let token = generate_control_token()?;
    let payload = serde_json::to_vec(&ControlTokenFile {
        token: token.clone(),
    })
    .map_err(|error| format!("failed to encode control token: {error}"))?;
    atomic_write_json_restricted(path, &payload)?;
    Ok(token)
}

fn verify_control_token_at(path: &Path, presented: &str) -> bool {
    let Ok(expected) = read_control_token_at(path) else {
        return false;
    };
    constant_time_eq(expected.as_bytes(), presented.as_bytes())
}

fn read_control_token_at(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    let parsed: ControlTokenFile = serde_json::from_slice(&bytes)
        .map_err(|error| format!("failed to parse {}: {error}", path.display()))?;
    if parsed.token.is_empty() {
        return Err(format!("{} holds an empty token", path.display()));
    }
    Ok(parsed.token)
}

fn generate_control_token() -> Result<String, String> {
    let mut bytes = [0u8; CONTROL_TOKEN_BYTES];
    getrandom::getrandom(&mut bytes).map_err(|error| format!("entropy source: {error}"))?;
    Ok(hex::encode(bytes))
}

/// Compare two byte strings without an early exit on the first differing byte.
///
/// Length is compared up front and therefore leaks — that is inherent to a
/// variable-length secret and reveals nothing about its contents.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut difference = 0u8;
    for (left, right) in a.iter().zip(b.iter()) {
        difference |= left ^ right;
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::{
        constant_time_eq, load_or_create_control_token_at, verify_control_token_at,
        CONTROL_TOKEN_BYTES,
    };

    #[test]
    fn load_or_create_mints_a_hex_token_on_first_call() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control-token.json");

        let token = load_or_create_control_token_at(&path).unwrap();

        assert_eq!(token.len(), CONTROL_TOKEN_BYTES * 2);
        assert!(token
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));
        assert!(path.exists());
    }

    #[test]
    fn load_or_create_reuses_an_existing_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control-token.json");

        let first = load_or_create_control_token_at(&path).unwrap();
        let second = load_or_create_control_token_at(&path).unwrap();

        assert_eq!(first, second);
    }

    #[test]
    fn load_or_create_refuses_to_remint_over_a_malformed_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control-token.json");
        std::fs::write(&path, b"not json").unwrap();

        assert!(load_or_create_control_token_at(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"not json");
    }

    #[test]
    fn verify_accepts_the_minted_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control-token.json");
        let token = load_or_create_control_token_at(&path).unwrap();

        assert!(verify_control_token_at(&path, &token));
    }

    #[test]
    fn verify_rejects_a_mismatched_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control-token.json");
        let token = load_or_create_control_token_at(&path).unwrap();

        assert!(!verify_control_token_at(&path, &token[..token.len() - 1]));
        assert!(!verify_control_token_at(&path, &format!("{token}0")));
        assert!(!verify_control_token_at(&path, ""));
        assert!(!verify_control_token_at(&path, &"0".repeat(token.len())));
    }

    #[test]
    fn verify_fails_closed_when_the_token_file_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control-token.json");

        assert!(!verify_control_token_at(&path, "anything"));
        assert!(!verify_control_token_at(&path, ""));
    }

    #[test]
    fn verify_fails_closed_on_a_malformed_or_empty_token_file() {
        let dir = tempfile::tempdir().unwrap();
        for (name, contents) in [
            ("bad-json.json", "not json".as_bytes()),
            ("empty-token.json", br#"{"token":""}"#.as_ref()),
            ("wrong-shape.json", br#"{"secret":"abc"}"#.as_ref()),
        ] {
            let path = dir.path().join(name);
            std::fs::write(&path, contents).unwrap();
            assert!(!verify_control_token_at(&path, "abc"), "{name}");
        }
    }

    #[test]
    fn constant_time_eq_matches_plain_equality() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(constant_time_eq(b"", b""));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"", b"a"));
    }
}
