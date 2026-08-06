//! Canonical-relay mirror coordinator for selected kind-9 channels.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use buzz_ws_client::{NostrWsConnection, RelayMessage};
use nostr::{
    Alphabet, Event, EventBuilder, Filter, JsonUtil, Keys, Kind, SingleLetterTag, Tag, Timestamp,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::time::Instant;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::eligibility::{verify_upstream_membership, AuthorizationStartup};
use crate::protocol::event_channel;
use crate::storage::{AuthorizationSignalOutcome, EdgeStore, UpstreamCursor};
use crate::EdgeRelay;

const ONLINE_SESSION_MAX_AGE: Duration = Duration::from_secs(6 * 60 * 60);
const RECONNECT_DELAY: Duration = Duration::from_secs(5);
const BACKFILL_TIMEOUT: Duration = Duration::from_secs(30);
const BACKFILL_PAGE_SIZE: usize = 1_000;

#[derive(Debug, Clone)]
struct MirrorChannel {
    channel_id: Uuid,
    cursor: Option<UpstreamCursor>,
    signal_cursor: Option<UpstreamCursor>,
    edge_notification_cursor: Option<UpstreamCursor>,
}

/// Maintain one authenticated upstream mirror session, recycling it every six hours.
///
/// The WebSocket subscription is registered before the HTTP bridge backlog is
/// paged. The bridge uses the relay's composite `(until, before_id)` cursor,
/// and a channel's durable high-water mark advances only after every page
/// succeeds. A disconnect or crash therefore causes harmless duplicate reads,
/// never permanent history omission.
pub async fn run_upstream_mirror(relay: EdgeRelay, edge_keys: Keys, startup_was_fresh: bool) {
    let mut may_reuse_startup_verification = startup_was_fresh;
    loop {
        if !may_reuse_startup_verification {
            let verification =
                verify_upstream_membership(Arc::clone(relay.store()), &edge_keys).await;
            let now = match unix_seconds() {
                Ok(now) => now,
                Err(reason) => {
                    error!(%reason, "upstream mirror could not read system time");
                    tokio::time::sleep(RECONNECT_DELAY).await;
                    continue;
                }
            };
            match relay.apply_authorization_refresh(now, verification).await {
                Ok(AuthorizationStartup::Fresh { eligible_channels }) => {
                    info!(
                        eligible_channels = eligible_channels.len(),
                        "upstream authorization snapshot refreshed"
                    );
                }
                Ok(AuthorizationStartup::OfflineLease {
                    eligible_channels,
                    expires_at,
                }) => {
                    warn!(
                        eligible_channels = eligible_channels.len(),
                        expires_at,
                        "upstream unavailable; mirror retry is using the signed local lease"
                    );
                }
                Ok(AuthorizationStartup::Disabled { reason }) => {
                    warn!(%reason, "upstream mirror is fail-closed");
                    tokio::time::sleep(RECONNECT_DELAY).await;
                    continue;
                }
                Err(reason) => {
                    error!(%reason, "upstream authorization refresh failed");
                    tokio::time::sleep(RECONNECT_DELAY).await;
                    continue;
                }
            }
        }
        may_reuse_startup_verification = false;

        let channels =
            match mirror_channels(Arc::clone(relay.store()), edge_keys.public_key()).await {
                Ok(channels) if !channels.is_empty() => channels,
                Ok(_) => {
                    tokio::time::sleep(RECONNECT_DELAY).await;
                    continue;
                }
                Err(reason) => {
                    error!(%reason, "upstream mirror could not load selected channels");
                    tokio::time::sleep(RECONNECT_DELAY).await;
                    continue;
                }
            };
        let relay_identity = match relay.store().relay_identity(&edge_keys.public_key()) {
            Ok(identity) => identity,
            Err(reason) => {
                error!(%reason, "upstream mirror could not load the pinned relay identity");
                tokio::time::sleep(RECONNECT_DELAY).await;
                continue;
            }
        };
        let filters = mirror_filters(&channels, edge_keys.public_key(), relay_identity);
        let canonical_origin = relay.store().binding().canonical_origin().to_string();
        let mut connection =
            match NostrWsConnection::connect_authenticated(&canonical_origin, &edge_keys, None)
                .await
            {
                Ok(connection) => connection,
                Err(reason) => {
                    warn!(%reason, "upstream mirror connection failed");
                    tokio::time::sleep(RECONNECT_DELAY).await;
                    continue;
                }
            };

        let subscription_id = format!("buzz-edge-mirror-{}", Uuid::new_v4());
        let request = match subscription_request(&subscription_id, &filters) {
            Ok(request) => request,
            Err(reason) => {
                error!(%reason, "upstream mirror could not encode its subscription");
                let _ = connection.disconnect().await;
                tokio::time::sleep(RECONNECT_DELAY).await;
                continue;
            }
        };
        if let Err(reason) = connection.send_raw(&request).await {
            warn!(%reason, "upstream mirror subscription failed");
            tokio::time::sleep(RECONNECT_DELAY).await;
            continue;
        }

        // The relay registers a REQ before sending its historical response.
        // Starting backfill only after the send therefore closes the query/live
        // race: bridge history covers the past and this socket covers the tail.
        if let Err(reason) = backfill_channels(&relay, &edge_keys, &channels).await {
            warn!(%reason, "upstream mirror backfill failed without advancing its cursor");
            let _ = connection.disconnect().await;
            tokio::time::sleep(RECONNECT_DELAY).await;
            continue;
        }

        info!(
            channels = channels.len(),
            "upstream kind-9 mirror session started"
        );
        let recycle_at = Instant::now() + ONLINE_SESSION_MAX_AGE;
        loop {
            let remaining = recycle_at
                .checked_duration_since(Instant::now())
                .unwrap_or(Duration::ZERO);
            if remaining.is_zero() {
                debug!("recycling upstream mirror session at authorization refresh boundary");
                break;
            }
            match connection.next_event(remaining).await {
                Ok(RelayMessage::Event {
                    subscription_id: received,
                    event,
                }) if received == subscription_id => {
                    if event.kind.as_u16() != 9 {
                        match relay.apply_authorization_signal(*event).await {
                            Ok(
                                AuthorizationSignalOutcome::EdgeRemoved { .. }
                                | AuthorizationSignalOutcome::RefreshRequired { .. },
                            ) => {
                                debug!("authorization signal requires a fresh upstream check");
                                break;
                            }
                            Ok(
                                AuthorizationSignalOutcome::Duplicate
                                | AuthorizationSignalOutcome::SignalObserved { .. }
                                | AuthorizationSignalOutcome::RosterReplaced { .. }
                                | AuthorizationSignalOutcome::AuthorRemoved { .. },
                            ) => continue,
                            Err(reason) => {
                                warn!(%reason, "rejected upstream authorization signal");
                                continue;
                            }
                        }
                    }
                    let cursor = UpstreamCursor {
                        created_at: match i64::try_from(event.created_at.as_secs()) {
                            Ok(value) => value,
                            Err(reason) => {
                                warn!(%reason, "upstream mirror event timestamp overflow");
                                continue;
                            }
                        },
                        event_id: event.id,
                    };
                    let channel_id = match event_channel(&event) {
                        Ok(channel_id) => channel_id,
                        Err(reason) => {
                            warn!(%reason, "rejected upstream mirror event");
                            continue;
                        }
                    };
                    let exact_event_bytes = event.as_json().into_bytes();
                    match relay.accept_upstream_event(*event, exact_event_bytes).await {
                        Ok(_) => {
                            if let Err(reason) =
                                relay.store().advance_upstream_cursor(channel_id, &cursor)
                            {
                                warn!(%reason, "could not advance upstream live cursor");
                                break;
                            }
                        }
                        Err(reason) => warn!(%reason, "rejected upstream mirror event"),
                    }
                }
                Ok(RelayMessage::Eose {
                    subscription_id: received,
                }) if received == subscription_id => {
                    debug!("upstream mirror caught up to canonical history");
                }
                Ok(RelayMessage::Closed {
                    subscription_id: received,
                    message,
                }) if received == subscription_id => {
                    warn!(%message, "upstream mirror subscription was closed");
                    break;
                }
                Ok(RelayMessage::Notice { message }) => {
                    warn!(%message, "upstream mirror notice");
                }
                Ok(_) => {}
                Err(reason) => {
                    warn!(%reason, "upstream mirror session ended");
                    break;
                }
            }
        }
        let _ = connection.disconnect().await;
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

async fn backfill_channels(
    relay: &EdgeRelay,
    edge_keys: &Keys,
    channels: &[MirrorChannel],
) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .timeout(BACKFILL_TIMEOUT)
        .build()
        .map_err(|error| error.to_string())?;
    let query_url = canonical_query_url(relay.store().binding().canonical_origin())?;
    for channel in channels {
        backfill_channel(relay, edge_keys, &client, &query_url, channel).await?;
    }
    Ok(())
}

async fn backfill_channel(
    relay: &EdgeRelay,
    edge_keys: &Keys,
    client: &reqwest::Client,
    query_url: &str,
    channel: &MirrorChannel,
) -> Result<(), String> {
    let mut filter = json!({
        "kinds": [9],
        "#h": [channel.channel_id],
        "limit": BACKFILL_PAGE_SIZE,
    });
    if let Some(cursor) = &channel.cursor {
        filter["since"] = json!(cursor.created_at);
    }
    let mut newest = channel.cursor;
    loop {
        let body = serde_json::to_vec(&[filter.clone()]).map_err(|error| error.to_string())?;
        let auth = sign_nip98(edge_keys, "POST", query_url, &body)?;
        let response = client
            .post(query_url)
            .header("authorization", auth)
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .map_err(|error| error.to_string())?;
        let status = response.status();
        let bytes = response.bytes().await.map_err(|error| error.to_string())?;
        if !status.is_success() {
            return Err(format!(
                "canonical /query returned {status}: {}",
                String::from_utf8_lossy(&bytes)
            ));
        }
        let page: Vec<Event> = serde_json::from_slice(&bytes)
            .map_err(|error| format!("canonical /query response is invalid: {error}"))?;
        for event in &page {
            let event_channel_id = event_channel(event)?;
            if event_channel_id != channel.channel_id {
                return Err("canonical /query returned an event from another channel".to_string());
            }
            let created_at = i64::try_from(event.created_at.as_secs())
                .map_err(|error| format!("upstream event timestamp overflow: {error}"))?;
            let cursor = UpstreamCursor {
                created_at,
                event_id: event.id,
            };
            if newest
                .as_ref()
                .is_none_or(|current| cursor_is_newer(&cursor, current))
            {
                newest = Some(cursor);
            }
            relay
                .accept_upstream_event(event.clone(), event.as_json().into_bytes())
                .await
                .map_err(|error| error.to_string())?;
        }
        if page.len() < BACKFILL_PAGE_SIZE {
            break;
        }
        advance_backfill_filter(&mut filter, &page)?;
    }

    if let Some(cursor) = newest {
        relay
            .store()
            .advance_upstream_cursor(channel.channel_id, &cursor)
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn advance_backfill_filter(filter: &mut Value, page: &[Event]) -> Result<(), String> {
    let last = page
        .last()
        .ok_or_else(|| "cannot advance an empty backfill page".to_string())?;
    filter["until"] = json!(last.created_at.as_secs());
    filter["before_id"] = json!(last.id.to_hex());
    Ok(())
}

fn cursor_is_newer(candidate: &UpstreamCursor, current: &UpstreamCursor) -> bool {
    candidate.created_at > current.created_at
        || (candidate.created_at == current.created_at && candidate.event_id > current.event_id)
}

fn sign_nip98(keys: &Keys, method: &str, url: &str, body: &[u8]) -> Result<String, String> {
    let payload = hex::encode(Sha256::digest(body));
    let nonce = Uuid::new_v4().to_string();
    let tags = [
        Tag::parse(["u", url]).map_err(|error| error.to_string())?,
        Tag::parse(["method", method]).map_err(|error| error.to_string())?,
        Tag::parse(["nonce", nonce.as_str()]).map_err(|error| error.to_string())?,
        Tag::parse(["payload", payload.as_str()]).map_err(|error| error.to_string())?,
    ];
    let event = EventBuilder::new(Kind::Custom(27_235), "")
        .tags(tags)
        .sign_with_keys(keys)
        .map_err(|error| error.to_string())?;
    Ok(format!(
        "Nostr {}",
        BASE64.encode(event.as_json().as_bytes())
    ))
}

async fn mirror_channels(
    store: Arc<EdgeStore>,
    edge_pubkey: nostr::PublicKey,
) -> Result<Vec<MirrorChannel>, String> {
    tokio::task::spawn_blocking(move || {
        let channel_ids = store
            .eligible_channels()
            .map_err(|error| error.to_string())?;
        channel_ids
            .into_iter()
            .map(|channel_id| {
                let cursor = store
                    .upstream_cursor(channel_id)
                    .map_err(|error| error.to_string())?;
                let signal_cursor = store
                    .authorization_signal_cursor(&edge_pubkey, channel_id)
                    .map_err(|error| error.to_string())?;
                let edge_notification_cursor = store
                    .edge_notification_cursor(&edge_pubkey, channel_id)
                    .map_err(|error| error.to_string())?;
                Ok(MirrorChannel {
                    channel_id,
                    cursor,
                    signal_cursor,
                    edge_notification_cursor,
                })
            })
            .collect()
    })
    .await
    .map_err(|error| error.to_string())?
}

fn mirror_filter(channels: &[MirrorChannel]) -> Filter {
    let values = channels
        .iter()
        .map(|channel| channel.channel_id.to_string())
        .collect::<Vec<_>>();
    let mut filter = Filter::new()
        .kind(Kind::Custom(9))
        .custom_tags(SingleLetterTag::lowercase(Alphabet::H), values)
        .limit(BACKFILL_PAGE_SIZE);
    let earliest_cursor = channels
        .iter()
        .map(|channel| channel.cursor.as_ref().map(|cursor| cursor.created_at))
        .collect::<Option<Vec<_>>>()
        .and_then(|cursors| cursors.into_iter().min());
    if let Some(created_at) = earliest_cursor.and_then(|value| u64::try_from(value).ok()) {
        filter = filter.since(Timestamp::from(created_at));
    }
    filter
}

fn mirror_filters(
    channels: &[MirrorChannel],
    edge_pubkey: nostr::PublicKey,
    relay_identity: nostr::PublicKey,
) -> Vec<Filter> {
    let channel_values = channels
        .iter()
        .map(|channel| channel.channel_id.to_string())
        .collect::<Vec<_>>();
    let mut system_messages = Filter::new()
        .kind(Kind::Custom(40_099))
        .author(relay_identity)
        .custom_tags(
            SingleLetterTag::lowercase(Alphabet::H),
            channel_values.clone(),
        )
        .limit(BACKFILL_PAGE_SIZE);
    let earliest_signal_cursor = channels
        .iter()
        .map(|channel| {
            channel
                .signal_cursor
                .as_ref()
                .map(|cursor| cursor.created_at)
        })
        .collect::<Option<Vec<_>>>()
        .and_then(|cursors| cursors.into_iter().min());
    if let Some(created_at) = earliest_signal_cursor.and_then(|value| u64::try_from(value).ok()) {
        system_messages = system_messages.since(Timestamp::from(created_at));
    }
    let edge_hex = edge_pubkey.to_hex();
    let roster = Filter::new()
        .kind(Kind::Custom(39_002))
        .author(relay_identity)
        .custom_tags(SingleLetterTag::lowercase(Alphabet::P), [edge_hex.as_str()]);
    let mut edge_notifications = Filter::new()
        .kinds([Kind::Custom(44_100), Kind::Custom(44_101)])
        .author(relay_identity)
        .custom_tags(SingleLetterTag::lowercase(Alphabet::P), [edge_hex.as_str()])
        .limit(BACKFILL_PAGE_SIZE);
    let earliest_edge_notification_cursor = channels
        .iter()
        .map(|channel| {
            channel
                .edge_notification_cursor
                .as_ref()
                .map(|cursor| cursor.created_at)
        })
        .collect::<Option<Vec<_>>>()
        .and_then(|cursors| cursors.into_iter().min());
    if let Some(created_at) =
        earliest_edge_notification_cursor.and_then(|value| u64::try_from(value).ok())
    {
        edge_notifications = edge_notifications.since(Timestamp::from(created_at));
    }
    vec![
        mirror_filter(channels),
        system_messages,
        roster,
        edge_notifications,
    ]
}

fn subscription_request(subscription_id: &str, filters: &[Filter]) -> Result<Value, String> {
    let mut request = vec![json!("REQ"), json!(subscription_id)];
    for filter in filters {
        request.push(serde_json::to_value(filter).map_err(|error| error.to_string())?);
    }
    Ok(Value::Array(request))
}

fn canonical_query_url(canonical_origin: &str) -> Result<String, String> {
    let mut url = url::Url::parse(canonical_origin).map_err(|error| error.to_string())?;
    let scheme = match url.scheme() {
        "wss" => "https",
        "ws" => "http",
        scheme => return Err(format!("unsupported canonical relay scheme: {scheme}")),
    };
    url.set_scheme(scheme)
        .map_err(|_| "could not derive canonical HTTP scheme".to_string())?;
    url.set_path("/query");
    url.set_query(None);
    url.set_fragment(None);
    Ok(url.to_string())
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
    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::routing::post;
    use axum::{Json, Router};
    use nostr::{EventId, Tag};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use crate::storage::{AuthorizationPolicy, CommunityBinding, VerifiedChannelAuthorization};

    fn event_at(keys: &Keys, channel: Uuid, created_at: u64, content: &str) -> Event {
        EventBuilder::new(Kind::Custom(9), content)
            .tags([Tag::parse(["h", channel.to_string().as_str()]).expect("tag")])
            .custom_created_at(Timestamp::from(created_at))
            .sign_with_keys(keys)
            .expect("event")
    }

    #[test]
    fn mirror_filter_uses_all_channels_and_the_earliest_complete_cursor() {
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let filter = mirror_filter(&[
            MirrorChannel {
                channel_id: first,
                cursor: Some(UpstreamCursor {
                    created_at: 200,
                    event_id: EventId::from_byte_array([1; 32]),
                }),
                signal_cursor: None,
                edge_notification_cursor: None,
            },
            MirrorChannel {
                channel_id: second,
                cursor: Some(UpstreamCursor {
                    created_at: 100,
                    event_id: EventId::from_byte_array([2; 32]),
                }),
                signal_cursor: None,
                edge_notification_cursor: None,
            },
        ]);
        assert_eq!(filter.limit, Some(BACKFILL_PAGE_SIZE));
        assert_eq!(filter.since, Some(Timestamp::from(100)));
        let h = SingleLetterTag::lowercase(Alphabet::H);
        let values = filter.generic_tags.get(&h).expect("h tags");
        assert!(values
            .iter()
            .any(|value| value.as_str() == first.to_string()));
        assert!(values
            .iter()
            .any(|value| value.as_str() == second.to_string()));
    }

    #[test]
    fn missing_cursor_requests_complete_history() {
        let filter = mirror_filter(&[
            MirrorChannel {
                channel_id: Uuid::new_v4(),
                cursor: Some(UpstreamCursor {
                    created_at: 100,
                    event_id: EventId::from_byte_array([1; 32]),
                }),
                signal_cursor: None,
                edge_notification_cursor: None,
            },
            MirrorChannel {
                channel_id: Uuid::new_v4(),
                cursor: None,
                signal_cursor: None,
                edge_notification_cursor: None,
            },
        ]);
        assert_eq!(filter.since, None);
    }

    #[test]
    fn authorization_filters_keep_global_notifications_edge_self_scoped() {
        let edge = Keys::generate();
        let relay = Keys::generate();
        let channel = Uuid::new_v4();
        let filters = mirror_filters(
            &[MirrorChannel {
                channel_id: channel,
                cursor: None,
                signal_cursor: Some(UpstreamCursor {
                    created_at: 123,
                    event_id: EventId::from_byte_array([3; 32]),
                }),
                edge_notification_cursor: Some(UpstreamCursor {
                    created_at: 456,
                    event_id: EventId::from_byte_array([4; 32]),
                }),
            }],
            edge.public_key(),
            relay.public_key(),
        );
        assert_eq!(filters.len(), 4);
        let request = subscription_request("signals", &filters).expect("request");
        let values = request.as_array().expect("array");
        assert_eq!(values.len(), 6);
        let system = &values[3];
        assert_eq!(system["kinds"], json!([40_099]));
        assert_eq!(system["#h"], json!([channel.to_string()]));
        assert_eq!(system["since"], json!(123));
        let roster = &values[4];
        assert_eq!(roster["kinds"], json!([39_002]));
        assert_eq!(roster["#p"], json!([edge.public_key().to_hex()]));
        let notifications = &values[5];
        assert_eq!(notifications["kinds"], json!([44_100, 44_101]));
        assert_eq!(notifications["#p"], json!([edge.public_key().to_hex()]));
        assert_eq!(
            notifications["authors"],
            json!([relay.public_key().to_hex()])
        );
        assert_eq!(notifications["since"], json!(456));
    }

    #[test]
    fn composite_backfill_cursor_preserves_more_than_one_page_and_tied_seconds() {
        let keys = Keys::generate();
        let channel = Uuid::new_v4();
        let page = (0..BACKFILL_PAGE_SIZE)
            .map(|index| event_at(&keys, channel, 1_800_000_000, &index.to_string()))
            .collect::<Vec<_>>();
        let mut filter = json!({"kinds":[9], "#h":[channel], "limit":BACKFILL_PAGE_SIZE});
        advance_backfill_filter(&mut filter, &page).expect("cursor");
        assert_eq!(filter["until"], json!(1_800_000_000_u64));
        assert_eq!(
            filter["before_id"],
            json!(page.last().expect("last").id.to_hex())
        );
    }

    #[test]
    fn canonical_origin_maps_to_query_bridge() {
        assert_eq!(
            canonical_query_url("wss://relay.example.com").expect("url"),
            "https://relay.example.com/query"
        );
    }

    #[derive(Clone)]
    struct MockQueryState {
        events: Arc<Vec<Event>>,
        requests: Arc<AtomicUsize>,
        fail_second_request: Arc<AtomicBool>,
    }

    async fn mock_query(
        State(state): State<MockQueryState>,
        Json(filters): Json<Vec<Value>>,
    ) -> Result<Json<Vec<Event>>, StatusCode> {
        let request = state.requests.fetch_add(1, Ordering::SeqCst) + 1;
        if request == 2 && state.fail_second_request.load(Ordering::SeqCst) {
            return Err(StatusCode::SERVICE_UNAVAILABLE);
        }
        let filter = filters.first().ok_or(StatusCode::BAD_REQUEST)?;
        let since = filter.get("since").and_then(Value::as_u64);
        let until = filter.get("until").and_then(Value::as_u64);
        let before_id = filter.get("before_id").and_then(Value::as_str);
        let limit = filter
            .get("limit")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(BACKFILL_PAGE_SIZE);
        let page = state
            .events
            .iter()
            .filter(|event| since.is_none_or(|value| event.created_at.as_secs() >= value))
            .filter(|event| {
                until.is_none_or(|until| {
                    event.created_at.as_secs() < until
                        || (event.created_at.as_secs() == until
                            && before_id.is_some_and(|id| event.id.to_hex().as_str() > id))
                })
            })
            .take(limit)
            .cloned()
            .collect();
        Ok(Json(page))
    }

    #[tokio::test]
    async fn interrupted_multi_page_backfill_retries_without_history_omission() {
        let binding =
            CommunityBinding::new("wss://relay.example.com", Uuid::new_v4()).expect("binding");
        let policy = AuthorizationPolicy::new(Duration::from_secs(60 * 60)).expect("policy");
        let store = Arc::new(EdgeStore::open_in_memory(binding.clone(), policy).expect("store"));
        let edge_keys = Keys::generate();
        let author = Keys::generate();
        let channel = Uuid::new_v4();
        store
            .set_channel_selected(channel, true)
            .expect("selection");
        let verified_at = unix_seconds().expect("time");
        let membership = EventBuilder::new(Kind::Custom(39_002), "")
            .tags([
                Tag::parse(["d", channel.to_string().as_str()]).expect("d"),
                Tag::parse(["p", edge_keys.public_key().to_hex().as_str()]).expect("p"),
            ])
            .custom_created_at(Timestamp::from(verified_at as u64))
            .sign_with_keys(&Keys::generate())
            .expect("membership");
        store
            .persist_verified_authorization_snapshot(
                &[VerifiedChannelAuthorization {
                    channel_id: channel,
                    membership_event_id: membership.id,
                    membership_event_created_at: verified_at,
                    membership_event_bytes: membership.as_json().into_bytes(),
                    membership_fetch_cursor: None,
                    signal_cursor: None,
                    edge_notification_cursor: None,
                    active_authors: vec![edge_keys.public_key()],
                    removed_authors: Vec::new(),
                }],
                verified_at,
                &edge_keys,
            )
            .expect("authorization");
        let relay = EdgeRelay::new(
            crate::EdgeConfig::new("ws://127.0.0.1:3031", binding).expect("config"),
            Arc::clone(&store),
            edge_keys.clone(),
        )
        .expect("relay");

        let mut events = (0..(BACKFILL_PAGE_SIZE + 5))
            .map(|index| {
                event_at(
                    &author,
                    channel,
                    1_800_000_000 - u64::try_from(index).expect("index"),
                    &index.to_string(),
                )
            })
            .collect::<Vec<_>>();
        events.sort_by(|left, right| {
            right
                .created_at
                .cmp(&left.created_at)
                .then_with(|| left.id.cmp(&right.id))
        });
        let state = MockQueryState {
            events: Arc::new(events),
            requests: Arc::new(AtomicUsize::new(0)),
            fail_second_request: Arc::new(AtomicBool::new(true)),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("address");
        let app = Router::new()
            .route("/query", post(mock_query))
            .with_state(state.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        let client = reqwest::Client::new();
        let channel_state = MirrorChannel {
            channel_id: channel,
            cursor: None,
            signal_cursor: None,
            edge_notification_cursor: None,
        };
        let query_url = format!("http://{address}/query");

        assert!(
            backfill_channel(&relay, &edge_keys, &client, &query_url, &channel_state,)
                .await
                .is_err()
        );
        assert_eq!(store.event_count().expect("partial count"), 1_000);
        assert_eq!(store.upstream_cursor(channel).expect("cursor"), None);

        state.requests.store(0, Ordering::SeqCst);
        state.fail_second_request.store(false, Ordering::SeqCst);
        backfill_channel(&relay, &edge_keys, &client, &query_url, &channel_state)
            .await
            .expect("retry");
        assert_eq!(store.event_count().expect("complete count"), 1_005);
        assert!(store.upstream_cursor(channel).expect("cursor").is_some());
        server.abort();
    }
}
