#![deny(unsafe_code)]
//! Loopback-only continuity relay for persistent Buzz channel messages.

pub mod eligibility;
mod protocol;
pub mod storage;
pub mod upstream;

use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use buzz_core::event::StoredEvent;
use buzz_core::filter::filters_match;
use futures_util::stream::FuturesUnordered;
use futures_util::{SinkExt, StreamExt};
use nostr::{Event, Filter, Keys, PublicKey, TagKind};
use parking_lot::Mutex;
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};
use uuid::Uuid;

use protocol::{
    auth_challenge, auth_tag_json, binding_result, bounded_filters, closed, count, eose,
    event_channel, event_message, filter_channels, notice, ok, parse_client_message, ClientMessage,
};
use storage::{CommunityBinding, EdgeStore, InsertOutcome, StorageError};

const MAX_CONNECTIONS: usize = 128;
const MAX_EVENT_CONTENT_BYTES: usize = 256 * 1024;
// JSON may expand each one-byte control character to a six-byte `\u00XX`
// escape. Reserve that worst case plus fixed event/envelope metadata.
const MAX_MESSAGE_BYTES: usize = (MAX_EVENT_CONTENT_BYTES * 6) + (64 * 1024);
const OUTBOUND_CAPACITY: usize = 512;
const OUTBOUND_ENQUEUE_TIMEOUT: Duration = Duration::from_millis(250);
const AUTHORIZATION_SWEEP_INTERVAL: Duration = Duration::from_secs(1);

/// Sidecar configuration shared by WebSocket and HTTP auth verification.
#[derive(Debug, Clone)]
pub struct EdgeConfig {
    relay_url: url::Url,
    binding: CommunityBinding,
}

impl EdgeConfig {
    /// Create a loopback WebSocket origin, such as `ws://127.0.0.1:3031`.
    pub fn new(relay_url: &str, binding: CommunityBinding) -> Result<Self, EdgeError> {
        let parsed = url::Url::parse(relay_url)
            .map_err(|error| EdgeError::Config(format!("invalid edge relay URL: {error}")))?;
        if parsed.scheme() != "ws" {
            return Err(EdgeError::Config(
                "edge relay URL scheme must be ws".to_string(),
            ));
        }
        if !url_host_is_loopback(&parsed) {
            return Err(EdgeError::Config(
                "edge relay URL must use a loopback host".to_string(),
            ));
        }
        if !parsed.username().is_empty()
            || parsed.password().is_some()
            || (parsed.path() != "" && parsed.path() != "/")
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(EdgeError::Config(
                "edge relay URL must be a plain loopback origin".to_string(),
            ));
        }
        Ok(Self {
            relay_url: parsed,
            binding,
        })
    }

    fn websocket_url(&self) -> &str {
        self.relay_url.as_str().trim_end_matches('/')
    }

    fn http_url(&self, path: &str) -> Result<String, EdgeError> {
        let mut url = self.relay_url.clone();
        let scheme = if url.scheme() == "wss" {
            "https"
        } else {
            "http"
        };
        url.set_scheme(scheme)
            .map_err(|_| EdgeError::Config("failed to derive edge HTTP URL".to_string()))?;
        url.set_path(path);
        url.set_query(None);
        url.set_fragment(None);
        Ok(url.to_string())
    }

    fn binding_matches(&self, canonical_origin: &str, community_id: Uuid) -> bool {
        CommunityBinding::new(canonical_origin, community_id)
            .map(|binding| binding == self.binding)
            .unwrap_or(false)
    }
}

/// Fatal sidecar errors.
#[derive(Debug, thiserror::Error)]
pub enum EdgeError {
    /// Invalid loopback configuration.
    #[error("configuration error: {0}")]
    Config(String),
    /// Listener or server I/O failed.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Durable storage failed.
    #[error(transparent)]
    Storage(#[from] StorageError),
    /// An upstream mirror event was outside the active authorization contract.
    #[error("upstream mirror rejected: {0}")]
    UpstreamRejected(String),
    /// A bounded blocking task failed to join.
    #[error("blocking task failed: {0}")]
    Join(String),
}

#[derive(Clone)]
struct Subscription {
    connection_id: u64,
    sub_id: String,
    filters: Vec<Filter>,
    principal: PublicKey,
    outbound: mpsc::Sender<Message>,
    runtime: Arc<tokio::sync::Mutex<SubscriptionRuntime>>,
    send_gate: Arc<tokio::sync::Mutex<()>>,
}

struct SubscriptionRuntime {
    cancelled: bool,
    backfilling: bool,
    buffered_live: Vec<Event>,
}

struct EdgeState {
    config: EdgeConfig,
    store: Arc<EdgeStore>,
    edge_keys: Keys,
    subscriptions: Mutex<Vec<Subscription>>,
    next_connection_id: AtomicU64,
    connections: Arc<Semaphore>,
    sequence: tokio::sync::Mutex<()>,
    authorization_epoch: tokio::sync::RwLock<()>,
    #[cfg(test)]
    history_gate: Mutex<Option<Arc<HistoryGate>>>,
}

#[cfg(test)]
#[derive(Default)]
struct HistoryGate {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

/// Cloneable handle to the local continuity relay.
#[derive(Clone)]
pub struct EdgeRelay {
    state: Arc<EdgeState>,
}

impl EdgeRelay {
    /// Build a sidecar around an opened store and dedicated edge-device key.
    pub fn new(
        config: EdgeConfig,
        store: Arc<EdgeStore>,
        edge_keys: Keys,
    ) -> Result<Self, EdgeError> {
        if store.binding() != &config.binding {
            return Err(EdgeError::Config(
                "edge configuration and SQLite community binding differ".to_string(),
            ));
        }
        Ok(Self {
            state: Arc::new(EdgeState {
                config,
                store,
                edge_keys,
                subscriptions: Mutex::new(Vec::new()),
                next_connection_id: AtomicU64::new(1),
                connections: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
                sequence: tokio::sync::Mutex::new(()),
                authorization_epoch: tokio::sync::RwLock::new(()),
                #[cfg(test)]
                history_gate: Mutex::new(None),
            }),
        })
    }

    /// Access the membership/cache store for the upstream mirror coordinator.
    pub fn store(&self) -> &Arc<EdgeStore> {
        &self.state.store
    }

    /// Apply one upstream authorization result without racing local access checks.
    pub async fn apply_authorization_refresh(
        &self,
        now: i64,
        verification: eligibility::VerificationResult,
    ) -> Result<eligibility::AuthorizationStartup, EdgeError> {
        let result = {
            let _authorization = self.state.authorization_epoch.write().await;
            let store = Arc::clone(&self.state.store);
            let edge_keys = self.state.edge_keys.clone();
            tokio::task::spawn_blocking(move || {
                eligibility::apply_startup_policy(&store, &edge_keys, now, verification)
            })
            .await
            .map_err(|error| EdgeError::Join(error.to_string()))?
            .map_err(EdgeError::from)?
        };
        self.close_ineligible_subscriptions().await?;
        Ok(result)
    }

    async fn accept_event(
        &self,
        principal: PublicKey,
        event: Event,
        exact_event_bytes: Vec<u8>,
    ) -> Result<(bool, String), EdgeError> {
        if event.content.len() > MAX_EVENT_CONTENT_BYTES {
            return Ok((
                false,
                "invalid: event content exceeds the 256 KiB limit".to_string(),
            ));
        }
        if event.pubkey != principal {
            return Ok((
                false,
                "restricted: event pubkey does not match authenticated principal".to_string(),
            ));
        }
        let channel_id = match event_channel(&event) {
            Ok(channel_id) => channel_id,
            Err(message) => return Ok((false, message)),
        };

        let event_to_verify = event.clone();
        let verified = tokio::task::spawn_blocking(move || {
            buzz_core::verification::verify_event(&event_to_verify)
        })
        .await
        .map_err(|error| EdgeError::Join(error.to_string()))?;
        if verified.is_err() {
            return Ok((
                false,
                "invalid: event signature verification failed".to_string(),
            ));
        }

        let _sequence = self.state.sequence.lock().await;
        let _authorization = self.state.authorization_epoch.read().await;
        if !self.principal_can_access(channel_id, principal).await? {
            return Ok((false, "restricted: channel membership changed".to_string()));
        }
        let store = Arc::clone(&self.state.store);
        let edge_keys = self.state.edge_keys.clone();
        let event_to_store = event.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            store.insert_local_event(&event_to_store, &exact_event_bytes, channel_id, &edge_keys)
        })
        .await
        .map_err(|error| EdgeError::Join(error.to_string()))??;

        if outcome == InsertOutcome::Inserted {
            self.fan_out(&event, channel_id).await?;
            Ok((true, "delivered locally".to_string()))
        } else {
            Ok((true, "duplicate: already delivered locally".to_string()))
        }
    }

    pub(crate) async fn accept_upstream_event(
        &self,
        event: Event,
        exact_event_bytes: Vec<u8>,
    ) -> Result<InsertOutcome, EdgeError> {
        let channel_id = event_channel(&event).map_err(|message| {
            StorageError::Corrupt(format!("invalid upstream event: {message}"))
        })?;
        let _sequence = self.state.sequence.lock().await;
        let _authorization = self.state.authorization_epoch.read().await;
        let store = Arc::clone(&self.state.store);
        let eligible =
            tokio::task::spawn_blocking(move || store.channel_is_edge_eligible(channel_id))
                .await
                .map_err(|error| EdgeError::Join(error.to_string()))??;
        if !eligible {
            return Err(EdgeError::UpstreamRejected(
                "channel is not edge-eligible".to_string(),
            ));
        }
        let store = Arc::clone(&self.state.store);
        let event_to_store = event.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            store.insert_upstream_event(&event_to_store, &exact_event_bytes, channel_id)
        })
        .await
        .map_err(|error| EdgeError::Join(error.to_string()))??;
        if outcome == InsertOutcome::Inserted {
            self.fan_out(&event, channel_id).await?;
        }
        Ok(outcome)
    }

    async fn fan_out(&self, event: &Event, channel_id: Uuid) -> Result<(), EdgeError> {
        let subscriptions = self.state.subscriptions.lock().clone();
        let stored = StoredEvent::new(event.clone(), Some(channel_id));
        let mut sends = FuturesUnordered::new();
        for subscription in subscriptions {
            if !filters_match(&subscription.filters, &stored) {
                continue;
            }
            let mut runtime = subscription.runtime.lock().await;
            if runtime.cancelled {
                continue;
            }
            if runtime.backfilling {
                if !runtime
                    .buffered_live
                    .iter()
                    .any(|buffered| buffered.id == event.id)
                {
                    runtime.buffered_live.push(event.clone());
                }
                continue;
            }
            drop(runtime);
            let relay = self.clone();
            let message = Message::Text(event_message(&subscription.sub_id, event).into());
            sends.push(async move {
                let _gate = subscription.send_gate.lock().await;
                if subscription.runtime.lock().await.cancelled {
                    return (
                        subscription.connection_id,
                        subscription.sub_id.clone(),
                        false,
                    );
                }
                match relay
                    .principal_can_access(channel_id, subscription.principal)
                    .await
                {
                    Ok(true) => {}
                    Ok(false) | Err(_) => {
                        relay
                            .cancel_subscription(
                                &subscription,
                                "restricted: authorization changed; reconnect to canonical relay",
                            )
                            .await;
                        return (
                            subscription.connection_id,
                            subscription.sub_id.clone(),
                            false,
                        );
                    }
                }
                let sent = matches!(
                    tokio::time::timeout(
                        OUTBOUND_ENQUEUE_TIMEOUT,
                        subscription.outbound.send(message),
                    )
                    .await,
                    Ok(Ok(()))
                );
                if !sent {
                    subscription.runtime.lock().await.cancelled = true;
                }
                (
                    subscription.connection_id,
                    subscription.sub_id.clone(),
                    sent,
                )
            });
        }
        let mut stale_subscriptions = Vec::new();
        while let Some((connection_id, sub_id, sent)) = sends.next().await {
            if !sent {
                stale_subscriptions.push((connection_id, sub_id));
            }
        }
        if !stale_subscriptions.is_empty() {
            self.remove_subscriptions(&stale_subscriptions);
            tracing::warn!(
                subscriptions = ?stale_subscriptions,
                "removed stalled edge subscriptions instead of silently dropping a local event"
            );
        }
        Ok(())
    }

    async fn close_ineligible_subscriptions(&self) -> Result<(), EdgeError> {
        let subscriptions = self.state.subscriptions.lock().clone();
        let mut closed_subscriptions = Vec::new();
        for subscription in subscriptions {
            let _gate = subscription.send_gate.lock().await;
            if subscription.runtime.lock().await.cancelled {
                closed_subscriptions
                    .push((subscription.connection_id, subscription.sub_id.clone()));
                continue;
            }
            let channels = filter_channels(&subscription.filters)
                .map_err(|error| EdgeError::Config(format!("stored subscription: {error}")))?;
            let mut authorized = true;
            for channel in channels {
                if !self
                    .principal_can_access(channel, subscription.principal)
                    .await?
                {
                    authorized = false;
                    break;
                }
            }
            if authorized {
                continue;
            }
            self.cancel_subscription(
                &subscription,
                "restricted: authorization changed; reconnect to canonical relay",
            )
            .await;
            closed_subscriptions.push((subscription.connection_id, subscription.sub_id.clone()));
        }
        if !closed_subscriptions.is_empty() {
            self.remove_subscriptions(&closed_subscriptions);
        }
        Ok(())
    }

    async fn deliver_history(
        &self,
        subscription: &Subscription,
        history: Vec<Event>,
    ) -> Result<(), EdgeError> {
        #[cfg(test)]
        let history_gate = { self.state.history_gate.lock().clone() };
        #[cfg(test)]
        if let Some(gate) = history_gate {
            gate.entered.notify_one();
            gate.release.notified().await;
        }
        let mut delivered = HashSet::new();
        for event in history {
            let _gate = subscription.send_gate.lock().await;
            if subscription.runtime.lock().await.cancelled {
                return Ok(());
            }
            let channel_id = event_channel(&event)
                .map_err(|error| EdgeError::Config(format!("stored event: {error}")))?;
            if !self
                .principal_can_access(channel_id, subscription.principal)
                .await?
            {
                self.cancel_and_remove(subscription).await;
                return Ok(());
            }
            if !delivered.insert(event.id) {
                continue;
            }
            let sent = matches!(
                tokio::time::timeout(
                    OUTBOUND_ENQUEUE_TIMEOUT,
                    subscription.outbound.send(Message::Text(
                        event_message(&subscription.sub_id, &event).into(),
                    )),
                )
                .await,
                Ok(Ok(()))
            );
            if !sent {
                subscription.runtime.lock().await.cancelled = true;
                self.remove_subscriptions(&[(
                    subscription.connection_id,
                    subscription.sub_id.clone(),
                )]);
                return Ok(());
            }
        }

        let _gate = subscription.send_gate.lock().await;
        if subscription.runtime.lock().await.cancelled {
            return Ok(());
        }
        if self
            .authorize_filters(&subscription.filters, subscription.principal)
            .await
            .is_err()
        {
            self.cancel_and_remove(subscription).await;
            return Ok(());
        }
        let buffered = {
            let mut runtime = subscription.runtime.lock().await;
            runtime.backfilling = false;
            std::mem::take(&mut runtime.buffered_live)
        };
        if subscription
            .outbound
            .send(Message::Text(eose(&subscription.sub_id).into()))
            .await
            .is_err()
        {
            subscription.runtime.lock().await.cancelled = true;
            self.remove_subscriptions(&[(subscription.connection_id, subscription.sub_id.clone())]);
            return Ok(());
        }
        for event in buffered {
            if !delivered.insert(event.id) {
                continue;
            }
            let channel_id = event_channel(&event)
                .map_err(|error| EdgeError::Config(format!("buffered event: {error}")))?;
            if !self
                .principal_can_access(channel_id, subscription.principal)
                .await?
            {
                self.cancel_and_remove(subscription).await;
                return Ok(());
            }
            if subscription
                .outbound
                .send(Message::Text(
                    event_message(&subscription.sub_id, &event).into(),
                ))
                .await
                .is_err()
            {
                subscription.runtime.lock().await.cancelled = true;
                self.remove_subscriptions(&[(
                    subscription.connection_id,
                    subscription.sub_id.clone(),
                )]);
                return Ok(());
            }
        }
        Ok(())
    }

    async fn cancel_and_remove(&self, subscription: &Subscription) {
        self.cancel_subscription(
            subscription,
            "restricted: authorization changed; reconnect to canonical relay",
        )
        .await;
        self.remove_subscriptions(&[(subscription.connection_id, subscription.sub_id.clone())]);
    }

    async fn cancel_subscription(&self, subscription: &Subscription, reason: &str) {
        let mut runtime = subscription.runtime.lock().await;
        if runtime.cancelled {
            return;
        }
        runtime.cancelled = true;
        drop(runtime);
        let _ = subscription
            .outbound
            .send(Message::Text(closed(&subscription.sub_id, reason).into()))
            .await;
    }

    fn remove_subscriptions(&self, removed: &[(u64, String)]) {
        self.state.subscriptions.lock().retain(|subscription| {
            !removed.iter().any(|(connection_id, sub_id)| {
                *connection_id == subscription.connection_id && *sub_id == subscription.sub_id
            })
        });
    }

    async fn principal_can_access(
        &self,
        channel_id: Uuid,
        principal: PublicKey,
    ) -> Result<bool, EdgeError> {
        let store = Arc::clone(&self.state.store);
        tokio::task::spawn_blocking(move || store.principal_can_access(channel_id, &principal))
            .await
            .map_err(|error| EdgeError::Join(error.to_string()))?
            .map_err(EdgeError::from)
    }

    async fn authorize_filters(
        &self,
        filters: &[Filter],
        principal: PublicKey,
    ) -> Result<(), String> {
        let channels = filter_channels(filters)?;
        for channel in channels {
            match self.principal_can_access(channel, principal).await {
                Ok(true) => {}
                Ok(false) => {
                    return Err(
                        "restricted: channel is not selected or principal is not a cached member"
                            .to_string(),
                    )
                }
                Err(_) => return Err("error: membership cache unavailable".to_string()),
            }
        }
        Ok(())
    }

    async fn query(&self, filters: Vec<Filter>) -> Result<Vec<Event>, EdgeError> {
        let store = Arc::clone(&self.state.store);
        tokio::task::spawn_blocking(move || store.query(&filters))
            .await
            .map_err(|error| EdgeError::Join(error.to_string()))?
            .map_err(EdgeError::from)
    }

    async fn count(&self, filters: Vec<Filter>) -> Result<u64, EdgeError> {
        let store = Arc::clone(&self.state.store);
        tokio::task::spawn_blocking(move || store.count(&filters))
            .await
            .map_err(|error| EdgeError::Join(error.to_string()))?
            .map_err(EdgeError::from)
    }

    async fn verified_owner(
        &self,
        principal: PublicKey,
        auth_tag: Option<String>,
    ) -> Result<(), String> {
        let Some(auth_tag) = auth_tag else {
            return Ok(());
        };
        let owner = tokio::task::spawn_blocking(move || {
            buzz_sdk::nip_oa::verify_auth_tag(&auth_tag, &principal)
        })
        .await
        .map_err(|_| "NIP-OA verification task failed".to_string())?
        .map_err(|_| "invalid NIP-OA owner attestation".to_string())?;
        let store = Arc::clone(&self.state.store);
        tokio::task::spawn_blocking(move || store.record_nip_oa_owner(&principal, &owner))
            .await
            .map_err(|_| "NIP-OA cache task failed".to_string())?
            .map_err(|_| "conflicting NIP-OA owner attestation".to_string())?;
        Ok(())
    }

    fn remove_connection(&self, connection_id: u64) {
        self.state
            .subscriptions
            .lock()
            .retain(|subscription| subscription.connection_id != connection_id);
    }
}

/// Serve WebSocket, `/events`, `/query`, and `/count` on a loopback listener.
pub async fn run_server(listener: TcpListener, relay: EdgeRelay) -> Result<(), EdgeError> {
    let address = listener.local_addr()?;
    if !address.ip().is_loopback() {
        return Err(EdgeError::Config(format!(
            "refusing non-loopback listener {address}"
        )));
    }
    let monitor_relay = relay.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(AUTHORIZATION_SWEEP_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if let Err(error) = monitor_relay.close_ineligible_subscriptions().await {
                tracing::warn!(%error, "authorization subscription sweep failed");
            }
        }
    });
    let app = Router::new()
        .route("/", get(websocket_upgrade))
        .route("/events", post(http_submit_event))
        .route("/query", post(http_query))
        .route("/count", post(http_count))
        .layer(axum::extract::DefaultBodyLimit::max(MAX_MESSAGE_BYTES))
        .with_state(relay);
    axum::serve(listener, app).await?;
    Ok(())
}

async fn websocket_upgrade(State(relay): State<EdgeRelay>, upgrade: WebSocketUpgrade) -> Response {
    let permit = match Arc::clone(&relay.state.connections).try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    upgrade
        .max_message_size(MAX_MESSAGE_BYTES)
        .max_frame_size(MAX_MESSAGE_BYTES)
        .on_upgrade(move |socket| handle_websocket(relay, socket, permit))
}

async fn handle_websocket(relay: EdgeRelay, socket: WebSocket, _permit: OwnedSemaphorePermit) {
    let connection_id = relay
        .state
        .next_connection_id
        .fetch_add(1, Ordering::Relaxed);
    let challenge = buzz_auth::generate_challenge();
    let (mut sink, mut source) = socket.split();
    let (outbound, mut outbound_rx) = mpsc::channel::<Message>(OUTBOUND_CAPACITY);
    let writer = tokio::spawn(async move {
        while let Some(message) = outbound_rx.recv().await {
            match tokio::time::timeout(Duration::from_secs(5), sink.send(message)).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) | Err(_) => break,
            }
        }
    });

    if outbound
        .send(Message::Text(auth_challenge(&challenge).into()))
        .await
        .is_err()
    {
        relay.remove_connection(connection_id);
        return;
    }

    let mut bound = false;
    let mut authenticated: Option<PublicKey> = None;
    let authentication_deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let next_frame = if authenticated.is_none() {
            let remaining = authentication_deadline
                .checked_duration_since(tokio::time::Instant::now())
                .unwrap_or(Duration::ZERO);
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, source.next()).await {
                Ok(frame) => frame,
                Err(_) => break,
            }
        } else {
            source.next().await
        };
        let Some(frame) = next_frame else {
            break;
        };
        let frame = match frame {
            Ok(frame) => frame,
            Err(_) => break,
        };
        match frame {
            Message::Text(raw) => {
                let message = match parse_client_message(raw.as_str()) {
                    Ok(message) => message,
                    Err(error) => {
                        let _ = outbound
                            .send(Message::Text(notice(&format!("error: {error}")).into()))
                            .await;
                        continue;
                    }
                };
                if let ClientMessage::Handshake {
                    canonical_origin,
                    community_id,
                } = message
                {
                    if bound || authenticated.is_some() {
                        let _ = outbound
                            .send(Message::Text(
                                binding_result(false, "binding handshake already completed").into(),
                            ))
                            .await;
                        break;
                    }
                    if !relay
                        .state
                        .config
                        .binding_matches(&canonical_origin, community_id)
                    {
                        let _ = outbound
                            .send(Message::Text(
                                binding_result(false, "canonical relay/community binding mismatch")
                                    .into(),
                            ))
                            .await;
                        break;
                    }
                    bound = true;
                    let _ = outbound
                        .send(Message::Text(binding_result(true, "").into()))
                        .await;
                    continue;
                }
                if !bound {
                    let _ = outbound
                        .send(Message::Text(
                            notice("binding-required: complete BUZZ-EDGE BIND first").into(),
                        ))
                        .await;
                    continue;
                }
                if let ClientMessage::Auth(event) = message {
                    if authenticated.is_some() {
                        let _ = outbound
                            .send(Message::Text(
                                ok(
                                    &event.id.to_hex(),
                                    false,
                                    "auth-required: already authenticated",
                                )
                                .into(),
                            ))
                            .await;
                        continue;
                    }
                    let auth_event = event.clone();
                    let challenge_to_verify = challenge.clone();
                    let relay_url = relay.state.config.websocket_url().to_string();
                    let auth_result = tokio::task::spawn_blocking(move || {
                        buzz_auth::verify_nip42_event(&auth_event, &challenge_to_verify, &relay_url)
                    })
                    .await;
                    if !matches!(auth_result, Ok(Ok(()))) {
                        let _ = outbound
                            .send(Message::Text(
                                ok(
                                    &event.id.to_hex(),
                                    false,
                                    "auth-required: verification failed",
                                )
                                .into(),
                            ))
                            .await;
                        continue;
                    }
                    let tag = match auth_tag_json(&event) {
                        Ok(tag) => tag,
                        Err(error) => {
                            let _ = outbound
                                .send(Message::Text(ok(&event.id.to_hex(), false, &error).into()))
                                .await;
                            continue;
                        }
                    };
                    match relay.verified_owner(event.pubkey, tag).await {
                        Ok(()) => {}
                        Err(error) => {
                            let _ = outbound
                                .send(Message::Text(ok(&event.id.to_hex(), false, &error).into()))
                                .await;
                            continue;
                        }
                    }
                    authenticated = Some(event.pubkey);
                    let _ = outbound
                        .send(Message::Text(ok(&event.id.to_hex(), true, "").into()))
                        .await;
                    continue;
                }

                let Some(principal) = authenticated else {
                    let _ = outbound
                        .send(Message::Text(
                            notice("auth-required: complete NIP-42 authentication first").into(),
                        ))
                        .await;
                    continue;
                };

                match message {
                    ClientMessage::Event(event) => {
                        let event_id = event.id.to_hex();
                        let bytes = match serde_json::to_vec(&event) {
                            Ok(bytes) => bytes,
                            Err(_) => {
                                let _ = outbound
                                    .send(Message::Text(
                                        ok(&event_id, false, "invalid: event serialization failed")
                                            .into(),
                                    ))
                                    .await;
                                continue;
                            }
                        };
                        let response = relay.accept_event(principal, event, bytes).await;
                        let (accepted, message) = match response {
                            Ok(response) => response,
                            Err(_) => (false, "error: local persistence failed".to_string()),
                        };
                        let _ = outbound
                            .send(Message::Text(ok(&event_id, accepted, &message).into()))
                            .await;
                    }
                    ClientMessage::Req { sub_id, filters } => {
                        let subscription;
                        {
                            let _sequence = relay.state.sequence.lock().await;
                            let _authorization = relay.state.authorization_epoch.read().await;
                            if let Err(error) = relay.authorize_filters(&filters, principal).await {
                                let _ = outbound
                                    .send(Message::Text(closed(&sub_id, &error).into()))
                                    .await;
                                continue;
                            }
                            if relay.state.subscriptions.lock().iter().any(|sub| {
                                sub.connection_id == connection_id && sub.sub_id == sub_id
                            }) {
                                let _ = outbound
                                    .send(Message::Text(
                                        closed(&sub_id, "duplicate subscription ID").into(),
                                    ))
                                    .await;
                                continue;
                            }
                            subscription = Subscription {
                                connection_id,
                                sub_id: sub_id.clone(),
                                filters: filters.clone(),
                                principal,
                                outbound: outbound.clone(),
                                runtime: Arc::new(tokio::sync::Mutex::new(SubscriptionRuntime {
                                    cancelled: false,
                                    backfilling: true,
                                    buffered_live: Vec::new(),
                                })),
                                send_gate: Arc::new(tokio::sync::Mutex::new(())),
                            };
                            relay.state.subscriptions.lock().push(subscription.clone());
                        }
                        let history = match relay.query(filters.clone()).await {
                            Ok(history) => history,
                            Err(_) => {
                                let _gate = subscription.send_gate.lock().await;
                                relay
                                    .cancel_subscription(&subscription, "error: local query failed")
                                    .await;
                                relay.remove_subscriptions(&[(connection_id, sub_id)]);
                                continue;
                            }
                        };
                        if let Err(error) = relay.deliver_history(&subscription, history).await {
                            tracing::warn!(%error, "edge history delivery failed");
                        }
                    }
                    ClientMessage::Count { sub_id, filters } => {
                        let _authorization = relay.state.authorization_epoch.read().await;
                        if let Err(error) = relay.authorize_filters(&filters, principal).await {
                            let _ = outbound
                                .send(Message::Text(closed(&sub_id, &error).into()))
                                .await;
                            continue;
                        }
                        let value = relay.count(filters).await;
                        match value {
                            Ok(value) => {
                                let _ = outbound
                                    .send(Message::Text(count(&sub_id, value).into()))
                                    .await;
                            }
                            Err(_) => {
                                let _ = outbound
                                    .send(Message::Text(
                                        closed(&sub_id, "error: local count failed").into(),
                                    ))
                                    .await;
                            }
                        }
                    }
                    ClientMessage::Close(sub_id) => {
                        relay.state.subscriptions.lock().retain(|subscription| {
                            subscription.connection_id != connection_id
                                || subscription.sub_id != sub_id
                        })
                    }
                    ClientMessage::Auth(_) | ClientMessage::Handshake { .. } => {}
                }
            }
            Message::Ping(payload) => {
                let _ = outbound.send(Message::Pong(payload)).await;
            }
            Message::Pong(_) => {}
            Message::Close(_) => break,
            Message::Binary(_) => break,
        }
    }

    relay.remove_connection(connection_id);
    drop(outbound);
    let _ = tokio::time::timeout(Duration::from_millis(250), writer).await;
}

type ApiResult = Result<Json<Value>, (StatusCode, Json<Value>)>;

async fn http_submit_event(
    State(relay): State<EdgeRelay>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult {
    let principal = authenticate_http(&relay, &headers, &body, "/events").await?;
    let event: Event = serde_json::from_slice(&body)
        .map_err(|_| api_error(StatusCode::BAD_REQUEST, "invalid event JSON"))?;
    let event_id = event.id.to_hex();
    let (accepted, message) = relay
        .accept_event(principal, event, body.to_vec())
        .await
        .map_err(|_| {
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "local persistence failed",
            )
        })?;
    if !accepted {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "event_id": event_id,
                "accepted": false,
                "message": message,
            })),
        ));
    }
    Ok(Json(serde_json::json!({
        "event_id": event_id,
        "accepted": true,
        "message": message,
    })))
}

async fn http_query(State(relay): State<EdgeRelay>, headers: HeaderMap, body: Bytes) -> ApiResult {
    let principal = authenticate_http(&relay, &headers, &body, "/query").await?;
    let filters: Vec<Filter> = serde_json::from_slice(&body)
        .map_err(|_| api_error(StatusCode::BAD_REQUEST, "invalid filters"))?;
    let filters =
        bounded_filters(filters).map_err(|error| api_error(StatusCode::BAD_REQUEST, &error))?;
    let _authorization = relay.state.authorization_epoch.read().await;
    relay
        .authorize_filters(&filters, principal)
        .await
        .map_err(|error| api_error(StatusCode::FORBIDDEN, &error))?;
    let events = relay
        .query(filters)
        .await
        .map_err(|_| api_error(StatusCode::INTERNAL_SERVER_ERROR, "local query failed"))?;
    Ok(Json(serde_json::json!(events)))
}

async fn http_count(State(relay): State<EdgeRelay>, headers: HeaderMap, body: Bytes) -> ApiResult {
    let principal = authenticate_http(&relay, &headers, &body, "/count").await?;
    let filters: Vec<Filter> = serde_json::from_slice(&body)
        .map_err(|_| api_error(StatusCode::BAD_REQUEST, "invalid filters"))?;
    let filters =
        bounded_filters(filters).map_err(|error| api_error(StatusCode::BAD_REQUEST, &error))?;
    let _authorization = relay.state.authorization_epoch.read().await;
    relay
        .authorize_filters(&filters, principal)
        .await
        .map_err(|error| api_error(StatusCode::FORBIDDEN, &error))?;
    let value = relay
        .count(filters)
        .await
        .map_err(|_| api_error(StatusCode::INTERNAL_SERVER_ERROR, "local count failed"))?;
    Ok(Json(serde_json::json!({"count": value})))
}

async fn authenticate_http(
    relay: &EdgeRelay,
    headers: &HeaderMap,
    body: &[u8],
    path: &str,
) -> Result<PublicKey, (StatusCode, Json<Value>)> {
    validate_http_binding(relay, headers)?;
    let encoded = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Nostr "))
        .ok_or_else(|| api_error(StatusCode::UNAUTHORIZED, "missing Nostr auth"))?;
    let event_json = BASE64
        .decode(encoded)
        .map_err(|_| api_error(StatusCode::UNAUTHORIZED, "invalid Nostr auth"))?;
    let event_json = String::from_utf8(event_json)
        .map_err(|_| api_error(StatusCode::UNAUTHORIZED, "invalid Nostr auth"))?;
    let auth_event: Event = serde_json::from_str(&event_json)
        .map_err(|_| api_error(StatusCode::UNAUTHORIZED, "invalid Nostr auth"))?;
    if !auth_event
        .tags
        .iter()
        .any(|tag| tag.kind() == TagKind::Payload)
    {
        return Err(api_error(
            StatusCode::UNAUTHORIZED,
            "NIP-98 payload tag required",
        ));
    }
    let expected_url = relay
        .state
        .config
        .http_url(path)
        .map_err(|_| api_error(StatusCode::INTERNAL_SERVER_ERROR, "invalid edge URL"))?;
    let event_json_to_verify = event_json.clone();
    let body_to_verify = body.to_vec();
    let principal = tokio::task::spawn_blocking(move || {
        buzz_auth::verify_nip98_event(
            &event_json_to_verify,
            &expected_url,
            "POST",
            Some(&body_to_verify),
        )
    })
    .await
    .map_err(|_| api_error(StatusCode::UNAUTHORIZED, "NIP-98 verification failed"))?
    .map_err(|_| api_error(StatusCode::UNAUTHORIZED, "NIP-98 verification failed"))?;

    let auth_tag = headers
        .get("x-auth-tag")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    relay
        .verified_owner(principal, auth_tag)
        .await
        .map_err(|error| api_error(StatusCode::UNAUTHORIZED, &error))?;
    let store = Arc::clone(&relay.state.store);
    let auth_event_id = auth_event.id;
    let fresh = tokio::task::spawn_blocking(move || store.mark_nip98_auth(&auth_event_id))
        .await
        .map_err(|_| api_error(StatusCode::UNAUTHORIZED, "NIP-98 replay check failed"))?
        .map_err(|_| api_error(StatusCode::UNAUTHORIZED, "NIP-98 replay check failed"))?;
    if !fresh {
        return Err(api_error(
            StatusCode::UNAUTHORIZED,
            "NIP-98 replay detected",
        ));
    }
    Ok(principal)
}

fn validate_http_binding(
    relay: &EdgeRelay,
    headers: &HeaderMap,
) -> Result<(), (StatusCode, Json<Value>)> {
    let canonical_origin = headers
        .get("x-buzz-canonical-origin")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| api_error(StatusCode::PRECONDITION_REQUIRED, "missing edge binding"))?;
    let community_id = headers
        .get("x-buzz-community-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<Uuid>().ok())
        .ok_or_else(|| api_error(StatusCode::PRECONDITION_REQUIRED, "invalid edge binding"))?;
    if !relay
        .state
        .config
        .binding_matches(canonical_origin, community_id)
    {
        return Err(api_error(
            StatusCode::MISDIRECTED_REQUEST,
            "canonical relay/community binding mismatch",
        ));
    }
    Ok(())
}

fn api_error(status: StatusCode, message: &str) -> (StatusCode, Json<Value>) {
    (status, Json(serde_json::json!({"error": message})))
}

fn url_host_is_loopback(url: &url::Url) -> bool {
    match url.host() {
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        Some(url::Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        None => false,
    }
}

/// Return whether a socket address is safe for direct sidecar binding.
pub fn socket_is_loopback(address: SocketAddr) -> bool {
    match address.ip() {
        IpAddr::V4(ip) => ip.is_loopback(),
        IpAddr::V6(ip) => ip.is_loopback(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use nostr::{EventBuilder, JsonUtil, Kind, Tag, Timestamp};
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use tokio_tungstenite::{
        connect_async, tungstenite::Message as TungsteniteMessage, MaybeTlsStream, WebSocketStream,
    };

    type ClientSocket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

    fn test_policy() -> storage::AuthorizationPolicy {
        storage::AuthorizationPolicy::new(Duration::from_secs(72 * 60 * 60)).expect("policy")
    }

    fn test_now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_secs() as i64
    }

    async fn text(socket: &mut ClientSocket) -> Value {
        let message = socket
            .next()
            .await
            .expect("server frame")
            .expect("valid frame");
        let TungsteniteMessage::Text(message) = message else {
            panic!("expected text frame");
        };
        serde_json::from_str(&message).expect("valid JSON")
    }

    async fn authenticated_client(
        url: &str,
        binding: &CommunityBinding,
        keys: &Keys,
    ) -> ClientSocket {
        let (mut socket, _) = connect_async(url).await.expect("connect");
        let challenge = text(&mut socket).await;
        let challenge = challenge
            .get(1)
            .and_then(Value::as_str)
            .expect("AUTH challenge");
        socket
            .send(TungsteniteMessage::Text(
                json!([
                    "BUZZ-EDGE",
                    "BIND",
                    {
                        "canonical_origin": binding.canonical_origin(),
                        "community_id": binding.community_id(),
                    }
                ])
                .to_string()
                .into(),
            ))
            .await
            .expect("binding");
        assert_eq!(text(&mut socket).await[2], true);
        let auth =
            buzz_ws_client::build_auth_event(challenge, url, keys, None).expect("auth event");
        socket
            .send(TungsteniteMessage::Text(
                json!(["AUTH", auth]).to_string().into(),
            ))
            .await
            .expect("auth");
        assert_eq!(text(&mut socket).await[2], true);
        socket
    }

    async fn test_server(
        binding: CommunityBinding,
        store: Arc<EdgeStore>,
        edge_keys: Keys,
    ) -> (String, tokio::task::JoinHandle<Result<(), EdgeError>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let url = format!("ws://{address}");
        let config = EdgeConfig::new(&url, binding).expect("config");
        let relay = EdgeRelay::new(config, store, edge_keys).expect("relay");
        let task = tokio::spawn(run_server(listener, relay));
        (url, task)
    }

    fn authorization(
        channel_id: Uuid,
        authors: &[PublicKey],
        verified_at: i64,
    ) -> storage::VerifiedChannelAuthorization {
        let mut tags = vec![Tag::parse(["d", channel_id.to_string().as_str()]).expect("d tag")];
        tags.extend(
            authors
                .iter()
                .map(|author| Tag::parse(["p", author.to_hex().as_str()]).expect("p tag")),
        );
        let membership = EventBuilder::new(Kind::Custom(39_002), "")
            .tags(tags)
            .custom_created_at(Timestamp::from(verified_at as u64))
            .sign_with_keys(&Keys::generate())
            .expect("membership");
        storage::VerifiedChannelAuthorization {
            channel_id,
            membership_event_id: membership.id,
            membership_event_created_at: verified_at,
            membership_event_bytes: membership.as_json().into_bytes(),
            active_authors: authors.to_vec(),
        }
    }

    fn nip98_authorization(keys: &Keys, url: &str, body: &[u8]) -> String {
        let payload = hex::encode(Sha256::digest(body));
        let nonce = Uuid::new_v4().to_string();
        let auth = EventBuilder::new(Kind::HttpAuth, "")
            .tags([
                Tag::parse(["u", url]).expect("u tag"),
                Tag::parse(["method", "POST"]).expect("method tag"),
                Tag::parse(["payload", payload.as_str()]).expect("payload tag"),
                Tag::parse(["nonce", nonce.as_str()]).expect("nonce tag"),
            ])
            .sign_with_keys(keys)
            .expect("NIP-98");
        format!("Nostr {}", BASE64.encode(auth.as_json()))
    }

    struct GatedFixture {
        binding: CommunityBinding,
        store: Arc<EdgeStore>,
        edge_keys: Keys,
        reader: Keys,
        author: Keys,
        channel: Uuid,
        relay: EdgeRelay,
        gate: Arc<HistoryGate>,
        url: String,
        server: tokio::task::JoinHandle<Result<(), EdgeError>>,
    }

    async fn gated_fixture(policy: storage::AuthorizationPolicy) -> GatedFixture {
        let binding =
            CommunityBinding::new("wss://relay.example.com", Uuid::new_v4()).expect("binding");
        let store = Arc::new(EdgeStore::open_in_memory(binding.clone(), policy).expect("store"));
        let edge_keys = Keys::generate();
        let reader = Keys::generate();
        let author = Keys::generate();
        let channel = Uuid::new_v4();
        store
            .set_channel_selected(channel, true)
            .expect("selection");
        let now = test_now();
        store
            .persist_verified_authorization_snapshot(
                &[authorization(
                    channel,
                    &[
                        edge_keys.public_key(),
                        reader.public_key(),
                        author.public_key(),
                    ],
                    now,
                )],
                now,
                &edge_keys,
            )
            .expect("authorization");
        let historical = EventBuilder::new(Kind::Custom(9), "historical")
            .tags([Tag::parse(["h", channel.to_string().as_str()]).expect("h")])
            .sign_with_keys(&author)
            .expect("historical");
        store
            .insert_local_event(
                &historical,
                historical.as_json().as_bytes(),
                channel,
                &edge_keys,
            )
            .expect("historical insert");

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let url = format!("ws://{address}");
        let relay = EdgeRelay::new(
            EdgeConfig::new(&url, binding.clone()).expect("config"),
            Arc::clone(&store),
            edge_keys.clone(),
        )
        .expect("relay");
        let gate = Arc::new(HistoryGate::default());
        *relay.state.history_gate.lock() = Some(Arc::clone(&gate));
        let server = tokio::spawn(run_server(listener, relay.clone()));
        GatedFixture {
            binding,
            store,
            edge_keys,
            reader,
            author,
            channel,
            relay,
            gate,
            url,
            server,
        }
    }

    async fn begin_gated_subscription(fixture: &GatedFixture) -> ClientSocket {
        let mut socket =
            authenticated_client(&fixture.url, &fixture.binding, &fixture.reader).await;
        socket
            .send(TungsteniteMessage::Text(
                json!(["REQ", "gated", {"kinds":[9], "#h":[fixture.channel]}])
                    .to_string()
                    .into(),
            ))
            .await
            .expect("subscribe");
        fixture.gate.entered.notified().await;
        socket
    }

    #[tokio::test]
    async fn kind_nine_round_trip_fans_out_and_counts_without_upstream() {
        let binding =
            CommunityBinding::new("wss://relay.example.com", Uuid::new_v4()).expect("binding");
        let store =
            Arc::new(EdgeStore::open_in_memory(binding.clone(), test_policy()).expect("store"));
        let edge_keys = Keys::generate();
        let sender_keys = Keys::generate();
        let receiver_keys = Keys::generate();
        let channel_id = Uuid::new_v4();
        store
            .set_channel_selected(channel_id, true)
            .expect("select");
        let verified_at = test_now();
        let membership = EventBuilder::new(Kind::Custom(39_002), "")
            .tags([
                Tag::parse(["d", channel_id.to_string().as_str()]).expect("d tag"),
                Tag::parse(["p", edge_keys.public_key().to_hex().as_str()]).expect("edge member"),
                Tag::parse(["p", sender_keys.public_key().to_hex().as_str()])
                    .expect("sender member"),
                Tag::parse(["p", receiver_keys.public_key().to_hex().as_str()])
                    .expect("receiver member"),
            ])
            .custom_created_at(Timestamp::from(verified_at as u64))
            .sign_with_keys(&Keys::generate())
            .expect("membership");
        let channel_authorization = storage::VerifiedChannelAuthorization {
            channel_id,
            membership_event_id: membership.id,
            membership_event_created_at: verified_at,
            membership_event_bytes: membership.as_json().into_bytes(),
            active_authors: vec![
                edge_keys.public_key(),
                sender_keys.public_key(),
                receiver_keys.public_key(),
            ],
        };
        store
            .persist_verified_authorization_snapshot(
                &[channel_authorization],
                verified_at,
                &edge_keys,
            )
            .expect("eligibility");

        let (url, server) = test_server(binding.clone(), store, edge_keys).await;
        let mut receiver = authenticated_client(&url, &binding, &receiver_keys).await;
        receiver
            .send(TungsteniteMessage::Text(
                json!([
                    "REQ",
                    "messages",
                    {"kinds":[9], "#h":[channel_id]}
                ])
                .to_string()
                .into(),
            ))
            .await
            .expect("subscribe");
        assert_eq!(text(&mut receiver).await[0], "EOSE");

        let mut sender = authenticated_client(&url, &binding, &sender_keys).await;
        let event = EventBuilder::new(Kind::Custom(9), "local message")
            .tags([Tag::parse(["h", channel_id.to_string().as_str()]).expect("channel tag")])
            .sign_with_keys(&sender_keys)
            .expect("sign");
        sender
            .send(TungsteniteMessage::Text(
                json!(["EVENT", event]).to_string().into(),
            ))
            .await
            .expect("submit");
        let accepted = text(&mut sender).await;
        assert_eq!(accepted[2], true);
        assert_eq!(accepted[3], "delivered locally");

        let delivered = text(&mut receiver).await;
        assert_eq!(delivered[0], "EVENT");
        assert_eq!(delivered[2]["id"], event.id.to_hex());
        receiver
            .send(TungsteniteMessage::Text(
                json!([
                    "COUNT",
                    "count",
                    {"kinds":[9], "#h":[channel_id]}
                ])
                .to_string()
                .into(),
            ))
            .await
            .expect("count");
        assert_eq!(text(&mut receiver).await[2]["count"], 1);

        server.abort();
    }

    #[tokio::test]
    async fn canonical_mirror_fans_out_without_creating_a_local_outbox_row() {
        let binding =
            CommunityBinding::new("wss://relay.example.com", Uuid::new_v4()).expect("binding");
        let store =
            Arc::new(EdgeStore::open_in_memory(binding.clone(), test_policy()).expect("store"));
        let edge_keys = Keys::generate();
        let receiver_keys = Keys::generate();
        let historical_author = Keys::generate();
        let channel_id = Uuid::new_v4();
        let verified_at = test_now();
        store
            .set_channel_selected(channel_id, true)
            .expect("select");
        let membership = EventBuilder::new(Kind::Custom(39_002), "")
            .tags([
                Tag::parse(["d", channel_id.to_string().as_str()]).expect("d tag"),
                Tag::parse(["p", edge_keys.public_key().to_hex().as_str()]).expect("edge member"),
                Tag::parse(["p", receiver_keys.public_key().to_hex().as_str()])
                    .expect("receiver member"),
            ])
            .custom_created_at(Timestamp::from(verified_at as u64))
            .sign_with_keys(&Keys::generate())
            .expect("membership");
        store
            .persist_verified_authorization_snapshot(
                &[storage::VerifiedChannelAuthorization {
                    channel_id,
                    membership_event_id: membership.id,
                    membership_event_created_at: verified_at,
                    membership_event_bytes: membership.as_json().into_bytes(),
                    active_authors: vec![edge_keys.public_key(), receiver_keys.public_key()],
                }],
                verified_at,
                &edge_keys,
            )
            .expect("eligibility");

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let url = format!("ws://{address}");
        let relay = EdgeRelay::new(
            EdgeConfig::new(&url, binding.clone()).expect("config"),
            Arc::clone(&store),
            edge_keys,
        )
        .expect("relay");
        let server = tokio::spawn(run_server(listener, relay.clone()));
        let mut receiver = authenticated_client(&url, &binding, &receiver_keys).await;
        receiver
            .send(TungsteniteMessage::Text(
                json!(["REQ", "messages", {"kinds":[9], "#h":[channel_id]}])
                    .to_string()
                    .into(),
            ))
            .await
            .expect("subscribe");
        assert_eq!(text(&mut receiver).await[0], "EOSE");

        let event = EventBuilder::new(Kind::Custom(9), "from canonical")
            .tags([Tag::parse(["h", channel_id.to_string().as_str()]).expect("h")])
            .sign_with_keys(&historical_author)
            .expect("event");
        assert_eq!(
            relay
                .accept_upstream_event(event.clone(), event.as_json().into_bytes())
                .await
                .expect("mirror"),
            InsertOutcome::Inserted
        );
        let delivered = text(&mut receiver).await;
        assert_eq!(delivered[2]["id"], event.id.to_hex());
        assert_eq!(store.pending_count().expect("pending"), 0);
        assert!(store.receipt(&event.id).expect("receipt").is_none());
        server.abort();
    }

    #[tokio::test]
    async fn authorization_refresh_closes_removed_author_and_channel_subscriptions() {
        let binding =
            CommunityBinding::new("wss://relay.example.com", Uuid::new_v4()).expect("binding");
        let store =
            Arc::new(EdgeStore::open_in_memory(binding.clone(), test_policy()).expect("store"));
        let edge_keys = Keys::generate();
        let reader = Keys::generate();
        let author_removed = Uuid::new_v4();
        let channel_removed = Uuid::new_v4();
        store
            .replace_selected_channels(&[author_removed, channel_removed])
            .expect("selection");
        let now = test_now();
        let initial = vec![
            authorization(
                author_removed,
                &[edge_keys.public_key(), reader.public_key()],
                now,
            ),
            authorization(
                channel_removed,
                &[edge_keys.public_key(), reader.public_key()],
                now,
            ),
        ];
        store
            .persist_verified_authorization_snapshot(&initial, now, &edge_keys)
            .expect("initial authorization");

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let url = format!("ws://{address}");
        let relay = EdgeRelay::new(
            EdgeConfig::new(&url, binding.clone()).expect("config"),
            Arc::clone(&store),
            edge_keys.clone(),
        )
        .expect("relay");
        let server = tokio::spawn(run_server(listener, relay.clone()));
        let mut socket = authenticated_client(&url, &binding, &reader).await;
        for (sub_id, channel) in [
            ("author-removed", author_removed),
            ("channel-removed", channel_removed),
        ] {
            socket
                .send(TungsteniteMessage::Text(
                    json!(["REQ", sub_id, {"kinds":[9], "#h":[channel]}])
                        .to_string()
                        .into(),
                ))
                .await
                .expect("subscribe");
            assert_eq!(text(&mut socket).await[0], "EOSE");
        }

        relay
            .apply_authorization_refresh(
                now + 1,
                eligibility::VerificationResult::Verified(vec![authorization(
                    author_removed,
                    &[edge_keys.public_key()],
                    now + 1,
                )]),
            )
            .await
            .expect("refresh");
        let first = text(&mut socket).await;
        let second = text(&mut socket).await;
        assert_eq!(first[0], "CLOSED");
        assert_eq!(second[0], "CLOSED");
        let ids = [first[1].as_str(), second[1].as_str()];
        assert!(ids.contains(&Some("author-removed")));
        assert!(ids.contains(&Some("channel-removed")));
        server.abort();
    }

    #[tokio::test]
    async fn lease_expiry_closes_existing_subscription() {
        let binding =
            CommunityBinding::new("wss://relay.example.com", Uuid::new_v4()).expect("binding");
        let policy = storage::AuthorizationPolicy::new(Duration::from_secs(1)).expect("policy");
        let store = Arc::new(EdgeStore::open_in_memory(binding.clone(), policy).expect("store"));
        let edge_keys = Keys::generate();
        let reader = Keys::generate();
        let channel = Uuid::new_v4();
        store
            .set_channel_selected(channel, true)
            .expect("selection");
        let now = test_now();
        store
            .persist_verified_authorization_snapshot(
                &[authorization(
                    channel,
                    &[edge_keys.public_key(), reader.public_key()],
                    now,
                )],
                now,
                &edge_keys,
            )
            .expect("authorization");
        let (url, server) = test_server(binding.clone(), store, edge_keys).await;
        let mut socket = authenticated_client(&url, &binding, &reader).await;
        socket
            .send(TungsteniteMessage::Text(
                json!(["REQ", "lease", {"kinds":[9], "#h":[channel]}])
                    .to_string()
                    .into(),
            ))
            .await
            .expect("subscribe");
        assert_eq!(text(&mut socket).await[0], "EOSE");
        let closed = tokio::time::timeout(Duration::from_secs(4), text(&mut socket))
            .await
            .expect("lease closure");
        assert_eq!(closed[0], "CLOSED");
        assert_eq!(closed[1], "lease");
        server.abort();
    }

    #[tokio::test]
    async fn event_authentication_and_transport_content_boundaries_are_enforced() {
        let binding =
            CommunityBinding::new("wss://relay.example.com", Uuid::new_v4()).expect("binding");
        let store =
            Arc::new(EdgeStore::open_in_memory(binding.clone(), test_policy()).expect("store"));
        let edge_keys = Keys::generate();
        let author = Keys::generate();
        let stranger = Keys::generate();
        let channel = Uuid::new_v4();
        store
            .set_channel_selected(channel, true)
            .expect("selection");
        let now = test_now();
        store
            .persist_verified_authorization_snapshot(
                &[authorization(
                    channel,
                    &[edge_keys.public_key(), author.public_key()],
                    now,
                )],
                now,
                &edge_keys,
            )
            .expect("authorization");
        let relay = EdgeRelay::new(
            EdgeConfig::new("ws://127.0.0.1:3031", binding).expect("config"),
            store,
            edge_keys,
        )
        .expect("relay");

        let normal = EventBuilder::new(Kind::Custom(9), "signed")
            .tags([Tag::parse(["h", channel.to_string().as_str()]).expect("h")])
            .sign_with_keys(&author)
            .expect("event");
        let (accepted, message) = relay
            .accept_event(
                stranger.public_key(),
                normal.clone(),
                normal.as_json().into_bytes(),
            )
            .await
            .expect("principal check");
        assert!(!accepted);
        assert!(message.contains("authenticated principal"));

        let mut tampered_value: Value = serde_json::from_str(&normal.as_json()).expect("json");
        tampered_value["content"] = json!("tampered");
        let tampered: Event = serde_json::from_value(tampered_value).expect("event shape");
        let (accepted, message) = relay
            .accept_event(
                author.public_key(),
                tampered.clone(),
                tampered.as_json().into_bytes(),
            )
            .await
            .expect("signature check");
        assert!(!accepted);
        assert!(message.contains("signature"));

        let boundary = EventBuilder::new(Kind::Custom(9), "\0".repeat(MAX_EVENT_CONTENT_BYTES))
            .tags([Tag::parse(["h", channel.to_string().as_str()]).expect("h")])
            .sign_with_keys(&author)
            .expect("boundary event");
        assert!(event_message("boundary", &boundary).len() <= MAX_MESSAGE_BYTES);
        assert!(
            relay
                .accept_event(
                    author.public_key(),
                    boundary.clone(),
                    boundary.as_json().into_bytes(),
                )
                .await
                .expect("boundary")
                .0
        );

        let oversized = EventBuilder::new(Kind::Custom(9), "x".repeat(MAX_EVENT_CONTENT_BYTES + 1))
            .tags([Tag::parse(["h", channel.to_string().as_str()]).expect("h")])
            .sign_with_keys(&author)
            .expect("oversized event");
        let (accepted, message) = relay
            .accept_event(
                author.public_key(),
                oversized.clone(),
                oversized.as_json().into_bytes(),
            )
            .await
            .expect("oversized");
        assert!(!accepted);
        assert!(message.contains("256 KiB"));
    }

    #[tokio::test]
    async fn mismatched_community_handshake_is_rejected_before_auth() {
        let binding =
            CommunityBinding::new("wss://relay.example.com", Uuid::new_v4()).expect("binding");
        let store =
            Arc::new(EdgeStore::open_in_memory(binding.clone(), test_policy()).expect("store"));
        let (url, server) = test_server(binding.clone(), store, Keys::generate()).await;
        let (mut socket, _) = connect_async(&url).await.expect("connect");
        assert_eq!(text(&mut socket).await[0], "AUTH");
        socket
            .send(TungsteniteMessage::Text(
                json!([
                    "BUZZ-EDGE",
                    "BIND",
                    {
                        "canonical_origin": binding.canonical_origin(),
                        "community_id": Uuid::new_v4(),
                    }
                ])
                .to_string()
                .into(),
            ))
            .await
            .expect("binding");
        let rejected = text(&mut socket).await;
        assert_eq!(rejected[0], "BUZZ-EDGE");
        assert_eq!(rejected[2], false);
        server.abort();
    }

    #[tokio::test]
    async fn nip42_auth_for_another_challenge_is_rejected() {
        let binding =
            CommunityBinding::new("wss://relay.example.com", Uuid::new_v4()).expect("binding");
        let store =
            Arc::new(EdgeStore::open_in_memory(binding.clone(), test_policy()).expect("store"));
        let (url, server) = test_server(binding.clone(), store, Keys::generate()).await;
        let (mut socket, _) = connect_async(&url).await.expect("connect");
        assert_eq!(text(&mut socket).await[0], "AUTH");
        socket
            .send(TungsteniteMessage::Text(
                json!([
                    "BUZZ-EDGE",
                    "BIND",
                    {
                        "canonical_origin": binding.canonical_origin(),
                        "community_id": binding.community_id(),
                    }
                ])
                .to_string()
                .into(),
            ))
            .await
            .expect("binding");
        assert_eq!(text(&mut socket).await[2], true);
        let keys = Keys::generate();
        let auth = buzz_ws_client::build_auth_event("wrong challenge", &url, &keys, None)
            .expect("auth event");
        socket
            .send(TungsteniteMessage::Text(
                json!(["AUTH", auth]).to_string().into(),
            ))
            .await
            .expect("auth");
        let rejected = text(&mut socket).await;
        assert_eq!(rejected[0], "OK");
        assert_eq!(rejected[2], false);
        server.abort();
    }

    #[tokio::test]
    async fn http_binding_precedes_nip98_consumption_and_replay_is_rejected() {
        let binding =
            CommunityBinding::new("wss://relay.example.com", Uuid::new_v4()).expect("binding");
        let store =
            Arc::new(EdgeStore::open_in_memory(binding.clone(), test_policy()).expect("store"));
        let edge_keys = Keys::generate();
        let author = Keys::generate();
        let channel_id = Uuid::new_v4();
        store
            .set_channel_selected(channel_id, true)
            .expect("select");
        let verified_at = test_now();
        let membership = EventBuilder::new(Kind::Custom(39_002), "")
            .tags([
                Tag::parse(["d", channel_id.to_string().as_str()]).expect("d tag"),
                Tag::parse(["p", edge_keys.public_key().to_hex().as_str()]).expect("edge member"),
                Tag::parse(["p", author.public_key().to_hex().as_str()]).expect("author member"),
            ])
            .custom_created_at(Timestamp::from(verified_at as u64))
            .sign_with_keys(&Keys::generate())
            .expect("membership");
        store
            .persist_verified_authorization_snapshot(
                &[storage::VerifiedChannelAuthorization {
                    channel_id,
                    membership_event_id: membership.id,
                    membership_event_created_at: verified_at,
                    membership_event_bytes: membership.as_json().into_bytes(),
                    active_authors: vec![edge_keys.public_key(), author.public_key()],
                }],
                verified_at,
                &edge_keys,
            )
            .expect("eligibility");
        let (ws_url, server) = test_server(binding.clone(), store, edge_keys).await;
        let event = EventBuilder::new(Kind::Custom(9), "HTTP local message")
            .tags([Tag::parse(["h", channel_id.to_string().as_str()]).expect("h tag")])
            .sign_with_keys(&author)
            .expect("message");
        let body = event.as_json();
        let http_url = format!("{}/events", ws_url.replacen("ws://", "http://", 1));
        let payload_hash = hex::encode(Sha256::digest(body.as_bytes()));
        let auth = EventBuilder::new(Kind::HttpAuth, "")
            .tags([
                Tag::parse(["u", http_url.as_str()]).expect("u tag"),
                Tag::parse(["method", "POST"]).expect("method tag"),
                Tag::parse(["payload", payload_hash.as_str()]).expect("payload tag"),
            ])
            .sign_with_keys(&author)
            .expect("NIP-98");
        let authorization = format!("Nostr {}", BASE64.encode(auth.as_json()));
        let client = reqwest::Client::new();

        let mismatch = client
            .post(&http_url)
            .header("authorization", &authorization)
            .header("x-buzz-canonical-origin", binding.canonical_origin())
            .header("x-buzz-community-id", Uuid::new_v4().to_string())
            .body(body.clone())
            .send()
            .await
            .expect("mismatch response");
        assert_eq!(mismatch.status(), StatusCode::MISDIRECTED_REQUEST);

        let accepted = client
            .post(&http_url)
            .header("authorization", &authorization)
            .header("x-buzz-canonical-origin", binding.canonical_origin())
            .header("x-buzz-community-id", binding.community_id().to_string())
            .body(body.clone())
            .send()
            .await
            .expect("accepted response");
        assert_eq!(accepted.status(), StatusCode::OK);

        let replay = client
            .post(&http_url)
            .header("authorization", &authorization)
            .header("x-buzz-canonical-origin", binding.canonical_origin())
            .header("x-buzz-community-id", binding.community_id().to_string())
            .body(body)
            .send()
            .await
            .expect("replay response");
        assert_eq!(replay.status(), StatusCode::UNAUTHORIZED);

        let filter_body = serde_json::to_vec(&[json!({
            "kinds": [9],
            "#h": [channel_id],
        })])
        .expect("filter body");
        let query_url = format!("{}/query", ws_url.replacen("ws://", "http://", 1));
        let query = client
            .post(&query_url)
            .header(
                "authorization",
                nip98_authorization(&author, &query_url, &filter_body),
            )
            .header("x-buzz-canonical-origin", binding.canonical_origin())
            .header("x-buzz-community-id", binding.community_id().to_string())
            .body(filter_body.clone())
            .send()
            .await
            .expect("query response");
        assert_eq!(query.status(), StatusCode::OK);
        let events: Vec<Event> = query.json().await.expect("query events");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, event.id);

        let count_url = format!("{}/count", ws_url.replacen("ws://", "http://", 1));
        let count_response = client
            .post(&count_url)
            .header(
                "authorization",
                nip98_authorization(&author, &count_url, &filter_body),
            )
            .header("x-buzz-canonical-origin", binding.canonical_origin())
            .header("x-buzz-community-id", binding.community_id().to_string())
            .body(filter_body)
            .send()
            .await
            .expect("count response");
        assert_eq!(count_response.status(), StatusCode::OK);
        let count: Value = count_response.json().await.expect("count JSON");
        assert_eq!(count["count"], 1);
        server.abort();
    }

    #[test]
    fn config_rejects_non_loopback_and_store_binding_mismatch() {
        let first =
            CommunityBinding::new("wss://relay.example.com", Uuid::new_v4()).expect("first");
        assert!(EdgeConfig::new("ws://192.0.2.1:3031", first.clone()).is_err());
        assert!(EdgeConfig::new("wss://127.0.0.1:3031", first.clone()).is_err());
        let second =
            CommunityBinding::new("wss://relay.example.com", Uuid::new_v4()).expect("second");
        let store = Arc::new(EdgeStore::open_in_memory(second, test_policy()).expect("store"));
        let config = EdgeConfig::new("ws://127.0.0.1:3031", first).expect("config");
        assert!(EdgeRelay::new(config, store, Keys::generate()).is_err());
    }
}
