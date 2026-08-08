//! Release gate 4 — key hygiene (spec "Test list", gate 4).
//!
//! "The sidecar process holds no human or agent private keys — only the
//! provisioned edge identity key." That sentence is easy to write in a comment
//! and hard to keep true, so this gate turns it into three assertions about
//! what the running sidecar can actually reach, each with its own positive
//! control.
//!
//! The scenario is deliberately the one where a leak would be most tempting:
//! two authors authenticate over NIP-42, publish, are fanned out to, drain
//! their own outbox rows, have one row refused and requeued, and have one row
//! swept into an edge-signed digest. If the sidecar were ever going to cache an
//! author's key to sign something on their behalf, a drain is where it would
//! happen.
//!
//! 1. **Nothing durable.** Every byte of every file in the sidecar's data
//!    directory — the SQLite database, its write-ahead log, its shared-memory
//!    index — is searched for each identity's secret in all four shapes a key
//!    is realistically stored in: raw 32 bytes, lowercase hex, uppercase hex,
//!    and bech32 `nsec`. *Positive controls:* a planted decoy secret is found
//!    by the same search in all four shapes, and an author's **public** key is
//!    found in the real database, so a clean result cannot come from an empty
//!    file or a broken matcher.
//!
//! 2. **Nothing on the wire.** The same search runs over every frame the
//!    clients sent to the sidecar and every frame the sidecar sent back. A
//!    private key the sidecar never receives is one it cannot cache, and a
//!    private key it never emits is one it cannot leak; the recording is
//!    unconditional inside the harness so a newly added `send` cannot slip past
//!    it.
//!
//! 3. **Nothing signed.** Every artifact the sidecar authored — one delivery
//!    receipt per locally accepted event, the authorization snapshot, every
//!    digest part — is checked to carry the edge identity's public key and a
//!    valid signature. And the set of author-signed events in the store is
//!    compared, exactly, against what the authors themselves submitted: if the
//!    sidecar ever acquired an author's key and used it, the extra artifact
//!    would show up here.
//!
//! What would break this gate: a future change that caches a drained author's
//! key. Assertion 2 fails the moment the key has to be transmitted to be
//! cached; assertion 1 fails the moment it is written down; assertion 3 fails
//! the moment it is used.

use std::collections::BTreeSet;
use std::sync::Arc;

use nostr::{Event, EventBuilder, Filter, JsonUtil, Keys, Kind, PublicKey, Tag, ToBech32};
use serde_json::{json, Value};
use uuid::Uuid;

use buzz_edge::storage::{build_digest_chunks, CommunityBinding, EdgeStore};

mod gate_harness;

use gate_harness as harness;
use harness::ClientSocket;

/// One shape a private key could realistically be stored or transmitted in.
struct Needle {
    shape: &'static str,
    bytes: Vec<u8>,
}

/// Every shape of one identity's secret key that this gate searches for.
fn secret_needles(keys: &Keys) -> Vec<Needle> {
    let secret = keys.secret_key();
    let lowercase = secret.to_secret_hex();
    vec![
        Needle {
            shape: "raw 32 bytes",
            bytes: secret.to_secret_bytes().to_vec(),
        },
        Needle {
            shape: "lowercase hex",
            bytes: lowercase.as_bytes().to_vec(),
        },
        Needle {
            shape: "uppercase hex",
            bytes: lowercase.to_uppercase().into_bytes(),
        },
        Needle {
            shape: "bech32 nsec",
            bytes: secret
                .to_bech32()
                .expect("bech32 encoding of a secret key")
                .into_bytes(),
        },
    ]
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Read every file in the sidecar's data directory.
fn data_directory_bytes(directory: &std::path::Path) -> Vec<(String, Vec<u8>)> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(directory).expect("read the edge data directory") {
        let entry = entry.expect("directory entry");
        if !entry.file_type().expect("file type").is_file() {
            continue;
        }
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("<unnamed>")
            .to_string();
        files.push((
            name,
            std::fs::read(&path).expect("read the edge database file"),
        ));
    }
    files.sort_by(|left, right| left.0.cmp(&right.0));
    files
}

async fn next_control_frame(socket: &mut ClientSocket) -> Value {
    loop {
        let frame = harness::next_json(socket).await;
        if frame[0].as_str() == Some("EVENT") {
            continue;
        }
        return frame;
    }
}

async fn drain(socket: &mut ClientSocket, claim_token: &str, limit: usize) -> Vec<Event> {
    harness::send(
        socket,
        json!(["BUZZ-EDGE", "DRAIN", {"claim_token": claim_token, "limit": limit}]),
    )
    .await;
    let frame = next_control_frame(socket).await;
    assert_eq!(frame[1], "DRAIN-BATCH", "expected a drain batch: {frame}");
    frame[2]["events"]
        .as_array()
        .expect("drain batch events")
        .iter()
        .map(|value| serde_json::from_value::<Event>(value.clone()).expect("drained event"))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gate4_key_hygiene() {
    let directory = tempfile::tempdir().expect("temporary edge data directory");
    let database = directory.path().join("buzz-edge-gate4.sqlite3");
    let binding = CommunityBinding::new("wss://relay.example.com", Uuid::new_v4())
        .expect("community binding");
    let edge_keys = Keys::generate();
    let alice = Keys::generate();
    let bob = Keys::generate();
    let channel = Uuid::new_v4();
    let now = harness::unix_now();

    let store = Arc::new(
        EdgeStore::open(&database, binding.clone(), harness::policy()).expect("open edge store"),
    );
    let (_relay_identity, snapshot) = harness::authorize(
        &store,
        &edge_keys,
        now,
        &[(
            channel,
            vec![alice.public_key(), bob.public_key(), edge_keys.public_key()],
        )],
    );
    let (url, relay, server) = harness::serve(Arc::clone(&store), edge_keys.clone(), true).await;

    // ── Give the sidecar every chance to cache a key ───────────────────────
    let mut alice_socket = harness::authenticated_client(&url, &binding, &alice).await;
    let mut bob_socket = harness::authenticated_client(&url, &binding, &bob).await;
    harness::subscribe(&mut alice_socket, "alice", channel).await;
    harness::subscribe(&mut bob_socket, "bob", channel).await;

    let mut alice_events = Vec::new();
    for index in 0..3 {
        let event = harness::message(&alice, channel, &format!("alice message {index}"));
        assert_eq!(harness::submit(&mut alice_socket, &event).await[2], true);
        alice_events.push(event);
    }
    let mut bob_events = Vec::new();
    for index in 0..2 {
        let event = harness::message(&bob, channel, &format!("bob message {index}"));
        assert_eq!(harness::submit(&mut bob_socket, &event).await[2], true);
        bob_events.push(event);
    }

    // Alice drains all three of her rows: one accepted, one permanently
    // refused, one left leased.
    let claimed = drain(&mut alice_socket, "alice-drain", 100).await;
    assert_eq!(
        claimed.len(),
        3,
        "alice must be offered exactly her own rows"
    );
    for (event, outcome) in [
        (&alice_events[0], json!({"outcome": "delivered"})),
        (
            &alice_events[1],
            json!({"outcome": "rejected", "reason": "upstream refused: gate 4"}),
        ),
    ] {
        let mut body = json!({
            "claim_token": "alice-drain",
            "event_id": event.id.to_hex(),
        });
        for (key, value) in outcome.as_object().expect("outcome fields") {
            body[key] = value.clone();
        }
        harness::send(&mut alice_socket, json!(["BUZZ-EDGE", "DRAIN-ACK", body])).await;
    }

    // Bob drains exactly one row, leaving the other pending so it can be swept
    // into a digest below.
    let claimed = drain(&mut bob_socket, "bob-drain", 1).await;
    assert_eq!(claimed.len(), 1);
    harness::send(
        &mut bob_socket,
        json!([
            "BUZZ-EDGE", "DRAIN-ACK",
            {"claim_token": "bob-drain", "event_id": claimed[0].id.to_hex(), "outcome": "duplicate"}
        ]),
    )
    .await;

    // The operator retries the refused row from the quarantine UI.
    harness::send(
        &mut alice_socket,
        json!([
            "BUZZ-EDGE", "REQUEUE",
            {"req_id": "gate4-requeue", "event_id": alice_events[1].id.to_hex()}
        ]),
    )
    .await;
    let requeue = next_control_frame(&mut alice_socket).await;
    assert_eq!(
        requeue[1], "REQUEUE-REPLY",
        "expected a requeue reply: {requeue}"
    );
    assert_eq!(
        requeue[2]["requeued"], true,
        "the requeue must actually happen, or the drain path below is untested: {requeue}"
    );

    // Bob's remaining pending row is carried by an edge-signed digest — the one
    // place the sidecar authors content on somebody else's behalf.
    let digest_source = bob_events
        .iter()
        .find(|event| event.id != claimed[0].id)
        .expect("bob's untouched row");
    let chunks = build_digest_chunks(&[(
        digest_source.pubkey,
        digest_source.created_at.as_secs() as i64,
        digest_source.content.clone(),
    )]);
    assert_eq!(chunks.len(), 1, "one short message is one digest chunk");
    let digest_part = EventBuilder::new(Kind::Custom(9), chunks[0].clone())
        .tags([
            Tag::parse(["h", channel.to_string().as_str()]).expect("h tag"),
            Tag::parse(["part", "1"]).expect("part tag"),
            Tag::parse(["total", "1"]).expect("total tag"),
        ])
        .sign_with_keys(&edge_keys)
        .expect("sign the digest part");
    store
        .materialize_digest_batch(
            "gate4-batch",
            channel,
            &[digest_source.id],
            &[digest_part],
            &edge_keys.public_key(),
        )
        .expect("materialize the digest batch");

    // Let the fire-and-forget acknowledgments land before anything is read back.
    let mut settled = false;
    for _ in 0..100 {
        let summary = store.outbox_summary(&[channel]).expect("outbox summary");
        if summary.synced_exact == 2 && summary.pending_via_digest == 1 {
            settled = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        settled,
        "the drain acknowledgments never landed, so this gate would be scanning a database \
         that never saw a drain: {:?}",
        store.outbox_summary(&[channel]).expect("outbox summary")
    );

    // ── 1. Nothing durable ─────────────────────────────────────────────────
    let files = data_directory_bytes(directory.path());
    let scanned_bytes: usize = files.iter().map(|(_, bytes)| bytes.len()).sum();
    assert!(
        !files.is_empty() && scanned_bytes > 8 * 1024,
        "only {scanned_bytes} bytes across {} files were scanned; an almost-empty data \
         directory would pass every search below for the wrong reason",
        files.len()
    );

    // Positive control A: the matcher finds every shape it claims to find.
    let decoy = Keys::generate();
    let mut planted = Vec::new();
    for needle in secret_needles(&decoy) {
        planted.extend_from_slice(b"----");
        planted.extend_from_slice(&needle.bytes);
    }
    for needle in secret_needles(&decoy) {
        assert!(
            contains(&planted, &needle.bytes),
            "the {} search cannot find a key that is definitely present, so its absence \
             elsewhere proves nothing",
            needle.shape
        );
    }

    // Positive control B: the real database really does hold identity material,
    // so a clean secret scan is a statement about secrets and not about an
    // unwritten file.
    let alice_public = alice.public_key().to_hex();
    assert!(
        files
            .iter()
            .any(|(_, bytes)| contains(bytes, alice_public.as_bytes())),
        "alice's public key is not in the edge database, so the database is not holding the \
         identity material this gate assumes it is"
    );

    for (label, keys) in [
        ("the human author", &alice),
        ("the agent author", &bob),
        ("the edge identity", &edge_keys),
    ] {
        for needle in secret_needles(keys) {
            for (name, bytes) in &files {
                assert!(
                    !contains(bytes, &needle.bytes),
                    "{label}'s private key is written into {name} as {}",
                    needle.shape
                );
            }
        }
    }

    // ── 2. Nothing on the wire ─────────────────────────────────────────────
    let sent = harness::sent_frames();
    let received = harness::received_frames();
    assert!(
        sent.len() >= 10 && received.len() >= 10,
        "only {} sent and {} received frames were recorded; this scan needs real traffic to \
         mean anything",
        sent.len(),
        received.len()
    );
    for (label, keys) in [
        ("the human author", &alice),
        ("the agent author", &bob),
        ("the edge identity", &edge_keys),
    ] {
        for needle in secret_needles(keys) {
            for frame in sent.iter() {
                assert!(
                    !contains(frame.as_bytes(), &needle.bytes),
                    "{label}'s private key was transmitted to the sidecar as {}: {frame}",
                    needle.shape
                );
            }
            for frame in received.iter() {
                assert!(
                    !contains(frame.as_bytes(), &needle.bytes),
                    "{label}'s private key was emitted by the sidecar as {}: {frame}",
                    needle.shape
                );
            }
        }
    }

    // ── 3. Nothing signed by anyone but its own author ─────────────────────
    let stored = store
        .query(&[Filter::new()
            .kind(Kind::Custom(9))
            .custom_tags(
                nostr::SingleLetterTag::lowercase(nostr::Alphabet::H),
                [channel.to_string()],
            )
            .limit(1_000)])
        .expect("stored channel events");
    let submitted: BTreeSet<String> = alice_events
        .iter()
        .chain(bob_events.iter())
        .map(|event| event.id.to_hex())
        .collect();
    assert_eq!(
        stored
            .iter()
            .map(|event| event.id.to_hex())
            .collect::<BTreeSet<_>>(),
        submitted,
        "the store holds a channel event nobody in this test submitted, which is what a \
         sidecar signing on an author's behalf would look like"
    );

    let mut edge_authored: Vec<Event> = Vec::new();
    for event in &stored {
        assert!(
            event.pubkey == alice.public_key() || event.pubkey == bob.public_key(),
            "a stored message is authored by neither author: {}",
            event.pubkey.to_hex()
        );
        let receipt = store
            .receipt(&event.id)
            .expect("receipt lookup")
            .unwrap_or_else(|| panic!("no delivery receipt for {}", event.id));
        edge_authored.push(receipt);
    }
    edge_authored.push(snapshot);
    for part in store.digest_parts("gate4-batch").expect("digest parts") {
        edge_authored.push(Event::from_json(part.event_bytes).expect("digest part event"));
    }
    assert_eq!(
        edge_authored.len(),
        5 + 1 + 1,
        "expected five receipts, one authorization snapshot, and one digest part"
    );
    let edge_public: PublicKey = edge_keys.public_key();
    for event in &edge_authored {
        assert_eq!(
            event.pubkey, edge_public,
            "the sidecar authored an artifact under an identity that is not the provisioned \
             edge key: {}",
            event.id
        );
        assert!(
            event.verify().is_ok(),
            "a sidecar-authored artifact does not verify: {}",
            event.id
        );
    }

    println!("[gate4] identities exercised: 2 authors + 1 provisioned edge identity");
    println!("[gate4] key shapes searched per identity: 4 (raw, lower hex, upper hex, nsec)");
    println!("[gate4] data-directory files scanned: {}", files.len());
    println!("[gate4] data-directory bytes scanned: {scanned_bytes}");
    println!("[gate4] client-to-sidecar frames scanned: {}", sent.len());
    println!(
        "[gate4] sidecar-to-client frames scanned: {}",
        received.len()
    );
    println!(
        "[gate4] sidecar-authored artifacts verified: {}",
        edge_authored.len()
    );
    println!(
        "[gate4] author-authored events in the store: {}",
        stored.len()
    );
    println!("[gate4] private keys found: 0");

    server.abort();
    drop(relay);
}
