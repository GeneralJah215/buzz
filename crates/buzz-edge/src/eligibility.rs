//! Upstream provisioning verification and bounded offline startup policy.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use buzz_ws_client::{NostrWsConnection, RelayMessage, WsClientError};
use nostr::{Alphabet, Filter, JsonUtil, Keys, Kind, PublicKey, SingleLetterTag};
use serde_json::json;
use uuid::Uuid;

use crate::storage::{AuthorizationLease, EdgeStore, StorageError, VerifiedChannelAuthorization};

const MEMBERS_KIND: u16 = 39_002;
const MEMBERSHIP_QUERY_TIMEOUT: Duration = Duration::from_secs(20);

/// Definitive or transport-level result of checking the provisioned edge identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerificationResult {
    /// Upstream admitted the direct member and returned the selected memberships.
    Verified(Vec<VerifiedChannelAuthorization>),
    /// Upstream definitively denied the edge identity or its membership read.
    Denied(String),
    /// Upstream could not be reached or did not answer in time.
    Unavailable(String),
}

/// Authorization mode selected before the loopback relay begins serving.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthorizationStartup {
    /// Fresh upstream verification was persisted.
    Fresh {
        /// Selected channels confirmed to contain the edge identity.
        eligible_channels: Vec<Uuid>,
    },
    /// Upstream was unavailable and a signed, unexpired snapshot was restored.
    OfflineLease {
        /// Selected channels restored from the signed snapshot.
        eligible_channels: Vec<Uuid>,
        /// Unix-second lease cutoff.
        expires_at: i64,
    },
    /// Local routing is fail-closed; clients must remain canonical-only.
    Disabled {
        /// Operator-facing explanation.
        reason: String,
    },
}

/// Verify direct relay admission and explicit membership in every selected channel.
///
/// Authentication deliberately carries no NIP-OA tag. A successful NIP-42
/// admission therefore exercises the production direct-relay-member path.
pub async fn verify_upstream_membership(
    store: Arc<EdgeStore>,
    edge_keys: &Keys,
) -> VerificationResult {
    let selected = match store.selected_channels() {
        Ok(channels) => channels,
        Err(error) => {
            return VerificationResult::Denied(format!(
                "selected-channel storage is unavailable: {error}"
            ));
        }
    };
    let relay_identity = match fetch_relay_identity(store.binding().canonical_origin()).await {
        Ok(identity) => identity,
        Err(result) => return result,
    };
    let mut connection = match NostrWsConnection::connect_authenticated(
        store.binding().canonical_origin(),
        edge_keys,
        None,
    )
    .await
    {
        Ok(connection) => connection,
        Err(WsClientError::AuthFailed(message)) => {
            return VerificationResult::Denied(format!(
                "direct relay membership was refused: {message}"
            ));
        }
        Err(error) => {
            return VerificationResult::Unavailable(format!(
                "canonical relay is unavailable: {error}"
            ));
        }
    };

    let subscription_id = format!("edge-eligibility-{}", Uuid::new_v4());
    let edge_pubkey = edge_keys.public_key().to_hex();
    let filter = Filter::new()
        .kind(Kind::Custom(MEMBERS_KIND))
        .author(relay_identity)
        .custom_tags(
            SingleLetterTag::lowercase(Alphabet::P),
            [edge_pubkey.as_str()],
        );
    if let Err(error) = connection
        .send_raw(&json!(["REQ", subscription_id, filter]))
        .await
    {
        return VerificationResult::Unavailable(format!(
            "membership query could not be sent: {error}"
        ));
    }

    let selected_set: HashSet<Uuid> = selected.iter().copied().collect();
    let mut newest: HashMap<Uuid, nostr::Event> = HashMap::new();
    loop {
        match connection.next_event(MEMBERSHIP_QUERY_TIMEOUT).await {
            Ok(RelayMessage::Event {
                subscription_id: received,
                event,
            }) if received == subscription_id => {
                if !is_relay_membership_event(&event, relay_identity) {
                    continue;
                }
                let channel_id = event.tags.iter().find_map(|tag| {
                    let values = tag.as_slice();
                    (values.first().map(String::as_str) == Some("d"))
                        .then(|| values.get(1))
                        .flatten()
                        .and_then(|value| Uuid::parse_str(value).ok())
                });
                let Some(channel_id) = channel_id else {
                    continue;
                };
                if selected_set.contains(&channel_id)
                    && newest
                        .get(&channel_id)
                        .map(|current| replaceable_event_is_newer(&event, current))
                        .unwrap_or(true)
                {
                    newest.insert(channel_id, *event);
                }
            }
            Ok(RelayMessage::Eose {
                subscription_id: received,
            }) if received == subscription_id => break,
            Ok(RelayMessage::Closed {
                subscription_id: received,
                message,
            }) if received == subscription_id => {
                return classify_membership_closed(&message);
            }
            Ok(_) => {}
            Err(WsClientError::Timeout | WsClientError::ConnectionClosed) => {
                return VerificationResult::Unavailable(
                    "membership query ended before EOSE".to_string(),
                );
            }
            Err(error) => {
                return VerificationResult::Unavailable(format!(
                    "membership query failed: {error}"
                ));
            }
        }
    }
    let _ = connection
        .send_raw(&json!(["CLOSE", subscription_id]))
        .await;
    let _ = connection.disconnect().await;

    let channels = selected
        .into_iter()
        .filter_map(|channel_id| {
            let event = newest.get(&channel_id)?;
            let mut active_authors = event
                .tags
                .iter()
                .filter_map(|tag| {
                    let values = tag.as_slice();
                    (values.first().map(String::as_str) == Some("p"))
                        .then(|| values.get(1))
                        .flatten()
                        .and_then(|value| PublicKey::from_hex(value).ok())
                })
                .collect::<Vec<_>>();
            active_authors.sort_by_key(PublicKey::to_hex);
            active_authors.dedup();
            if !active_authors.contains(&edge_keys.public_key()) {
                return None;
            }
            Some(VerifiedChannelAuthorization {
                channel_id,
                membership_event_id: event.id,
                membership_event_created_at: i64::try_from(event.created_at.as_secs()).ok()?,
                membership_event_bytes: event.as_json().into_bytes(),
                active_authors,
            })
        })
        .collect();
    VerificationResult::Verified(channels)
}

fn is_relay_membership_event(event: &nostr::Event, relay_identity: PublicKey) -> bool {
    event.kind == Kind::Custom(MEMBERS_KIND)
        && event.pubkey == relay_identity
        && buzz_core::verification::verify_event(event).is_ok()
}

fn replaceable_event_is_newer(candidate: &nostr::Event, current: &nostr::Event) -> bool {
    candidate.created_at > current.created_at
        || (candidate.created_at == current.created_at && candidate.id < current.id)
}

async fn fetch_relay_identity(canonical_origin: &str) -> Result<PublicKey, VerificationResult> {
    let mut url = match url::Url::parse(canonical_origin) {
        Ok(url) => url,
        Err(error) => {
            return Err(VerificationResult::Denied(format!(
                "canonical relay origin is invalid: {error}"
            )));
        }
    };
    let scheme = if url.scheme() == "wss" {
        "https"
    } else {
        "http"
    };
    if url.set_scheme(scheme).is_err() {
        return Err(VerificationResult::Denied(
            "canonical relay information URL is invalid".to_string(),
        ));
    }
    let response = reqwest::Client::new()
        .get(url)
        .header("accept", "application/nostr+json")
        .timeout(MEMBERSHIP_QUERY_TIMEOUT)
        .send()
        .await
        .map_err(|error| {
            VerificationResult::Unavailable(format!(
                "relay identity document is unavailable: {error}"
            ))
        })?;
    if !response.status().is_success() {
        return Err(classify_identity_status(response.status()));
    }
    let document: serde_json::Value = response.json().await.map_err(|error| {
        VerificationResult::Denied(format!("relay identity document is invalid: {error}"))
    })?;
    let relay_self = document
        .get("self")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            VerificationResult::Denied(
                "relay identity document does not advertise its signing key".to_string(),
            )
        })?;
    PublicKey::from_hex(relay_self).map_err(|error| {
        VerificationResult::Denied(format!(
            "relay identity document has an invalid signing key: {error}"
        ))
    })
}

fn classify_membership_closed(message: &str) -> VerificationResult {
    let normalized = message.to_ascii_lowercase();
    let definitive = ["auth-required", "restricted", "forbidden", "blocked"]
        .iter()
        .any(|marker| normalized.contains(marker));
    if definitive {
        VerificationResult::Denied(format!("membership query was refused: {message}"))
    } else {
        VerificationResult::Unavailable(format!(
            "membership query closed without a definitive authorization denial: {message}"
        ))
    }
}

fn classify_identity_status(status: reqwest::StatusCode) -> VerificationResult {
    let message = format!("relay identity document returned {status}");
    if status == reqwest::StatusCode::REQUEST_TIMEOUT
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
    {
        VerificationResult::Unavailable(message)
    } else {
        VerificationResult::Denied(message)
    }
}

/// Apply fresh verification or the signed owner-selected offline lease before serving.
pub fn apply_startup_policy(
    store: &EdgeStore,
    edge_keys: &Keys,
    now: i64,
    verification: VerificationResult,
) -> Result<AuthorizationStartup, StorageError> {
    match verification {
        VerificationResult::Verified(channels) => {
            store.persist_verified_authorization_snapshot(&channels, now, edge_keys)?;
            let eligible_channels = channels.iter().map(|channel| channel.channel_id).collect();
            Ok(AuthorizationStartup::Fresh { eligible_channels })
        }
        VerificationResult::Denied(reason) => {
            store.revoke_authorization_snapshot()?;
            Ok(AuthorizationStartup::Disabled { reason })
        }
        VerificationResult::Unavailable(unavailable_reason) => {
            match store.load_authorization_lease(&edge_keys.public_key(), now) {
                Ok(AuthorizationLease::Valid {
                    channels,
                    expires_at,
                    ..
                }) => {
                    store.restore_edge_eligibility(&channels)?;
                    let eligible_channels =
                        channels.iter().map(|channel| channel.channel_id).collect();
                    Ok(AuthorizationStartup::OfflineLease {
                        eligible_channels,
                        expires_at,
                    })
                }
                Ok(AuthorizationLease::Expired { expires_at }) => {
                    store.clear_edge_eligibility()?;
                    Ok(AuthorizationStartup::Disabled {
                        reason: format!(
                            "{unavailable_reason}; authorization lease expired at {expires_at}"
                        ),
                    })
                }
                Ok(AuthorizationLease::Missing) => {
                    store.clear_edge_eligibility()?;
                    Ok(AuthorizationStartup::Disabled {
                        reason: format!(
                            "{unavailable_reason}; no authorization snapshot is available"
                        ),
                    })
                }
                Err(error) => {
                    store.clear_edge_eligibility()?;
                    Ok(AuthorizationStartup::Disabled {
                        reason: format!(
                            "{unavailable_reason}; authorization snapshot was rejected: {error}"
                        ),
                    })
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{AuthorizationPolicy, CommunityBinding, VerifiedChannelAuthorization};
    use nostr::{EventBuilder, JsonUtil, Tag, Timestamp};

    fn policy() -> AuthorizationPolicy {
        AuthorizationPolicy::new(Duration::from_secs(72 * 60 * 60)).expect("policy")
    }

    fn now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_secs() as i64
    }

    fn store() -> EdgeStore {
        EdgeStore::open_in_memory(
            CommunityBinding::new("wss://relay.example.com", Uuid::new_v4()).expect("binding"),
            policy(),
        )
        .expect("store")
    }

    fn authorization(
        edge: &Keys,
        channel_id: Uuid,
        additional_authors: &[PublicKey],
        created_at: i64,
    ) -> VerifiedChannelAuthorization {
        let mut active_authors = vec![edge.public_key()];
        active_authors.extend_from_slice(additional_authors);
        let event = EventBuilder::new(Kind::Custom(MEMBERS_KIND), "")
            .tags(
                std::iter::once(Tag::parse(["d", channel_id.to_string().as_str()]).expect("d tag"))
                    .chain(
                        active_authors.iter().map(|author| {
                            Tag::parse(["p", author.to_hex().as_str()]).expect("p tag")
                        }),
                    ),
            )
            .custom_created_at(Timestamp::from(created_at as u64))
            .sign_with_keys(&Keys::generate())
            .expect("membership event");
        VerifiedChannelAuthorization {
            channel_id,
            membership_event_id: event.id,
            membership_event_created_at: created_at,
            membership_event_bytes: event.as_json().into_bytes(),
            active_authors,
        }
    }

    #[test]
    fn valid_signed_lease_restores_offline_eligibility() {
        let store = store();
        let edge = Keys::generate();
        let channel = Uuid::new_v4();
        store.set_channel_selected(channel, true).expect("select");
        apply_startup_policy(
            &store,
            &edge,
            1_800_000_000,
            VerificationResult::Verified(vec![authorization(&edge, channel, &[], 1_800_000_000)]),
        )
        .expect("fresh");
        store.clear_edge_eligibility().expect("clear");

        let startup = apply_startup_policy(
            &store,
            &edge,
            1_800_000_001,
            VerificationResult::Unavailable("offline".to_string()),
        )
        .expect("lease");
        assert!(matches!(
            startup,
            AuthorizationStartup::OfflineLease {
                eligible_channels,
                ..
            } if eligible_channels == vec![channel]
        ));
    }

    #[test]
    fn expired_lease_and_definitive_denial_fail_closed() {
        let store = store();
        let edge = Keys::generate();
        let channel = Uuid::new_v4();
        store.set_channel_selected(channel, true).expect("select");
        apply_startup_policy(
            &store,
            &edge,
            1_800_000_000,
            VerificationResult::Verified(vec![authorization(&edge, channel, &[], 1_800_000_000)]),
        )
        .expect("fresh");

        let expired = apply_startup_policy(
            &store,
            &edge,
            1_800_000_000 + policy().lease_seconds() + 1,
            VerificationResult::Unavailable("offline".to_string()),
        )
        .expect("expired");
        assert!(matches!(expired, AuthorizationStartup::Disabled { .. }));

        let denied = apply_startup_policy(
            &store,
            &edge,
            1_800_000_001,
            VerificationResult::Denied("revoked".to_string()),
        )
        .expect("denied");
        assert_eq!(
            denied,
            AuthorizationStartup::Disabled {
                reason: "revoked".to_string()
            }
        );
    }

    #[test]
    fn signed_roster_replacement_bounds_removed_and_added_authors() {
        let store = store();
        let edge = Keys::generate();
        let existing = Keys::generate();
        let added_while_offline = Keys::generate();
        let channel = Uuid::new_v4();
        let now = now();
        store.set_channel_selected(channel, true).expect("select");
        apply_startup_policy(
            &store,
            &edge,
            now,
            VerificationResult::Verified(vec![authorization(
                &edge,
                channel,
                &[existing.public_key()],
                now,
            )]),
        )
        .expect("fresh roster");
        assert!(store
            .principal_can_access(channel, &existing.public_key())
            .expect("existing access"));

        store
            .set_channel_member(channel, &added_while_offline.public_key(), true)
            .expect("simulate unsigned local addition");
        apply_startup_policy(
            &store,
            &edge,
            now + 1,
            VerificationResult::Unavailable("offline".to_string()),
        )
        .expect("offline lease");
        assert!(store
            .principal_can_access(channel, &existing.public_key())
            .expect("lease keeps prior author"));
        assert!(!store
            .principal_can_access(channel, &added_while_offline.public_key())
            .expect("lease rejects unsigned addition"));

        apply_startup_policy(
            &store,
            &edge,
            now + 2,
            VerificationResult::Verified(vec![authorization(&edge, channel, &[], now + 2)]),
        )
        .expect("refreshed roster");
        assert!(!store
            .principal_can_access(channel, &existing.public_key())
            .expect("removed author blocked"));
    }

    #[test]
    fn transient_upstream_responses_preserve_lease_eligibility() {
        assert!(matches!(
            classify_membership_closed("rate-limited: retry later"),
            VerificationResult::Unavailable(_)
        ));
        assert!(matches!(
            classify_identity_status(reqwest::StatusCode::TOO_MANY_REQUESTS),
            VerificationResult::Unavailable(_)
        ));
        assert!(matches!(
            classify_identity_status(reqwest::StatusCode::SERVICE_UNAVAILABLE),
            VerificationResult::Unavailable(_)
        ));
    }

    #[test]
    fn definitive_upstream_denials_fail_closed() {
        assert!(matches!(
            classify_membership_closed("restricted: not a member"),
            VerificationResult::Denied(_)
        ));
        assert!(matches!(
            classify_identity_status(reqwest::StatusCode::FORBIDDEN),
            VerificationResult::Denied(_)
        ));
    }

    #[test]
    fn observed_revocation_cannot_restore_the_old_offline_lease() {
        let store = store();
        let edge = Keys::generate();
        let channel = Uuid::new_v4();
        let now = now();
        store.set_channel_selected(channel, true).expect("select");
        apply_startup_policy(
            &store,
            &edge,
            now,
            VerificationResult::Verified(vec![authorization(&edge, channel, &[], now)]),
        )
        .expect("fresh");
        apply_startup_policy(
            &store,
            &edge,
            now + 1,
            VerificationResult::Denied("revoked".to_string()),
        )
        .expect("denial");
        let offline = apply_startup_policy(
            &store,
            &edge,
            now + 2,
            VerificationResult::Unavailable("offline".to_string()),
        )
        .expect("offline");
        assert!(matches!(offline, AuthorizationStartup::Disabled { .. }));
    }

    #[test]
    fn membership_snapshot_must_be_signed_by_the_advertised_relay_identity() {
        let relay = Keys::generate();
        let impostor = Keys::generate();
        let event = EventBuilder::new(Kind::Custom(MEMBERS_KIND), "")
            .sign_with_keys(&impostor)
            .expect("membership event");
        assert!(!is_relay_membership_event(&event, relay.public_key()));
        let valid = EventBuilder::new(Kind::Custom(MEMBERS_KIND), "")
            .sign_with_keys(&relay)
            .expect("membership event");
        assert!(is_relay_membership_event(&valid, relay.public_key()));
    }

    #[test]
    fn same_second_membership_replacement_uses_lowest_event_id() {
        let keys = Keys::generate();
        let created_at = Timestamp::from(1_800_000_000_u64);
        let first = EventBuilder::new(Kind::Custom(MEMBERS_KIND), "first")
            .custom_created_at(created_at)
            .sign_with_keys(&keys)
            .expect("first");
        let second = EventBuilder::new(Kind::Custom(MEMBERS_KIND), "second")
            .custom_created_at(created_at)
            .sign_with_keys(&keys)
            .expect("second");
        let (lower, higher) = if first.id < second.id {
            (&first, &second)
        } else {
            (&second, &first)
        };
        assert!(replaceable_event_is_newer(lower, higher));
        assert!(!replaceable_event_is_newer(higher, lower));
    }
}
