use super::*;

/// A payload shaped exactly like the sidecar's `status_payload` output.
///
/// Keep this in step with `status_payload` in
/// `crates/buzz-edge/src/protocol.rs`. Every count is a different number so a
/// pair of crossed fields cannot pass.
fn sidecar_payload() -> serde_json::Value {
    serde_json::json!({
        "summary": {
            "pending": 3,
            "pendingViaDigest": 17,
            "claimed": 1,
            "syncedExact": 12,
            "syncedViaDigest": 4,
            "quarantined": 2
        },
        "quarantined": [{
            "eventId": "aa".repeat(32),
            "channelId": "6f1b6f0a-0000-4000-8000-000000000001",
            "author": "bb".repeat(32),
            "createdAt": 1_700_000_000_i64,
            "attempts": 5,
            "reason": "membership revoked",
            "carriedByDigest": true,
            "demotionReason": "older than the relay drift window",
            "updatedAt": 1_700_000_500_i64
        }],
        "waitingAuthors": [{
            "author": "cc".repeat(32),
            "pending": 3,
            "ancestorBlocked": 19,
            "pendingViaDigest": 23,
            "oldestPendingAt": 1_699_999_000_i64,
            "oldestClaimableAt": 1_699_999_100_i64,
            "oldestAncestorBlockedAt": 1_699_999_200_i64
        }]
    })
}

/// The wire shape is a contract with the sidecar. If either side renames a
/// field, this must fail rather than quietly deserializing to zero.
#[test]
fn parses_the_sidecar_status_payload() {
    let parsed: EdgeStatusPayload = serde_json::from_value(sidecar_payload()).expect("parse");
    assert_eq!(parsed.summary.pending, 3);
    assert_eq!(parsed.summary.pending_via_digest, 17);
    assert_eq!(parsed.summary.claimed, 1);
    assert_eq!(parsed.summary.synced_exact, 12);
    assert_eq!(parsed.summary.synced_via_digest, 4);
    assert_eq!(parsed.summary.quarantined, 2);
    assert_eq!(parsed.quarantined.len(), 1);
    assert_eq!(parsed.quarantined[0].attempts, 5);
    assert_eq!(parsed.quarantined[0].reason, "membership revoked");
    assert!(parsed.quarantined[0].carried_by_digest);
    assert_eq!(
        parsed.quarantined[0].demotion_reason.as_deref(),
        Some("older than the relay drift window")
    );
    assert_eq!(parsed.waiting_authors.len(), 1);
    assert_eq!(parsed.waiting_authors[0].pending, 3);
    assert_eq!(parsed.waiting_authors[0].ancestor_blocked, 19);
    assert_eq!(parsed.waiting_authors[0].pending_via_digest, 23);
    assert_eq!(parsed.waiting_authors[0].oldest_pending_at, 1_699_999_000);
    assert_eq!(
        parsed.waiting_authors[0].oldest_claimable_at,
        Some(1_699_999_100)
    );
    assert_eq!(
        parsed.waiting_authors[0].oldest_ancestor_blocked_at,
        Some(1_699_999_200)
    );
}

/// The per-bucket ages are nullable but NOT optional, for the same reason
/// `demotionReason` is: an absent key is a sidecar too old to answer, and
/// reading that as "this bucket is empty" would put the aggregate age back
/// beside a count it does not belong to — the BUG-023 defect, restored by
/// silence instead of by code.
#[test]
fn a_missing_per_bucket_age_is_version_skew_not_an_empty_bucket() {
    for dropped in ["oldestClaimableAt", "oldestAncestorBlockedAt"] {
        let mut payload = sidecar_payload();
        payload["waitingAuthors"][0]
            .as_object_mut()
            .expect("waiting author")
            .remove(dropped);
        assert!(
            serde_json::from_value::<EdgeStatusPayload>(payload).is_err(),
            "a waiting author without '{dropped}' must fail loudly, not default to null"
        );
    }

    // An explicit null is the sidecar saying "that bucket is empty", and is
    // the answer an author with no blocked rows must get.
    let mut payload = sidecar_payload();
    payload["waitingAuthors"][0]["oldestAncestorBlockedAt"] = serde_json::Value::Null;
    let parsed: EdgeStatusPayload = serde_json::from_value(payload).expect("parse");
    assert_eq!(parsed.waiting_authors[0].oldest_ancestor_blocked_at, None);
}

/// The rename alarm. The sidecar settled on `synced*` for canonical history in
/// the summary as well as in `EventDeliveryState`, and this build reads exactly
/// those six keys. A rename on either side has to fail HERE — in one place,
/// loudly — rather than reaching the operator as a summary of zeroes.
///
/// The mirror of this assertion lives in
/// `status_reply_uses_the_camel_case_shape_the_frontend_reads`
/// (`crates/buzz-edge/src/protocol.rs`); the two key sets must be identical.
#[test]
fn the_summary_key_set_is_pinned_to_the_sidecars() {
    let parsed: EdgeStatusPayload = serde_json::from_value(sidecar_payload()).expect("parse");
    let round_tripped = serde_json::to_value(parsed.summary).expect("serialize");
    let mut keys: Vec<&str> = round_tripped
        .as_object()
        .expect("summary object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "claimed",
            "pending",
            "pendingViaDigest",
            "quarantined",
            "syncedExact",
            "syncedViaDigest"
        ],
        "the summary key set is a contract with the sidecar and with the frontend"
    );

    // And a payload missing any one of them is a skewed sidecar, not a quiet
    // one — including the three keys whose absence would otherwise read as
    // "nothing on that path", which is the most reassuring possible lie.
    for dropped in keys {
        let mut partial = sidecar_payload();
        partial["summary"]
            .as_object_mut()
            .expect("summary object")
            .remove(dropped);
        assert!(
            serde_json::from_value::<EdgeStatusPayload>(partial).is_err(),
            "a summary without '{dropped}' must fail loudly, not default to zero"
        );
    }
}

/// The two synced states are carried separately by the *parsing layer*, not
/// merely by the JSON this test wrote. Each assertion below fails if the two
/// counts are conflated in `EdgeDeliverySummary`:
///
/// 1. the two wire keys land in the two named fields, and swapping the values
///    on the wire swaps them in the struct (crossed or aliased wiring fails);
/// 2. a payload carrying only one of the two keys is rejected (a single field
///    serving both keys via an alias, or a defaulted second field, would parse).
#[test]
fn keeps_exact_and_digest_delivery_counts_apart() {
    let parsed: EdgeStatusPayload = serde_json::from_value(sidecar_payload()).expect("parse");
    assert_eq!(parsed.summary.synced_exact, 12);
    assert_eq!(parsed.summary.synced_via_digest, 4);

    let mut swapped = sidecar_payload();
    swapped["summary"]["syncedExact"] = serde_json::json!(4);
    swapped["summary"]["syncedViaDigest"] = serde_json::json!(12);
    let reparsed: EdgeStatusPayload = serde_json::from_value(swapped).expect("parse");
    assert_eq!(
        reparsed.summary.synced_exact, 4,
        "syncedExact must read the syncedExact key, nothing else"
    );
    assert_eq!(
        reparsed.summary.synced_via_digest, 12,
        "syncedViaDigest must read the syncedViaDigest key, nothing else"
    );

    for dropped in ["syncedExact", "syncedViaDigest"] {
        let mut partial = sidecar_payload();
        partial["summary"]
            .as_object_mut()
            .expect("summary object")
            .remove(dropped);
        assert!(
            serde_json::from_value::<EdgeStatusPayload>(partial).is_err(),
            "a summary without '{dropped}' must not parse — the two synced \
             counts are separate required fields, never one field under two names"
        );
    }

    let round_tripped = serde_json::to_value(parsed.summary).expect("serialize");
    assert_eq!(round_tripped.get("syncedExact").and_then(|v| v.as_u64()), Some(12));
    assert_eq!(
        round_tripped.get("syncedViaDigest").and_then(|v| v.as_u64()),
        Some(4)
    );
}

/// `pendingViaDigest` is not decoration on the pending count: those rows have
/// no author coming for them, so folding them into `pending` would tell the
/// operator to wait for somebody who is never going to claim them.
#[test]
fn the_two_pending_paths_are_two_counts_not_one() {
    let mut swapped = sidecar_payload();
    swapped["summary"]["pending"] = serde_json::json!(17);
    swapped["summary"]["pendingViaDigest"] = serde_json::json!(3);
    let parsed: EdgeStatusPayload = serde_json::from_value(swapped).expect("parse");
    assert_eq!(parsed.summary.pending, 17);
    assert_eq!(parsed.summary.pending_via_digest, 3);
}

/// Inverted on purpose (was `tolerates_a_payload_with_only_a_summary`).
///
/// The sidecar emits `summary`, `quarantined` and `waitingAuthors` on every
/// reply, empty arrays included. So a payload missing one of them is not a
/// quiet sidecar — it is a sidecar this build cannot read, and tolerating it
/// would render "nothing is stuck" over an unknown number of stuck events.
#[test]
fn rejects_a_payload_missing_any_top_level_key() {
    for dropped in ["summary", "quarantined", "waitingAuthors"] {
        let mut payload = sidecar_payload();
        payload
            .as_object_mut()
            .expect("payload object")
            .remove(dropped);
        assert!(
            serde_json::from_value::<EdgeStatusPayload>(payload).is_err(),
            "a payload without '{dropped}' must fail loudly, not default to empty"
        );
    }
}

/// The version-skew case the whole file exists for: a sidecar that renames
/// `summary` must not deserialize into all-zero counters.
#[test]
fn a_renamed_summary_key_is_an_error_not_six_zeroes() {
    let mut payload = sidecar_payload();
    let object = payload.as_object_mut().expect("payload object");
    let summary = object.remove("summary").expect("summary");
    object.insert("counts".to_string(), summary);

    let parsed = serde_json::from_value::<EdgeStatusPayload>(payload);
    assert!(
        parsed.is_err(),
        "a renamed summary must fail, never report an idle outbox"
    );
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

/// A quarantine row the edge is already carrying upstream has a Retry button
/// that cannot work — the sidecar answers `outcome: "carriedByDigest"` and
/// changes nothing. The flag is what lets the list say so, so an older sidecar
/// that omits it must be an error rather than a `false` that re-arms the lie.
#[test]
fn a_quarantine_row_must_state_whether_the_digest_already_carries_it() {
    let mut payload = sidecar_payload();
    payload["quarantined"][0]
        .as_object_mut()
        .expect("quarantine row")
        .remove("carriedByDigest");
    assert!(
        serde_json::from_value::<EdgeStatusPayload>(payload).is_err(),
        "a missing carriedByDigest must fail, not default to 'retry is fine'"
    );

    // Both values survive the round trip, so the flag is read rather than
    // hard-coded.
    for carried in [true, false] {
        let mut payload = sidecar_payload();
        payload["quarantined"][0]["carriedByDigest"] = serde_json::json!(carried);
        let parsed: EdgeStatusPayload = serde_json::from_value(payload).expect("parse");
        assert_eq!(parsed.quarantined[0].carried_by_digest, carried);
    }
}

/// `demotionReason` is nullable but not optional. Serde turns an absent
/// `Option` field into `None` by default, which would make "this sidecar is too
/// old to tell you why" indistinguishable from "this row was never demoted" —
/// and the reason is the actionable half ("older than the relay drift window"
/// is routine, "permanently rejected upstream" is not).
#[test]
fn a_missing_demotion_reason_is_version_skew_not_an_undemoted_row() {
    let mut payload = sidecar_payload();
    payload["quarantined"][0]
        .as_object_mut()
        .expect("quarantine row")
        .remove("demotionReason");
    assert!(
        serde_json::from_value::<EdgeStatusPayload>(payload).is_err(),
        "an absent demotionReason must fail, not masquerade as null"
    );

    // An explicit null is the sidecar saying "never demoted", and is fine.
    let mut payload = sidecar_payload();
    payload["quarantined"][0]["demotionReason"] = serde_json::Value::Null;
    let parsed: EdgeStatusPayload = serde_json::from_value(payload).expect("parse");
    assert_eq!(parsed.quarantined[0].demotion_reason, None);
}

/// The three waiting-author counts answer three different questions, and only
/// `pending` is solved by the author coming back. A build that dropped either
/// of the other two would silently re-create the bug the split exists to fix:
/// "waiting for alice, 1 pending" forever, while alice's drain client runs and
/// correctly claims nothing.
#[test]
fn waiting_authors_keep_the_three_stall_reasons_apart() {
    for dropped in ["pending", "ancestorBlocked", "pendingViaDigest"] {
        let mut payload = sidecar_payload();
        payload["waitingAuthors"][0]
            .as_object_mut()
            .expect("waiting author")
            .remove(dropped);
        assert!(
            serde_json::from_value::<EdgeStatusPayload>(payload).is_err(),
            "a waiting author without '{dropped}' must fail loudly, not default to zero"
        );
    }

    // Crossed wiring fails: each key lands in its own field.
    let mut payload = sidecar_payload();
    payload["waitingAuthors"][0]["pending"] = serde_json::json!(1);
    payload["waitingAuthors"][0]["ancestorBlocked"] = serde_json::json!(2);
    payload["waitingAuthors"][0]["pendingViaDigest"] = serde_json::json!(3);
    let parsed: EdgeStatusPayload = serde_json::from_value(payload).expect("parse");
    assert_eq!(parsed.waiting_authors[0].pending, 1);
    assert_eq!(parsed.waiting_authors[0].ancestor_blocked, 2);
    assert_eq!(parsed.waiting_authors[0].pending_via_digest, 3);
}

/// All three requeue outcomes are real answers the operator must be able to
/// tell apart. `false` alone cannot: "no such row of yours" and "the edge is
/// already carrying this row upstream" are the same boolean and completely
/// different advice.
#[test]
fn reads_all_three_requeue_outcomes() {
    for (outcome, requeued) in [
        ("requeued", true),
        ("notFound", false),
        ("carriedByDigest", false),
    ] {
        assert_eq!(
            parse_requeue_reply(serde_json::json!({
                "requeued": requeued,
                "outcome": outcome,
            })),
            Ok(EdgeRequeueResult {
                requeued,
                outcome: outcome.to_string(),
            }),
            "{outcome}"
        );
    }
}

/// A missing or non-boolean field means the sidecar never answered. Reporting
/// that as `false` would tell the operator their retry was refused when in
/// fact nobody knows what happened. A missing `outcome` is the same failure one
/// question deeper: the retry is known to have not happened, but the reason
/// — the part that says whether pressing Retry again could ever help — is
/// absent, and "declined" is not a safe guess for it.
#[test]
fn refuses_to_read_a_missing_requeue_field() {
    assert!(parse_requeue_reply(serde_json::json!({})).is_err());
    assert!(parse_requeue_reply(serde_json::json!({ "requeued": "yes" })).is_err());
    assert!(parse_requeue_reply(serde_json::json!({ "requeued": 1 })).is_err());
    assert!(parse_requeue_reply(serde_json::json!(null)).is_err());
    // The boolean alone is the pre-rename shape, and it is no longer enough.
    assert!(parse_requeue_reply(serde_json::json!({ "requeued": false })).is_err());
    assert!(parse_requeue_reply(serde_json::json!({ "requeued": true })).is_err());
    assert!(
        parse_requeue_reply(serde_json::json!({ "requeued": false, "outcome": 7 })).is_err(),
        "a non-string outcome is unreadable, not an unknown outcome"
    );
}

/// A newer sidecar's fourth outcome must reach the UI as an unrecognised
/// outcome, not fail the retry. The same tolerance as an unknown delivery
/// state: this build does not get to veto a vocabulary it predates.
#[test]
fn an_unknown_requeue_outcome_is_carried_through_rather_than_refused() {
    assert_eq!(
        parse_requeue_reply(serde_json::json!({
            "requeued": false,
            "outcome": "deferredUntilRelayWindowReopens",
        })),
        Ok(EdgeRequeueResult {
            requeued: false,
            outcome: "deferredUntilRelayWindowReopens".to_string(),
        })
    );
}

/// The boolean and the outcome are one value in the sidecar
/// (`RequeueOutcome::requeued()`), so a reply where they disagree has been
/// rewritten in transit. Believing either half would put a definite claim on
/// screen — "sent back to the queue" or "refused" — with no basis.
#[test]
fn a_self_contradicting_requeue_reply_is_refused() {
    assert!(
        parse_requeue_reply(serde_json::json!({ "requeued": true, "outcome": "notFound" }))
            .is_err(),
        "'nothing moved' must never be reported as a successful retry"
    );
    assert!(
        parse_requeue_reply(
            serde_json::json!({ "requeued": true, "outcome": "carriedByDigest" })
        )
        .is_err()
    );
    assert!(
        parse_requeue_reply(serde_json::json!({ "requeued": false, "outcome": "requeued" }))
            .is_err(),
        "a retry that worked must never be reported as refused"
    );
}

/// The delivery-state reply is a list of objects, and every one carries the
/// reason it left the exact path alongside the label. `pendingViaDigest` on its
/// own says where the event went but not why, and why is the half the operator
/// can act on.
#[test]
fn parses_delivery_state_rows_with_their_demotion_reason() {
    let value = serde_json::json!([
        {
            "eventId": "aa".repeat(32),
            "state": "pending",
            "carriedByDigest": false,
            "demotionReason": null
        },
        {
            "eventId": "bb".repeat(32),
            "state": "pendingViaDigest",
            "carriedByDigest": true,
            "demotionReason": "older than the relay drift window"
        },
        {
            "eventId": "cc".repeat(32),
            "state": "syncedExact",
            "carriedByDigest": false,
            "demotionReason": null
        },
        {
            "eventId": "dd".repeat(32),
            "state": "syncedViaDigest",
            "carriedByDigest": true,
            "demotionReason": null
        },
        {
            "eventId": "ee".repeat(32),
            "state": "quarantined",
            "carriedByDigest": false,
            "demotionReason": null
        },
    ]);
    let parsed: Vec<EdgeEventDeliveryState> = serde_json::from_value(value).expect("parse");
    assert_eq!(parsed.len(), 5);
    assert_eq!(parsed[0].event_id, "aa".repeat(32));
    assert_eq!(parsed[0].state, "pending");
    assert_eq!(parsed[0].demotion_reason, None);
    assert_eq!(parsed[1].state, "pendingViaDigest");
    assert_eq!(
        parsed[1].demotion_reason.as_deref(),
        Some("older than the relay drift window"),
        "a demoted row must carry the reason it was demoted"
    );
    assert_eq!(parsed[2].state, "syncedExact");
    assert_eq!(parsed[3].state, "syncedViaDigest");
    assert_ne!(
        parsed[2].state, parsed[3].state,
        "an exact replay and a digest replay are different states"
    );
    assert_ne!(
        parsed[0].state, parsed[1].state,
        "an author-drained row and a digest-carried row are different states"
    );
}

/// BUG-023. Two rows, one label, opposite advice: a quarantined row the edge is
/// already carrying upstream is not a stuck row, and the badge can only say so
/// if `carriedByDigest` survives the parse. The quarantine list has always
/// carried this flag; the badge did not, so one row got two answers.
#[test]
fn a_quarantined_delivery_row_says_whether_the_digest_is_carrying_it() {
    let value = serde_json::json!([
        {
            "eventId": "aa".repeat(32),
            "state": "quarantined",
            "carriedByDigest": false,
            "demotionReason": null
        },
        {
            "eventId": "bb".repeat(32),
            "state": "quarantined",
            "carriedByDigest": true,
            "demotionReason": "permanently rejected upstream"
        },
    ]);
    let parsed: Vec<EdgeEventDeliveryState> = serde_json::from_value(value).expect("parse");
    assert_eq!(
        parsed[0].state, parsed[1].state,
        "the label cannot separate them"
    );
    assert!(
        !parsed[0].carried_by_digest,
        "a genuinely stuck row must not be reported as handled"
    );
    assert!(
        parsed[1].carried_by_digest,
        "the flag is what lets the badge agree with the quarantine list"
    );
}

/// The flag is required on every delivery row, exactly as it is on every
/// quarantine row. Defaulting an absent one to `false` would put "this is
/// stuck, act now" back on a row that is on its way upstream.
#[test]
fn a_delivery_row_without_the_carried_flag_is_refused() {
    let value = serde_json::json!([
        { "eventId": "aa".repeat(32), "state": "quarantined", "demotionReason": null }
    ]);
    assert!(
        serde_json::from_value::<Vec<EdgeEventDeliveryState>>(value).is_err(),
        "a missing carriedByDigest must fail, not default to 'this row is stuck'"
    );

    // Both values are read rather than assumed.
    for carried in [true, false] {
        let value = serde_json::json!([{
            "eventId": "aa".repeat(32),
            "state": "quarantined",
            "carriedByDigest": carried,
            "demotionReason": null
        }]);
        let parsed: Vec<EdgeEventDeliveryState> = serde_json::from_value(value).expect("parse");
        assert_eq!(parsed[0].carried_by_digest, carried);
    }
}

/// The pair shape is gone. A build still reading `[eventId, state]` would lose
/// the demotion reason silently, so the old shape must be a hard error.
#[test]
fn the_old_event_id_state_pair_shape_no_longer_parses() {
    let pairs = serde_json::json!([["aa".repeat(32), "pending"]]);
    assert!(
        serde_json::from_value::<Vec<EdgeEventDeliveryState>>(pairs).is_err(),
        "the pair shape carries no demotionReason and must not be accepted"
    );
}

/// A delivery row is structurally required to say whether it was demoted, for
/// the same reason a quarantine row is: `None` must mean "never demoted", never
/// "this sidecar is too old to say".
#[test]
fn a_delivery_row_without_a_demotion_reason_key_is_refused() {
    // Every OTHER required key is present, so this fails for the reason the
    // test is named for and not incidentally.
    let value = serde_json::json!([{
        "eventId": "aa".repeat(32),
        "state": "pending",
        "carriedByDigest": false
    }]);
    assert!(serde_json::from_value::<Vec<EdgeEventDeliveryState>>(value).is_err());
}

/// A state this build predates must survive the parse and reach the frontend,
/// which degrades it to one neutral badge. Rejecting it here would throw away
/// the whole batch — every other badge in the window — over one row.
#[test]
fn an_unknown_delivery_state_string_still_parses() {
    let value = serde_json::json!([
        {
            "eventId": "aa".repeat(32),
            "state": "pending",
            "carriedByDigest": false,
            "demotionReason": null
        },
        {
            "eventId": "bb".repeat(32),
            "state": "syncedSomehowInTheFuture",
            "carriedByDigest": false,
            "demotionReason": null
        },
    ]);
    let parsed: Vec<EdgeEventDeliveryState> = serde_json::from_value(value).expect("parse");
    assert_eq!(parsed.len(), 2, "one unreadable row must not cost the batch");
    assert_eq!(parsed[1].state, "syncedSomehowInTheFuture");
}
