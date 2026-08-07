//! Process entry point for the single-community loopback edge relay.

use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use buzz_edge::eligibility::{
    apply_startup_policy, verify_upstream_membership, AuthorizationStartup,
};
use buzz_edge::storage::{AuthorizationPolicy, CommunityBinding, EdgeStore};
use buzz_edge::{run_server, EdgeConfig, EdgeRelay};
use nostr::Keys;
#[cfg(windows)]
use nostr::ToBech32;
use tokio::net::TcpListener;
use tracing::{error, info, warn};
use uuid::Uuid;

const AUTHORIZATION_LEASE_HOURS: u64 = 7 * 24;

#[cfg(windows)]
const EDGE_KEYRING_SERVICE: &str = "buzz-edge";

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_target(false)
        .without_time()
        .init();
    if let Err(error) = run().await {
        error!("{error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let canonical_origin = required_env("BUZZ_RELAY_URL")?;
    let community_id = required_env("BUZZ_COMMUNITY_ID")?
        .parse::<Uuid>()
        .map_err(|error| format!("BUZZ_COMMUNITY_ID must be a UUID: {error}"))?;
    let binding = CommunityBinding::new(&canonical_origin, community_id)
        .map_err(|error| error.to_string())?;
    let authorization_policy = authorization_policy()?;
    let data_directory = PathBuf::from(required_env("BUZZ_EDGE_DATA_DIR")?);
    let (store, database_path) =
        EdgeStore::open_bound(&data_directory, binding.clone(), authorization_policy)
            .map_err(|error| error.to_string())?;
    store
        .replace_selected_channels(&selected_channels_from_env()?)
        .map_err(|error| error.to_string())?;

    let edge_keys = load_or_create_edge_keys(&binding)?;
    info!(
        edge_pubkey = %edge_keys.public_key().to_hex(),
        database = %database_path.display(),
        "edge identity loaded; provision this pubkey as a direct relay and selected-channel member"
    );
    let store = Arc::new(store);
    let verification = verify_upstream_membership(Arc::clone(&store), &edge_keys).await;
    let now = unix_seconds()?;
    let startup = apply_startup_policy(&store, &edge_keys, now, verification)
        .map_err(|error| error.to_string())?;
    log_authorization_startup(&startup);
    let startup_was_fresh = matches!(startup, AuthorizationStartup::Fresh { .. });

    let edge_url =
        env::var("BUZZ_EDGE_RELAY_URL").unwrap_or_else(|_| "ws://127.0.0.1:3031".to_string());
    let listen_address = loopback_socket_address(&edge_url)?;
    let config = EdgeConfig::new(&edge_url, binding).map_err(|error| error.to_string())?;
    let relay = EdgeRelay::new(
        config,
        store,
        edge_keys.clone(),
        matches!(startup, AuthorizationStartup::OfflineLease { .. }),
    )
    .map_err(|error| error.to_string())?;
    let listener = TcpListener::bind(listen_address)
        .await
        .map_err(|error| format!("failed to bind {listen_address}: {error}"))?;
    info!(%listen_address, "buzz-edge listening");
    let mirror_relay = relay.clone();
    tokio::spawn(async move {
        buzz_edge::upstream::run_upstream_mirror(mirror_relay, edge_keys, startup_was_fresh).await;
    });
    run_server(listener, relay)
        .await
        .map_err(|error| error.to_string())
}

fn log_authorization_startup(startup: &AuthorizationStartup) {
    match startup {
        AuthorizationStartup::Fresh { eligible_channels } => {
            info!(
                eligible_channels = eligible_channels.len(),
                "fresh upstream edge eligibility verified"
            );
        }
        AuthorizationStartup::OfflineLease {
            eligible_channels,
            expires_at,
        } => {
            warn!(
                eligible_channels = eligible_channels.len(),
                expires_at, "upstream unavailable; serving under signed authorization lease"
            );
        }
        AuthorizationStartup::Disabled { reason } => {
            warn!(%reason, "edge routing is fail-closed; clients must use canonical routing");
        }
    }
}

fn required_env(name: &str) -> Result<String, String> {
    env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("{name} is required"))
}

fn selected_channels_from_env() -> Result<Vec<Uuid>, String> {
    let raw = env::var("BUZZ_EDGE_CHANNELS").unwrap_or_default();
    raw.split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| {
            value
                .parse::<Uuid>()
                .map_err(|error| format!("invalid BUZZ_EDGE_CHANNELS entry {value}: {error}"))
        })
        .collect()
}

fn authorization_policy() -> Result<AuthorizationPolicy, String> {
    let seconds = AUTHORIZATION_LEASE_HOURS
        .checked_mul(60 * 60)
        .ok_or_else(|| "authorization lease is too large".to_string())?;
    AuthorizationPolicy::new(Duration::from_secs(seconds)).map_err(|error| error.to_string())
}

fn loopback_socket_address(edge_url: &str) -> Result<SocketAddr, String> {
    let parsed = url::Url::parse(edge_url)
        .map_err(|error| format!("invalid BUZZ_EDGE_RELAY_URL: {error}"))?;
    if parsed.scheme() != "ws"
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || (parsed.path() != "" && parsed.path() != "/")
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err("BUZZ_EDGE_RELAY_URL must be a plain ws:// loopback origin".to_string());
    }
    let addresses = parsed
        .socket_addrs(|| None)
        .map_err(|error| format!("invalid BUZZ_EDGE_RELAY_URL address: {error}"))?;
    addresses
        .into_iter()
        .find(|address| address.ip().is_loopback())
        .ok_or_else(|| "BUZZ_EDGE_RELAY_URL must resolve to loopback".to_string())
}

#[cfg(windows)]
fn load_or_create_edge_keys(binding: &CommunityBinding) -> Result<Keys, String> {
    let account = binding.database_filename();
    let entry = keyring::Entry::new(EDGE_KEYRING_SERVICE, &account)
        .map_err(|error| format!("Windows protected key store is unavailable: {error}"))?;
    match entry.get_password() {
        Ok(secret) => Keys::parse(secret.trim())
            .map_err(|error| format!("stored edge identity is corrupt: {error}")),
        Err(keyring::Error::NoEntry) => {
            let keys = Keys::generate();
            let encoded = keys
                .secret_key()
                .to_bech32()
                .map_err(|error| format!("failed to encode edge identity: {error}"))?;
            entry
                .set_password(&encoded)
                .map_err(|error| format!("failed to protect edge identity: {error}"))?;
            let read_back = entry
                .get_password()
                .map_err(|error| format!("edge identity read-back failed: {error}"))?;
            let persisted = Keys::parse(read_back.trim())
                .map_err(|error| format!("persisted edge identity is corrupt: {error}"))?;
            if persisted.public_key() != keys.public_key() {
                return Err("edge identity read-back verification failed".to_string());
            }
            Ok(keys)
        }
        Err(error) => Err(format!(
            "Windows protected key store is unavailable: {error}"
        )),
    }
}

#[cfg(not(windows))]
fn load_or_create_edge_keys(_binding: &CommunityBinding) -> Result<Keys, String> {
    Err("buzz-edge phase 1 requires the Windows protected key store".to_string())
}

fn unix_seconds() -> Result<i64, String> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("system clock is before Unix epoch: {error}"))?;
    i64::try_from(duration.as_secs()).map_err(|error| format!("system clock overflow: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listener_url_must_be_plain_loopback_ws() {
        assert!(loopback_socket_address("ws://127.0.0.1:3031").is_ok());
        assert!(loopback_socket_address("ws://localhost:3031/path").is_err());
        assert!(loopback_socket_address("wss://127.0.0.1:3031").is_err());
        assert!(loopback_socket_address("ws://192.0.2.1:3031").is_err());
    }

    #[test]
    fn authorization_lease_is_the_owner_selected_seven_days() {
        assert_eq!(
            authorization_policy().expect("policy").lease_seconds(),
            7 * 24 * 60 * 60
        );
    }
}
