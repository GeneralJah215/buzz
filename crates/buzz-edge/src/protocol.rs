//! Narrow Nostr protocol surface accepted by the phase-1 edge relay.

use nostr::{Alphabet, Event, Filter, Kind, SingleLetterTag};
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
