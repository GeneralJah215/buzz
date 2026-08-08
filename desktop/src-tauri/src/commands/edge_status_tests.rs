use super::*;

/// A payload shaped exactly like the sidecar's `status_payload` output.
fn sidecar_payload() -> serde_json::Value {
    serde_json::json!({
        "summary": {
            "pending": 3,
            "claimed": 1,
            "deliveredExact": 12,
            "deliveredViaDigest": 4,
            "quarantined": 2
        },
        "quarantined": [{
            "eventId": "aa".repeat(32),
            "channelId": "6f1b6f0a-0000-4000-8000-000000000001",
            "author": "bb".repeat(32),
            "createdAt": 1_700_000_000_i64,
            "attempts": 5,
            "reason": "membership revoked",
            "updatedAt": 1_700_000_500_i64
        }],
        "waitingAuthors": [{
            "author": "cc".repeat(32),
            "pending": 3,
            "oldestPendingAt": 1_699_999_000_i64
        }]
    })
}

/// The wire shape is a contract with the sidecar. If either side renames a
/// field, this must fail rather than quietly deserializing to zero.
#[test]
fn parses_the_sidecar_status_payload() {
    let parsed: EdgeStatusPayload = serde_json::from_value(sidecar_payload()).expect("parse");
    assert_eq!(parsed.summary.pending, 3);
    assert_eq!(parsed.summary.claimed, 1);
    assert_eq!(parsed.summary.delivered_exact, 12);
    assert_eq!(parsed.summary.delivered_via_digest, 4);
    assert_eq!(parsed.summary.quarantined, 2);
    assert_eq!(parsed.quarantined.len(), 1);
    assert_eq!(parsed.quarantined[0].attempts, 5);
    assert_eq!(parsed.quarantined[0].reason, "membership revoked");
    assert_eq!(parsed.waiting_authors.len(), 1);
    assert_eq!(parsed.waiting_authors[0].pending, 3);
    assert_eq!(parsed.waiting_authors[0].oldest_pending_at, 1_699_999_000);
}

/// The two synced states are stored and carried separately all the way to the
/// frontend. Collapsing them into one number is the exact regression the
/// spec's success criterion (3) forbids.
#[test]
fn keeps_exact_and_digest_delivery_counts_apart() {
    let parsed: EdgeStatusPayload = serde_json::from_value(sidecar_payload()).expect("parse");
    assert_ne!(parsed.summary.delivered_exact, parsed.summary.delivered_via_digest);
    let round_tripped = serde_json::to_value(parsed.summary).expect("serialize");
    assert_eq!(round_tripped.get("deliveredExact").and_then(|v| v.as_u64()), Some(12));
    assert_eq!(
        round_tripped.get("deliveredViaDigest").and_then(|v| v.as_u64()),
        Some(4)
    );
}

/// A sidecar that only ever pushed local events sends no quarantine or waiting
/// arrays at all. That is an empty list, not a parse failure.
#[test]
fn tolerates_a_payload_with_only_a_summary() {
    let parsed: EdgeStatusPayload = serde_json::from_value(serde_json::json!({
        "summary": {
            "pending": 0,
            "claimed": 0,
            "deliveredExact": 0,
            "deliveredViaDigest": 0,
            "quarantined": 0
        }
    }))
    .expect("parse");
    assert!(parsed.quarantined.is_empty());
    assert!(parsed.waiting_authors.is_empty());
}

/// A version-skewed sidecar sending the wrong type must fail loudly here,
/// rather than reaching the UI as a half-parsed object.
#[test]
fn rejects_a_summary_of_the_wrong_shape() {
    let mut payload = sidecar_payload();
    payload["summary"]["pending"] = serde_json::json!("three");
    let parsed = serde_json::from_value::<EdgeStatusPayload>(payload);
    assert!(parsed.is_err(), "a string count must not parse as a u64");
}

/// The quarantine list is community-wide, so it must never widen into a way to
/// read another identity's messages. This asserts the struct has no field that
/// could carry content at all.
#[test]
fn quarantine_rows_carry_no_message_content() {
    let mut payload = sidecar_payload();
    payload["quarantined"][0]["content"] = serde_json::json!("secret message body");
    let parsed: EdgeStatusPayload = serde_json::from_value(payload).expect("parse");
    let serialized = serde_json::to_string(&parsed.quarantined[0]).expect("serialize");
    assert!(
        !serialized.contains("secret message body"),
        "quarantine rows must not carry message content: {serialized}"
    );
}

/// `false` is a real answer — the row may belong to someone else, or may
/// already have been retried from another window.
#[test]
fn reads_both_requeue_outcomes() {
    assert_eq!(
        parse_requeue_reply(serde_json::json!({ "requeued": true })),
        Ok(true)
    );
    assert_eq!(
        parse_requeue_reply(serde_json::json!({ "requeued": false })),
        Ok(false)
    );
}

/// A missing or non-boolean field means the sidecar never answered. Reporting
/// that as `false` would tell the operator their retry was refused when in
/// fact nobody knows what happened.
#[test]
fn refuses_to_read_a_missing_requeue_field() {
    assert!(parse_requeue_reply(serde_json::json!({})).is_err());
    assert!(parse_requeue_reply(serde_json::json!({ "requeued": "yes" })).is_err());
    assert!(parse_requeue_reply(serde_json::json!({ "requeued": 1 })).is_err());
    assert!(parse_requeue_reply(serde_json::json!(null)).is_err());
}

/// The delivery-state reply is a list of `[event_id, state]` pairs, and the
/// state strings are the ones the frontend switches on.
#[test]
fn parses_delivery_state_pairs() {
    let value = serde_json::json!([
        ["aa".repeat(32), "pending"],
        ["bb".repeat(32), "syncedExact"],
        ["cc".repeat(32), "syncedViaDigest"],
        ["dd".repeat(32), "quarantined"],
    ]);
    let parsed: Vec<(String, String)> = serde_json::from_value(value).expect("parse");
    assert_eq!(parsed.len(), 4);
    assert_eq!(parsed[1].1, "syncedExact");
    assert_eq!(parsed[2].1, "syncedViaDigest");
    assert_ne!(
        parsed[1].1, parsed[2].1,
        "an exact replay and a digest replay are different states"
    );
}
