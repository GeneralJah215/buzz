//! Narrow Nostr protocol surface accepted by the phase-1 edge relay.

use nostr::{Alphabet, Event, EventId, Filter, Kind, SingleLetterTag};
use serde_json::Value;
use uuid::Uuid;

const MAX_SUB_ID_LENGTH: usize = 256;
const MAX_FILTERS: usize = 10;
const MESSAGE_KIND: u16 = 9;
pub(crate) const MAX_FILTER_LIMIT: usize = 1_000;

/// A supported client-to-edge message.
#[derive(Debug, Clone)]
pub enum ClientMessage {
    /// Bind this connection to the sidecar's canonical relay/community pair.
    Handshake {
        /// Canonical upstream WebSocket origin expected by the client.
        canonical_origin: String,
        /// Active Buzz community expected by the client.
        community_id: Uuid,
    },
    /// Submit one signed kind-9 event.
    Event(Event),
    /// Open a historical and live subscription.
    Req {
        /// Client-chosen subscription ID.
        sub_id: String,
        /// OR-combined NIP-01 filters.
        filters: Vec<Filter>,
    },
    /// Close a live subscription.
    Close(String),
    /// Count locally stored matching events.
    Count {
        /// Client-chosen request ID.
        sub_id: String,
        /// OR-combined NIP-01 filters.
        filters: Vec<Filter>,
    },
    /// Answer the connection's NIP-42 challenge.
    Auth(Event),
    /// Claim a batch of this identity's queued events for upstream submission.
    ///
    /// The sidecar cannot submit another identity's events — canonical ingest
    /// refuses — so the author does it. The session principal decides whose
    /// rows these are; the request cannot name an author.
    Drain {
        /// Client-chosen token identifying this claim, used to acknowledge it.
        claim_token: String,
        /// Maximum rows to lease.
        limit: usize,
    },
    /// Report what upstream did with one claimed event.
    DrainAck {
        /// The token from the corresponding [`ClientMessage::Drain`].
        claim_token: String,
        /// Which claimed event this result is for.
        event_id: EventId,
        /// `delivered`, `duplicate`, `rejected`, or `transient`.
        outcome: String,
        /// Why, when the outcome is `rejected`.
        reason: Option<String>,
    },
}

/// Largest batch a single claim may lease.
///
/// A lease is a 60-second promise to drain. Claiming more than can plausibly
/// be submitted in that window just parks rows until the lease lapses.
const MAX_DRAIN_BATCH: usize = 100;

fn parse_drain(value: &Value) -> Result<ClientMessage, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "BUZZ-EDGE DRAIN requires an object".to_string())?;
    if object.len() > 2 {
        return Err("BUZZ-EDGE DRAIN accepts only claim_token and limit".to_string());
    }
    let claim_token = object
        .get("claim_token")
        .and_then(Value::as_str)
        .ok_or_else(|| "claim_token must be a string".to_string())?
        .to_string();
    if claim_token.trim().is_empty() {
        return Err("claim_token must not be empty".to_string());
    }
    let limit = object
        .get("limit")
        .and_then(Value::as_u64)
        .map(|value| value as usize)
        .unwrap_or(MAX_DRAIN_BATCH)
        .clamp(1, MAX_DRAIN_BATCH);
    Ok(ClientMessage::Drain { claim_token, limit })
}

fn parse_drain_ack(value: &Value) -> Result<ClientMessage, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "BUZZ-EDGE DRAIN-ACK requires an object".to_string())?;
    let claim_token = object
        .get("claim_token")
        .and_then(Value::as_str)
        .ok_or_else(|| "claim_token must be a string".to_string())?
        .to_string();
    let event_id = object
        .get("event_id")
        .and_then(Value::as_str)
        .ok_or_else(|| "event_id must be a string".to_string())?;
    let event_id =
        EventId::from_hex(event_id).map_err(|error| format!("invalid event_id: {error}"))?;
    let outcome = object
        .get("outcome")
        .and_then(Value::as_str)
        .ok_or_else(|| "outcome must be a string".to_string())?
        .to_string();
    if !matches!(
        outcome.as_str(),
        "delivered" | "duplicate" | "rejected" | "transient"
    ) {
        return Err(format!("unsupported drain outcome: {outcome}"));
    }
    let reason = object
        .get("reason")
        .and_then(Value::as_str)
        .map(str::to_string);
    if outcome == "rejected" && reason.is_none() {
        return Err("a rejected outcome requires a reason".to_string());
    }
    Ok(ClientMessage::DrainAck {
        claim_token,
        event_id,
        outcome,
        reason,
    })
}

/// Format a leased batch for the author to submit upstream.
pub fn drain_batch(claim_token: &str, events: &[Event], lease_expires_at: i64) -> String {
    let payload: Vec<Value> = events
        .iter()
        .map(|event| serde_json::to_value(event).unwrap_or(Value::Null))
        .collect();
    serde_json::json!([
        "BUZZ-EDGE",
        "DRAIN-BATCH",
        {
            "claim_token": claim_token,
            "lease_expires_at": lease_expires_at,
            "events": payload,
        }
    ])
    .to_string()
}

/// Parse a WebSocket frame without accepting trailing or extra message fields.
pub fn parse_client_message(raw: &str) -> Result<ClientMessage, String> {
    let value: Value =
        serde_json::from_str(raw).map_err(|error| format!("JSON parse error: {error}"))?;
    let values = value
        .as_array()
        .ok_or_else(|| "expected JSON array".to_string())?;
    let verb = values
        .first()
        .and_then(Value::as_str)
        .ok_or_else(|| "message type must be a string".to_string())?;

    match verb {
        "BUZZ-EDGE" => {
            require_len(values, 3, "BUZZ-EDGE")?;
            match values[1].as_str() {
                Some("DRAIN") => return parse_drain(&values[2]),
                Some("DRAIN-ACK") => return parse_drain_ack(&values[2]),
                _ => {}
            }
            if values[1].as_str() != Some("BIND") {
                return Err("unsupported BUZZ-EDGE operation".to_string());
            }
            let binding = values[2]
                .as_object()
                .ok_or_else(|| "BUZZ-EDGE BIND requires an object".to_string())?;
            if binding.len() != 2 {
                return Err(
                    "BUZZ-EDGE BIND accepts only canonical_origin and community_id".to_string(),
                );
            }
            let canonical_origin = binding
                .get("canonical_origin")
                .and_then(Value::as_str)
                .ok_or_else(|| "canonical_origin must be a string".to_string())?
                .to_string();
            let community_id = binding
                .get("community_id")
                .and_then(Value::as_str)
                .ok_or_else(|| "community_id must be a string".to_string())?
                .parse()
                .map_err(|_| "community_id must be a UUID".to_string())?;
            Ok(ClientMessage::Handshake {
                canonical_origin,
                community_id,
            })
        }
        "EVENT" => {
            require_len(values, 2, "EVENT")?;
            let event = serde_json::from_value(values[1].clone())
                .map_err(|error| format!("invalid event: {error}"))?;
            Ok(ClientMessage::Event(event))
        }
        "AUTH" => {
            require_len(values, 2, "AUTH")?;
            let event = serde_json::from_value(values[1].clone())
                .map_err(|error| format!("invalid auth event: {error}"))?;
            Ok(ClientMessage::Auth(event))
        }
        "REQ" => {
            let (sub_id, filters) = parse_filters(values, "REQ")?;
            Ok(ClientMessage::Req { sub_id, filters })
        }
        "COUNT" => {
            let (sub_id, filters) = parse_filters(values, "COUNT")?;
            Ok(ClientMessage::Count { sub_id, filters })
        }
        "CLOSE" => {
            require_len(values, 2, "CLOSE")?;
            Ok(ClientMessage::Close(parse_sub_id(&values[1])?))
        }
        _ => Err(format!("unsupported message type: {verb}")),
    }
}

/// Validate that every filter is kind-9-only and channel-scoped.
pub fn filter_channels(filters: &[Filter]) -> Result<Vec<Uuid>, String> {
    if filters.is_empty() {
        return Err("at least one filter is required".to_string());
    }
    let h = SingleLetterTag::lowercase(Alphabet::H);
    let mut channels = Vec::new();
    for filter in filters {
        match &filter.kinds {
            Some(kinds)
                if kinds.len() == 1 && kinds.first() == Some(&Kind::Custom(MESSAGE_KIND)) => {}
            _ => return Err("filters must specify kinds:[9] only".to_string()),
        }
        let values = filter
            .generic_tags
            .get(&h)
            .ok_or_else(|| "every filter must include a #h channel".to_string())?;
        if values.is_empty() {
            return Err("#h channel filter must not be empty".to_string());
        }
        for value in values {
            let channel = Uuid::parse_str(value.as_str())
                .map_err(|_| "#h channel must be a UUID".to_string())?;
            if !channels.contains(&channel) {
                channels.push(channel);
            }
        }
    }
    Ok(channels)
}

/// Validate phase-1 filters and apply the local relay's bounded page default.
pub(crate) fn bounded_filters(mut filters: Vec<Filter>) -> Result<Vec<Filter>, String> {
    filter_channels(&filters)?;
    for filter in &mut filters {
        match filter.limit {
            Some(limit) if limit > MAX_FILTER_LIMIT => {
                return Err(format!(
                    "filter limit exceeds the {MAX_FILTER_LIMIT}-event maximum"
                ));
            }
            None => filter.limit = Some(MAX_FILTER_LIMIT),
            Some(_) => {}
        }
    }
    Ok(filters)
}

/// Extract the one UUID channel tag required on a kind-9 message.
pub fn event_channel(event: &Event) -> Result<Uuid, String> {
    if event.kind != Kind::Custom(MESSAGE_KIND) {
        return Err("blocked: buzz-edge accepts kind 9 only".to_string());
    }
    let mut values = event
        .tags
        .iter()
        .filter(|tag| tag.kind().to_string() == "h")
        .filter_map(|tag| tag.content());
    let raw = values
        .next()
        .ok_or_else(|| "invalid: kind-9 event requires one h tag".to_string())?;
    if values.next().is_some() {
        return Err("invalid: kind-9 event must not contain multiple h tags".to_string());
    }
    Uuid::parse_str(raw).map_err(|_| "invalid: h tag must be a UUID".to_string())
}

/// Extract one integrity-protected NIP-OA tag from a verified auth event.
pub fn auth_tag_json(event: &Event) -> Result<Option<String>, String> {
    let mut tags = event
        .tags
        .iter()
        .filter(|tag| tag.as_slice().first().map(String::as_str) == Some("auth"));
    let Some(tag) = tags.next() else {
        return Ok(None);
    };
    if tags.next().is_some() {
        return Err("multiple NIP-OA auth tags are not allowed".to_string());
    }
    serde_json::to_string(tag.as_slice())
        .map(Some)
        .map_err(|error| format!("failed to encode NIP-OA auth tag: {error}"))
}

/// Format a NIP-42 challenge.
pub fn auth_challenge(challenge: &str) -> String {
    serde_json::json!(["AUTH", challenge]).to_string()
}

/// Format an EVENT delivery.
pub fn event_message(sub_id: &str, event: &Event) -> String {
    serde_json::json!(["EVENT", sub_id, event]).to_string()
}

/// Format end-of-stored-events.
pub fn eose(sub_id: &str) -> String {
    serde_json::json!(["EOSE", sub_id]).to_string()
}

/// Format a NIP-45 count response.
pub fn count(sub_id: &str, value: u64) -> String {
    serde_json::json!(["COUNT", sub_id, {"count": value}]).to_string()
}

/// Format an event/auth acknowledgment.
pub fn ok(event_id: &str, accepted: bool, message: &str) -> String {
    serde_json::json!(["OK", event_id, accepted, message]).to_string()
}

/// Format a subscription rejection.
pub fn closed(sub_id: &str, message: &str) -> String {
    serde_json::json!(["CLOSED", sub_id, message]).to_string()
}

/// Format a general protocol notice.
pub fn notice(message: &str) -> String {
    serde_json::json!(["NOTICE", message]).to_string()
}

/// Format the mandatory community-binding handshake result.
pub fn binding_result(accepted: bool, message: &str) -> String {
    serde_json::json!(["BUZZ-EDGE", "BOUND", accepted, message]).to_string()
}

fn parse_filters(values: &[Value], verb: &str) -> Result<(String, Vec<Filter>), String> {
    if values.len() < 3 {
        return Err(format!("{verb} requires a sub_id and filter"));
    }
    if values.len() - 2 > MAX_FILTERS {
        return Err(format!("{verb} accepts at most {MAX_FILTERS} filters"));
    }
    let sub_id = parse_sub_id(&values[1])?;
    let filters = values[2..]
        .iter()
        .cloned()
        .map(|value| {
            serde_json::from_value(value).map_err(|error| format!("invalid filter: {error}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let filters = bounded_filters(filters)?;
    Ok((sub_id, filters))
}

fn parse_sub_id(value: &Value) -> Result<String, String> {
    let sub_id = value
        .as_str()
        .ok_or_else(|| "sub_id must be a string".to_string())?;
    if sub_id.is_empty() {
        return Err("sub_id must not be empty".to_string());
    }
    if sub_id.len() > MAX_SUB_ID_LENGTH {
        return Err(format!("sub_id exceeds the {MAX_SUB_ID_LENGTH}-byte limit"));
    }
    Ok(sub_id.to_string())
}

fn require_len(values: &[Value], expected: usize, verb: &str) -> Result<(), String> {
    if values.len() == expected {
        Ok(())
    } else {
        Err(format!("invalid {verb} message"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::{EventBuilder, Keys, Tag};

    // ── Author-drain wire format (§11) ──────────────────────────────────────

    #[test]
    fn a_drain_request_cannot_name_an_author() {
        // Whose rows get drained is decided by the authenticated session, never
        // by the request. An author field here would be a way to ask the
        // sidecar for someone else's queued events.
        let ok =
            serde_json::json!(["BUZZ-EDGE", "DRAIN", {"claim_token":"t1","limit":10}]).to_string();
        match parse_client_message(&ok).expect("parse") {
            ClientMessage::Drain { claim_token, limit } => {
                assert_eq!(claim_token, "t1");
                assert_eq!(limit, 10);
            }
            other => panic!("expected Drain, got {other:?}"),
        }

        let with_author = serde_json::json!([
            "BUZZ-EDGE", "DRAIN",
            {"claim_token":"t1","limit":10,"author":"deadbeef"}
        ])
        .to_string();
        assert!(
            parse_client_message(&with_author).is_err(),
            "an extra field must be refused, not silently ignored"
        );
    }

    #[test]
    fn drain_limit_is_defaulted_and_clamped() {
        let no_limit = serde_json::json!(["BUZZ-EDGE", "DRAIN", {"claim_token":"t"}]).to_string();
        match parse_client_message(&no_limit).expect("parse") {
            ClientMessage::Drain { limit, .. } => assert_eq!(limit, MAX_DRAIN_BATCH),
            other => panic!("expected Drain, got {other:?}"),
        }
        // A lease is a 60-second promise to drain; an unbounded claim would
        // just park rows until it lapsed.
        let huge = serde_json::json!(["BUZZ-EDGE", "DRAIN", {"claim_token":"t","limit":100000}])
            .to_string();
        match parse_client_message(&huge).expect("parse") {
            ClientMessage::Drain { limit, .. } => assert_eq!(limit, MAX_DRAIN_BATCH),
            other => panic!("expected Drain, got {other:?}"),
        }
        let zero =
            serde_json::json!(["BUZZ-EDGE", "DRAIN", {"claim_token":"t","limit":0}]).to_string();
        match parse_client_message(&zero).expect("parse") {
            ClientMessage::Drain { limit, .. } => assert_eq!(limit, 1),
            other => panic!("expected Drain, got {other:?}"),
        }
    }

    #[test]
    fn an_empty_claim_token_is_refused() {
        // The token is what stops a lapsed author overwriting rows a newer
        // drain now owns. An empty one would collide with every other empty one.
        for bad in ["", "   "] {
            let raw = serde_json::json!(["BUZZ-EDGE", "DRAIN", {"claim_token":bad}]).to_string();
            assert!(parse_client_message(&raw).is_err(), "token {bad:?}");
        }
    }

    #[test]
    fn drain_ack_accepts_the_four_outcomes_and_nothing_else() {
        let id = EventId::all_zeros().to_hex();
        for outcome in ["delivered", "duplicate", "transient"] {
            let raw = serde_json::json!([
                "BUZZ-EDGE", "DRAIN-ACK",
                {"claim_token":"t","event_id":id,"outcome":outcome}
            ])
            .to_string();
            assert!(parse_client_message(&raw).is_ok(), "{outcome}");
        }
        let unknown = serde_json::json!([
            "BUZZ-EDGE", "DRAIN-ACK",
            {"claim_token":"t","event_id":id,"outcome":"maybe"}
        ])
        .to_string();
        assert!(parse_client_message(&unknown).is_err());
    }

    #[test]
    fn a_rejection_must_carry_its_reason() {
        // Quarantine is permanent and surfaced to a human. "Rejected" with no
        // explanation is not actionable.
        let id = EventId::all_zeros().to_hex();
        let no_reason = serde_json::json!([
            "BUZZ-EDGE", "DRAIN-ACK",
            {"claim_token":"t","event_id":id,"outcome":"rejected"}
        ])
        .to_string();
        assert!(parse_client_message(&no_reason).is_err());

        let with_reason = serde_json::json!([
            "BUZZ-EDGE", "DRAIN-ACK",
            {"claim_token":"t","event_id":id,"outcome":"rejected","reason":"membership revoked"}
        ])
        .to_string();
        match parse_client_message(&with_reason).expect("parse") {
            ClientMessage::DrainAck { reason, .. } => {
                assert_eq!(reason.as_deref(), Some("membership revoked"))
            }
            other => panic!("expected DrainAck, got {other:?}"),
        }
    }

    #[test]
    fn a_malformed_event_id_is_refused() {
        let raw = serde_json::json!([
            "BUZZ-EDGE", "DRAIN-ACK",
            {"claim_token":"t","event_id":"not-hex","outcome":"delivered"}
        ])
        .to_string();
        assert!(parse_client_message(&raw).is_err());
    }

    #[test]
    fn a_drain_batch_frame_round_trips_the_exact_event_bytes() {
        // The author must re-submit byte-identical events, so the frame cannot
        // reserialize them into a different shape.
        let keys = Keys::generate();
        let channel = Uuid::new_v4();
        let event = EventBuilder::new(Kind::Custom(9), "hello")
            .tags([Tag::parse(["h", channel.to_string().as_str()]).expect("h")])
            .sign_with_keys(&keys)
            .expect("sign");

        let frame = drain_batch("token", std::slice::from_ref(&event), 1_234);
        let parsed: Value = serde_json::from_str(&frame).expect("json");
        assert_eq!(parsed[0], "BUZZ-EDGE");
        assert_eq!(parsed[1], "DRAIN-BATCH");
        assert_eq!(parsed[2]["claim_token"], "token");
        assert_eq!(parsed[2]["lease_expires_at"], 1_234);

        let returned: Event =
            serde_json::from_value(parsed[2]["events"][0].clone()).expect("event");
        assert_eq!(
            returned.id, event.id,
            "event id must survive the round trip"
        );
        assert!(returned.verify().is_ok(), "signature must still verify");
    }

    #[test]
    fn rejects_unscoped_or_non_message_filters() {
        let unscoped = serde_json::json!(["REQ", "sub", {"kinds":[9]}]).to_string();
        assert!(parse_client_message(&unscoped).is_err());
        let channel = Uuid::new_v4();
        let wrong_kind = serde_json::json!([
            "REQ", "sub", {"kinds":[7], "#h":[channel]}
        ])
        .to_string();
        assert!(parse_client_message(&wrong_kind).is_err());
    }

    #[test]
    fn accepts_kind_nine_channel_filter() {
        let channel = Uuid::new_v4();
        let raw = serde_json::json!([
            "REQ", "sub", {"kinds":[9], "#h":[channel]}
        ])
        .to_string();
        let parsed = parse_client_message(&raw).expect("valid REQ");
        assert!(matches!(
            parsed,
            ClientMessage::Req { filters, .. }
                if filters[0].limit == Some(MAX_FILTER_LIMIT)
        ));
    }

    #[test]
    fn rejects_oversized_history_page() {
        let channel = Uuid::new_v4();
        let raw = serde_json::json!([
            "REQ", "sub", {"kinds":[9], "#h":[channel], "limit":MAX_FILTER_LIMIT + 1}
        ])
        .to_string();
        assert!(parse_client_message(&raw).is_err());
    }

    #[test]
    fn parses_strict_binding_handshake() {
        let community_id = Uuid::new_v4();
        let raw = serde_json::json!([
            "BUZZ-EDGE",
            "BIND",
            {
                "canonical_origin": "wss://relay.example.com",
                "community_id": community_id,
            }
        ])
        .to_string();
        assert!(matches!(
            parse_client_message(&raw).expect("handshake"),
            ClientMessage::Handshake { community_id: parsed, .. } if parsed == community_id
        ));

        let extra = serde_json::json!([
            "BUZZ-EDGE", "BIND",
            {"canonical_origin":"wss://relay.example.com", "community_id":community_id, "extra":true}
        ])
        .to_string();
        assert!(parse_client_message(&extra).is_err());
    }

    #[test]
    fn event_requires_exactly_one_channel_tag() {
        let keys = Keys::generate();
        let channel = Uuid::new_v4();
        let event = EventBuilder::new(Kind::Custom(9), "hello")
            .tags([Tag::parse(["h", channel.to_string().as_str()]).expect("tag")])
            .sign_with_keys(&keys)
            .expect("sign");
        assert_eq!(event_channel(&event).expect("channel"), channel);

        let second = Uuid::new_v4();
        let ambiguous = EventBuilder::new(Kind::Custom(9), "hello")
            .tags([
                Tag::parse(["h", channel.to_string().as_str()]).expect("tag"),
                Tag::parse(["h", second.to_string().as_str()]).expect("tag"),
            ])
            .sign_with_keys(&keys)
            .expect("sign");
        assert!(event_channel(&ambiguous).is_err());
    }
}
