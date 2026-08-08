//! Narrow Nostr protocol surface accepted by the phase-1 edge relay.

use nostr::{Alphabet, Event, EventId, Filter, Kind, SingleLetterTag};
use serde_json::Value;
use uuid::Uuid;

use crate::storage::{
    truncate_quarantine_reason, EventDeliveryRow, OutboxSummary, QuarantinedRow, RequeueOutcome,
    WaitingAuthor,
};

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
    /// Ask for the operator-facing sync status of the bound community.
    ///
    /// Unlike [`ClientMessage::Drain`], this is not scoped to the session
    /// principal: the Desktop operator has to be able to see that an *agent's*
    /// events are stuck, which is the whole point of the surface. What keeps
    /// that safe is that the reply carries metadata only — never message
    /// content — so one identity still cannot read another's messages.
    Status {
        /// Client-chosen request ID echoed in the reply.
        req_id: String,
        /// Maximum quarantine rows to return.
        limit: usize,
    },
    /// Manually retry one quarantined event (§11, "manual retry from the
    /// quarantine UI").
    ///
    /// Reading status is community-wide; *writing* is not. The storage layer
    /// filters on the session principal, so this can only ever move the
    /// caller's own row.
    Requeue {
        /// Client-chosen request ID echoed in the reply.
        req_id: String,
        /// Which quarantined event to return to `pending`.
        event_id: EventId,
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

/// Refuse any field the verb does not define.
///
/// Counting keys is not enough when a field is optional: an unknown field can
/// sit in the optional one's slot, keep the count legal, and be silently
/// dropped while the optional field quietly defaults. A request that names
/// something the sidecar does not understand must be refused, not guessed at —
/// that is what stops a client believing it constrained a request it did not.
fn reject_unknown_fields(
    object: &serde_json::Map<String, Value>,
    verb: &str,
    allowed: &[&str],
) -> Result<(), String> {
    match object.keys().find(|key| !allowed.contains(&key.as_str())) {
        Some(unknown) => Err(format!(
            "BUZZ-EDGE {verb} does not accept the field {unknown}"
        )),
        None => Ok(()),
    }
}

fn parse_drain(value: &Value) -> Result<ClientMessage, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "BUZZ-EDGE DRAIN requires an object".to_string())?;
    reject_unknown_fields(object, "DRAIN", &["claim_token", "limit"])?;
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
    // Bounded here, at the edge of the process. `reason` is free text chosen by
    // a drain client, stored verbatim, and then shown to *every other* local
    // identity's operator list — so an unbounded one is both a content channel
    // (ack your own message body and it lands in someone else's UI) and a
    // memory amplifier (a 1.6 MB frame per row, times a page of rows).
    //
    // Truncated, never rejected: refusing the ack would leave the row leased
    // and the author retrying the same oversized ack forever, which strands the
    // row instead of bounding it.
    let reason = object
        .get("reason")
        .and_then(Value::as_str)
        .map(truncate_quarantine_reason);
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

/// Largest quarantine page a single status request may ask for.
pub(crate) const MAX_STATUS_PAGE: usize = 500;

fn parse_req_id(object: &serde_json::Map<String, Value>, verb: &str) -> Result<String, String> {
    let req_id = object
        .get("req_id")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("BUZZ-EDGE {verb} requires a string req_id"))?
        .to_string();
    if req_id.trim().is_empty() {
        return Err("req_id must not be empty".to_string());
    }
    if req_id.len() > MAX_SUB_ID_LENGTH {
        return Err("req_id is too long".to_string());
    }
    Ok(req_id)
}

fn parse_status(value: &Value) -> Result<ClientMessage, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "BUZZ-EDGE STATUS requires an object".to_string())?;
    reject_unknown_fields(object, "STATUS", &["req_id", "limit"])?;
    let req_id = parse_req_id(object, "STATUS")?;
    let limit = object
        .get("limit")
        .and_then(Value::as_u64)
        .map(|value| value as usize)
        .unwrap_or(MAX_STATUS_PAGE)
        .clamp(1, MAX_STATUS_PAGE);
    Ok(ClientMessage::Status { req_id, limit })
}

fn parse_requeue(value: &Value) -> Result<ClientMessage, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "BUZZ-EDGE REQUEUE requires an object".to_string())?;
    reject_unknown_fields(object, "REQUEUE", &["req_id", "event_id"])?;
    let req_id = parse_req_id(object, "REQUEUE")?;
    let event_id = object
        .get("event_id")
        .and_then(Value::as_str)
        .ok_or_else(|| "event_id must be a string".to_string())?;
    let event_id =
        EventId::from_hex(event_id).map_err(|error| format!("invalid event_id: {error}"))?;
    Ok(ClientMessage::Requeue { req_id, event_id })
}

/// Format the operator-facing status reply.
///
/// The JSON is written out by hand in camelCase rather than derived from the
/// storage structs, because the wire shape is a contract with the Desktop
/// frontend and must not silently follow a rename on the Rust side.
///
/// `quarantined` and `waiting` carry identifiers, counts, and failure reasons
/// only. Message content is deliberately absent — see [`ClientMessage::Status`].
pub fn status_reply(
    req_id: &str,
    summary: &OutboxSummary,
    quarantined: &[QuarantinedRow],
    waiting: &[WaitingAuthor],
) -> String {
    let mut payload = status_payload(summary, quarantined, waiting);
    if let Some(object) = payload.as_object_mut() {
        object.insert("req_id".to_string(), Value::String(req_id.to_string()));
    }
    serde_json::json!(["BUZZ-EDGE", "STATUS-REPLY", payload]).to_string()
}

/// The status body shared by the WebSocket reply and the `/status` HTTP route.
///
/// Both transports must describe the same state in the same shape; building it
/// once is what guarantees a field added for one client is visible to the other.
pub fn status_payload(
    summary: &OutboxSummary,
    quarantined: &[QuarantinedRow],
    waiting: &[WaitingAuthor],
) -> Value {
    let quarantined: Vec<Value> = quarantined
        .iter()
        .map(|row| {
            serde_json::json!({
                "eventId": row.event_id.to_hex(),
                "channelId": row.channel_id.to_string(),
                "author": row.author.to_hex(),
                "createdAt": row.created_at,
                "attempts": row.attempts,
                "reason": row.reason,
                "carriedByDigest": row.carried_by_digest,
                "demotionReason": row.demotion_reason,
                "updatedAt": row.updated_at,
            })
        })
        .collect();
    let waiting: Vec<Value> = waiting
        .iter()
        .map(|row| {
            serde_json::json!({
                "author": row.author.to_hex(),
                "pending": row.pending,
                "ancestorBlocked": row.ancestor_blocked,
                "pendingViaDigest": row.pending_via_digest,
                "oldestPendingAt": row.oldest_pending_at,
            })
        })
        .collect();
    serde_json::json!({
        "summary": {
            "pending": summary.pending,
            "pendingViaDigest": summary.pending_via_digest,
            "claimed": summary.claimed,
            "syncedExact": summary.synced_exact,
            "syncedViaDigest": summary.synced_via_digest,
            "quarantined": summary.quarantined,
        },
        "quarantined": quarantined,
        "waitingAuthors": waiting,
    })
}

/// The per-event delivery-state body shared by the WebSocket and HTTP surfaces.
///
/// Objects rather than `[id, state]` pairs, because a demoted row needs its
/// `demotionReason` alongside the label: "pendingViaDigest" on its own tells
/// the operator where the event went but not why, and why is the actionable
/// half ("older than the relay drift window" is normal; "permanently rejected
/// upstream" is not).
pub fn delivery_states_payload(states: &[EventDeliveryRow]) -> Value {
    let rows: Vec<Value> = states
        .iter()
        .map(|row| {
            serde_json::json!({
                "eventId": row.event_id.to_hex(),
                "state": row.state,
                "demotionReason": row.demotion_reason,
            })
        })
        .collect();
    serde_json::json!(rows)
}

/// Format the reply to a manual quarantine retry.
///
/// `requeued: false` is a normal answer, not an error: the row may already
/// have been retried from another window, or it may belong to someone else.
/// `outcome` says which of those it was, because "carriedByDigest" is a state
/// the operator can act on (the edge is already carrying the event upstream;
/// stop pressing Retry) and a bare `false` looks identical to a lost row.
pub fn requeue_reply(req_id: &str, outcome: RequeueOutcome) -> String {
    let mut payload = requeue_payload(outcome);
    if let Some(object) = payload.as_object_mut() {
        object.insert("req_id".to_string(), Value::String(req_id.to_string()));
    }
    serde_json::json!(["BUZZ-EDGE", "REQUEUE-REPLY", payload]).to_string()
}

/// The requeue body shared by the WebSocket reply and the `/requeue` route.
pub fn requeue_payload(outcome: RequeueOutcome) -> Value {
    serde_json::json!({
        "requeued": outcome.requeued(),
        "outcome": outcome,
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
                Some("STATUS") => return parse_status(&values[2]),
                Some("REQUEUE") => return parse_requeue(&values[2]),
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
    use crate::storage::{EventDeliveryState, MAX_QUARANTINE_REASON_BYTES};
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

    // ── Operator status + manual requeue wire format (§11) ─────────────────

    /// One real signed kind-9 event, so a formatter that started echoing its
    /// source has something quotable to leak.
    fn signed_message(keys: &Keys, channel: Uuid, content: &str) -> Event {
        EventBuilder::new(Kind::Custom(9), content)
            .tags([Tag::parse(["h", channel.to_string().as_str()]).expect("h")])
            .sign_with_keys(keys)
            .expect("sign")
    }

    #[test]
    fn status_limit_is_defaulted_and_clamped() {
        let no_limit = serde_json::json!(["BUZZ-EDGE", "STATUS", {"req_id":"r"}]).to_string();
        match parse_client_message(&no_limit).expect("parse") {
            ClientMessage::Status { limit, .. } => assert_eq!(limit, MAX_STATUS_PAGE),
            other => panic!("expected Status, got {other:?}"),
        }
        // The quarantine list is an operator triage surface, not an export. An
        // unbounded page would pull the whole table in behind the store mutex.
        let huge =
            serde_json::json!(["BUZZ-EDGE", "STATUS", {"req_id":"r","limit":1000000}]).to_string();
        match parse_client_message(&huge).expect("parse") {
            ClientMessage::Status { limit, .. } => assert_eq!(limit, MAX_STATUS_PAGE),
            other => panic!("expected Status, got {other:?}"),
        }
        // A zero-row page would answer every status request with an empty
        // quarantine list, which reads as "nothing is wrong here".
        let zero = serde_json::json!(["BUZZ-EDGE", "STATUS", {"req_id":"r","limit":0}]).to_string();
        match parse_client_message(&zero).expect("parse") {
            ClientMessage::Status { limit, .. } => assert_eq!(limit, 1),
            other => panic!("expected Status, got {other:?}"),
        }
    }

    #[test]
    fn a_status_request_requires_a_usable_req_id() {
        // Every way of failing to supply a request ID has to say which rule
        // fired: the operator window matches replies by this string, and a
        // single opaque refusal gives the client nothing to correct.
        let missing = serde_json::json!(["BUZZ-EDGE", "STATUS", {"limit":10}]).to_string();
        let missing = parse_client_message(&missing).expect_err("missing req_id");
        // A non-string is the same failure as an absent one on purpose: both
        // mean "no usable req_id was supplied".
        let non_string = serde_json::json!(["BUZZ-EDGE", "STATUS", {"req_id":7}]).to_string();
        let non_string = parse_client_message(&non_string).expect_err("non-string req_id");
        assert!(
            missing.contains("STATUS requires a string req_id"),
            "{missing}"
        );
        assert_eq!(missing, non_string);

        let empty = serde_json::json!(["BUZZ-EDGE", "STATUS", {"req_id":""}]).to_string();
        let empty = parse_client_message(&empty).expect_err("empty req_id");
        let blank = serde_json::json!(["BUZZ-EDGE", "STATUS", {"req_id":"   "}]).to_string();
        let blank = parse_client_message(&blank).expect_err("whitespace-only req_id");
        assert!(empty.contains("must not be empty"), "{empty}");
        assert_eq!(
            empty, blank,
            "a blank ID is an empty ID, not a length problem"
        );

        let long = "r".repeat(MAX_SUB_ID_LENGTH + 1);
        let long = serde_json::json!(["BUZZ-EDGE", "STATUS", {"req_id":long}]).to_string();
        let long = parse_client_message(&long).expect_err("over-long req_id");
        assert!(long.contains("too long"), "{long}");

        // The three refusals must stay tellable apart; collapsing them into one
        // generic message is the regression this guards.
        assert_ne!(missing, empty);
        assert_ne!(empty, long);
        assert_ne!(missing, long);

        // The boundary itself is accepted, so the limit is a limit and not an
        // off-by-one.
        let at_limit = "r".repeat(MAX_SUB_ID_LENGTH);
        let at_limit = serde_json::json!(["BUZZ-EDGE", "STATUS", {"req_id":at_limit}]).to_string();
        assert!(parse_client_message(&at_limit).is_ok());
    }

    #[test]
    fn a_status_request_refuses_an_unknown_field() {
        // A field the sidecar silently drops is a field the client believes it
        // sent. `limit` is optional, so an unknown key can occupy its slot
        // without changing the object's size — the extra field has to be
        // refused on its own merits, not by counting keys.
        let beside_limit = serde_json::json!([
            "BUZZ-EDGE", "STATUS",
            {"req_id":"r","limit":10,"author":"deadbeef"}
        ])
        .to_string();
        assert!(
            parse_client_message(&beside_limit).is_err(),
            "an extra field must be refused, not silently ignored"
        );

        let instead_of_limit = serde_json::json!([
            "BUZZ-EDGE", "STATUS",
            {"req_id":"r","author":"deadbeef"}
        ])
        .to_string();
        assert!(
            parse_client_message(&instead_of_limit).is_err(),
            "an extra field must be refused even when it takes the optional \
             limit's place, not silently ignored"
        );
    }

    #[test]
    fn a_requeue_names_the_event_it_retries() {
        let keys = Keys::generate();
        let event = signed_message(&keys, Uuid::new_v4(), "queued");
        let hex = event.id.to_hex();
        let raw = serde_json::json!([
            "BUZZ-EDGE", "REQUEUE",
            {"req_id":"r","event_id":hex}
        ])
        .to_string();
        match parse_client_message(&raw).expect("parse") {
            ClientMessage::Requeue { req_id, event_id } => {
                assert_eq!(req_id, "r");
                assert_eq!(event_id, event.id);
            }
            other => panic!("expected Requeue, got {other:?}"),
        }

        // A truncated or mistyped ID must never reach the storage layer, where
        // it would be matched against every row.
        let truncated = &hex[..hex.len() - 1];
        for bad in ["not-hex", "", truncated] {
            let raw = serde_json::json!([
                "BUZZ-EDGE", "REQUEUE",
                {"req_id":"r","event_id":bad}
            ])
            .to_string();
            assert!(parse_client_message(&raw).is_err(), "event_id {bad:?}");
        }

        // The shared req_id helper is told which verb it is parsing for; if it
        // is not, a REQUEUE failure reports itself as a STATUS failure.
        let no_req_id = serde_json::json!([
            "BUZZ-EDGE", "REQUEUE",
            {"event_id":hex}
        ])
        .to_string();
        let error = parse_client_message(&no_req_id).expect_err("missing req_id");
        assert!(error.contains("REQUEUE"), "{error}");
    }

    #[test]
    fn a_requeue_request_refuses_an_unknown_field() {
        let hex = EventId::all_zeros().to_hex();
        let extra = serde_json::json!([
            "BUZZ-EDGE", "REQUEUE",
            {"req_id":"r","event_id":hex,"author":"deadbeef"}
        ])
        .to_string();
        assert!(
            parse_client_message(&extra).is_err(),
            "an extra field must be refused, not silently ignored"
        );
    }

    #[test]
    fn an_unknown_buzz_edge_operation_is_still_refused() {
        // STATUS and REQUEUE are matched ahead of the BIND fall-through. A
        // wildcard added beside them would make every unrecognised verb parse
        // as whichever arm caught it.
        for verb in ["STATUS-REPLY", "REQUEUE-REPLY", "STATUSES", "PURGE"] {
            let raw = serde_json::json!(["BUZZ-EDGE", verb, {"req_id":"r"}]).to_string();
            let error = parse_client_message(&raw).expect_err(verb);
            assert!(
                error.contains("unsupported BUZZ-EDGE operation"),
                "{verb}: {error}"
            );
        }
    }

    #[test]
    fn status_reply_uses_the_camel_case_shape_the_frontend_reads() {
        // The keys below are a contract with Desktop. Every count is a
        // different number so a pair of swapped fields cannot pass.
        let author = Keys::generate();
        let waiter = Keys::generate();
        let channel = Uuid::new_v4();
        let event = signed_message(&author, channel, "queued");
        let summary = OutboxSummary {
            pending: 3,
            pending_via_digest: 17,
            claimed: 5,
            synced_exact: 7,
            synced_via_digest: 11,
            quarantined: 13,
        };
        let row = QuarantinedRow {
            event_id: event.id,
            channel_id: channel,
            author: author.public_key(),
            created_at: 1_700_000_001,
            attempts: 4,
            reason: "upstream refused: membership revoked".to_string(),
            carried_by_digest: true,
            demotion_reason: Some("permanently rejected upstream".to_string()),
            updated_at: 1_700_000_002,
        };
        let waiting = WaitingAuthor {
            author: waiter.public_key(),
            pending: 9,
            ancestor_blocked: 19,
            pending_via_digest: 23,
            oldest_pending_at: 1_699_999_999,
        };

        let frame = status_reply(
            "req-1",
            &summary,
            std::slice::from_ref(&row),
            std::slice::from_ref(&waiting),
        );
        let parsed: Value = serde_json::from_str(&frame).expect("json");
        assert_eq!(parsed[0], "BUZZ-EDGE");
        assert_eq!(parsed[1], "STATUS-REPLY");
        let body = &parsed[2];
        assert_eq!(body["req_id"], "req-1");
        assert_eq!(body["summary"]["pending"], 3);
        assert_eq!(body["summary"]["pendingViaDigest"], 17);
        assert_eq!(body["summary"]["claimed"], 5);
        // One vocabulary end to end: the canonical-history counts are named
        // `synced*` here and in `EventDeliveryState` both, never `delivered*`.
        assert_eq!(body["summary"]["syncedExact"], 7);
        assert_eq!(body["summary"]["syncedViaDigest"], 11);
        assert_eq!(body["summary"]["quarantined"], 13);
        let mut summary_keys: Vec<&str> = body["summary"]
            .as_object()
            .expect("summary object")
            .keys()
            .map(String::as_str)
            .collect();
        summary_keys.sort_unstable();
        assert_eq!(
            summary_keys,
            [
                "claimed",
                "pending",
                "pendingViaDigest",
                "quarantined",
                "syncedExact",
                "syncedViaDigest"
            ],
            "the summary key set is a contract with Desktop"
        );

        let quarantined = &body["quarantined"][0];
        assert_eq!(quarantined["eventId"], event.id.to_hex());
        assert_eq!(quarantined["channelId"], channel.to_string());
        assert_eq!(quarantined["author"], author.public_key().to_hex());
        assert_eq!(quarantined["createdAt"], 1_700_000_001);
        assert_eq!(quarantined["attempts"], 4);
        assert_eq!(
            quarantined["reason"],
            "upstream refused: membership revoked"
        );
        assert_eq!(quarantined["updatedAt"], 1_700_000_002);
        assert_eq!(
            quarantined["carriedByDigest"], true,
            "a row the edge already carries must say so, or Retry only hides it"
        );
        assert_eq!(
            quarantined["demotionReason"],
            "permanently rejected upstream"
        );

        let waiting = &body["waitingAuthors"][0];
        assert_eq!(waiting["author"], waiter.public_key().to_hex());
        assert_eq!(waiting["pending"], 9);
        assert_eq!(waiting["ancestorBlocked"], 19);
        assert_eq!(waiting["pendingViaDigest"], 23);
        assert_eq!(waiting["oldestPendingAt"], 1_699_999_999);
    }

    #[test]
    fn status_reply_never_carries_message_content() {
        // The quarantine list is community-wide, but the rows in it belong to
        // every local identity. One identity's message body appearing in
        // another's operator list is a private message leaked through a status
        // surface — the privacy invariant of this whole surface.
        const BODY: &str = "PRIVATE-BODY-1f4c-do-not-leak";
        let author = Keys::generate();
        let channel = Uuid::new_v4();
        let event = signed_message(&author, channel, BODY);
        let row = QuarantinedRow {
            event_id: event.id,
            channel_id: channel,
            author: author.public_key(),
            created_at: 1_700_000_001,
            attempts: 1,
            reason: "upstream refused: oversized".to_string(),
            carried_by_digest: false,
            demotion_reason: None,
            updated_at: 1_700_000_002,
        };

        let frame = status_reply("req-1", &OutboxSummary::default(), &[row], &[]);
        assert!(
            frame.contains(&event.id.to_hex()),
            "the row must actually be in the frame, or its absence of content \
             proves nothing: {frame}"
        );
        assert!(
            !frame.contains(BODY),
            "message content must never appear in a status reply: {frame}"
        );

        // Structural backstop for the same invariant: the entry carries
        // identifiers, counts, and a failure reason. Adding a key here has to
        // be a deliberate decision, not a rename or a serde derive away.
        let parsed: Value = serde_json::from_str(&frame).expect("json");
        let mut keys: Vec<&str> = parsed[2]["quarantined"][0]
            .as_object()
            .expect("quarantine entry")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "attempts",
                "author",
                "carriedByDigest",
                "channelId",
                "createdAt",
                "demotionReason",
                "eventId",
                "reason",
                "updatedAt"
            ]
        );
    }

    #[test]
    fn requeue_reply_reports_a_json_boolean_and_a_distinguishable_outcome() {
        // Desktop branches on `requeued` directly. A stringified "false" is
        // truthy in JavaScript, so it would report a refused retry as a
        // successful one.
        //
        // `outcome` is the half a boolean cannot carry: "no such row of yours"
        // and "the edge is already carrying this row upstream, stop pressing
        // Retry" are the same `false` and completely different advice.
        for (outcome, requeued, name) in [
            (RequeueOutcome::Requeued, true, "requeued"),
            (RequeueOutcome::NotFound, false, "notFound"),
            (RequeueOutcome::CarriedByDigest, false, "carriedByDigest"),
        ] {
            let parsed: Value =
                serde_json::from_str(&requeue_reply("req-2", outcome)).expect("json");
            assert_eq!(parsed[0], "BUZZ-EDGE");
            assert_eq!(parsed[1], "REQUEUE-REPLY");
            assert!(
                parsed[2]["requeued"].is_boolean(),
                "requeued must be a JSON boolean, got {}",
                parsed[2]["requeued"]
            );
            assert_eq!(parsed[2]["requeued"], requeued);
            assert_eq!(parsed[2]["outcome"], name);
        }
    }

    #[test]
    fn delivery_states_carry_the_label_and_the_reason_it_was_demoted() {
        // A demoted row's label says where the event went; `demotionReason`
        // says why, and why is the half the operator can act on.
        let author = Keys::generate();
        let channel = Uuid::new_v4();
        let normal = signed_message(&author, channel, "still exact");
        let demoted = signed_message(&author, channel, "demoted");
        let payload = delivery_states_payload(&[
            EventDeliveryRow {
                event_id: normal.id,
                state: EventDeliveryState::Pending,
                demotion_reason: None,
            },
            EventDeliveryRow {
                event_id: demoted.id,
                state: EventDeliveryState::PendingViaDigest,
                demotion_reason: Some("older than the relay drift window".to_string()),
            },
        ]);
        assert_eq!(payload[0]["eventId"], normal.id.to_hex());
        assert_eq!(payload[0]["state"], "pending");
        assert!(payload[0]["demotionReason"].is_null());
        assert_eq!(payload[1]["state"], "pendingViaDigest");
        assert_eq!(
            payload[1]["demotionReason"],
            "older than the relay drift window"
        );
    }

    #[test]
    fn a_drain_ack_reason_is_truncated_rather_than_stored_whole() {
        // The reason is free text that ends up in every other local identity's
        // operator list. Unbounded, it is both a way to push a message body
        // into someone else's UI and a way to make one status page hundreds of
        // megabytes: MAX_MESSAGE_BYTES allows a 1.6 MB reason per row.
        let event_id = EventId::from_hex(&"ab".repeat(32)).expect("event id");
        let huge = "x".repeat(MAX_QUARANTINE_REASON_BYTES * 40);
        let raw = serde_json::json!([
            "BUZZ-EDGE", "DRAIN-ACK",
            {
                "claim_token": "t",
                "event_id": event_id.to_hex(),
                "outcome": "rejected",
                "reason": huge,
            }
        ])
        .to_string();
        let parsed = parse_client_message(&raw).expect("DRAIN-ACK");
        let ClientMessage::DrainAck { reason, .. } = parsed else {
            panic!("expected a DRAIN-ACK");
        };
        let reason = reason.expect("a rejected outcome keeps its reason");
        assert!(
            reason.len() <= MAX_QUARANTINE_REASON_BYTES,
            "reason kept {} bytes",
            reason.len()
        );
        // Truncated, not refused: refusing the ack would leave the row leased
        // and the author retrying the same oversized frame forever.
        assert!(reason.starts_with("xxx"));
        assert!(reason.ends_with('\u{2026}'));
    }

    #[test]
    fn truncating_a_reason_never_splits_a_multi_byte_character() {
        // The cut lands inside a euro sign; splitting it would produce invalid
        // UTF-8 and panic on the slice.
        let mut reason = "a".repeat(MAX_QUARANTINE_REASON_BYTES - 4);
        reason.push_str(&"\u{20ac}".repeat(8));
        assert!(reason.len() > MAX_QUARANTINE_REASON_BYTES);
        let truncated = truncate_quarantine_reason(&reason);
        assert!(truncated.len() <= MAX_QUARANTINE_REASON_BYTES);
        assert!(truncated.ends_with('\u{2026}'));
        assert_eq!(
            truncated
                .chars()
                .filter(|character| *character == '\u{20ac}')
                .count(),
            0,
            "the character straddling the cut is dropped whole, not split"
        );
    }

    #[test]
    fn both_replies_echo_the_request_id_verbatim() {
        // The client matches a reply to the window that asked for it by exact
        // string. Anything that trims, slugs, or truncates the ID orphans the
        // reply in a window that is still waiting for it.
        let req_id = "  status/42 ünïcode \"quoted\"  ";
        let status: Value =
            serde_json::from_str(&status_reply(req_id, &OutboxSummary::default(), &[], &[]))
                .expect("json");
        assert_eq!(status[2]["req_id"], req_id);
        let requeue: Value =
            serde_json::from_str(&requeue_reply(req_id, RequeueOutcome::Requeued)).expect("json");
        assert_eq!(requeue[2]["req_id"], req_id);
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
