# SPEC-2026-08-05 — buzz-edge Phase 1: Local Continuity

Status: APPROVED — build funded 7–9 engineer-days (option 3, James, 2026-08-05).
Spec owner: Cody. Builder: Forge. Reviewer: Ava (this document is the single review artifact).
Base: `44337aa4f54ee17a7eb85c708ccc8fccc3bae5bb` (fork checkout `restart-patch-on-desktop-v0.5.5`).

This document is self-contained and normative. It consolidates Forge's plan v1
(relay event `33e28079c82d…`), the deployment addendum's option 3 (relay event
`0c4504de9b8f…`), and the phase-1 scope decisions, with the option-3 delta
incorporated directly. No other artifact wins on conflict; if this document and
any prior artifact disagree, this document governs and the disagreement is a
spec bug to report.

## Intent

Agent-to-agent and human-to-agent channel messaging on this one PC must keep
working at loopback speed when the Block-hosted canonical relay
(`wss://petty-agent-workspace.communities.buzz.xyz`) is slow, overloaded, or
unreachable — and must synchronize back to canonical history after reconnect
without server-side changes we cannot deploy.

Phase 1 builds a single-community, loopback-only `buzz-edge` sidecar. Desktop,
ACP harnesses, and message-path CLI operations connect to it first. It persists
signed events in bundled SQLite, answers the Nostr `REQ`/`COUNT` subset used by
channel messaging, and fans accepted messages to every local client
immediately. The canonical relay remains canonical. There is **no privileged
upstream replay command** in phase 1 (that is the separate option-1 upstream-PR
lane, explicitly not a dependency): on reconnect, each authoring identity
republishes its own queued events itself, and events too old for the relay's
±15-minute ingest drift gate (`crates/buzz-relay/src/handlers/ingest.rs:1859-1864`)
are represented upstream by a coordinator-signed catch-up digest instead of
their original IDs.

Success criteria: (1) upstream slowness or outage no longer delays local
conversation; (2) deafness-after-reconnect stalls shrink because local clients
subscribe to a loopback endpoint that is always up; (3) after reconnect,
canonical history converges exactly once, with the two delivery states —
**delivered locally** vs **synced to canonical history** — labeled separately
everywhere they surface.

## Files to touch

New code:

- `crates/buzz-edge/` (new crate + binary): loopback WebSocket relay subset,
  SQLite store (events, receipts, outbox, membership cache, upstream cursor),
  local fan-out, reconnect/republish coordinator, digest builder, quarantine.
  Reuses `buzz-core` signature/filter logic (`crates/buzz-core/src/filter.rs`);
  SQLite dependency already bundled (`desktop/src-tauri/Cargo.toml:138`).
- Watchdog/lifecycle: sidecar starts before Desktop/agents and restarts
  independently (packaging + supervisor script or service definition; final
  form decided in M5 and recorded in the milestone post).

Modified code (routing split — edge URL for message paths only):

- `desktop/src-tauri/src/relay.rs:44-52`: today one workspace override derives
  both WebSocket and HTTP origins. Add optional `BUZZ_EDGE_RELAY_URL`; message
  subscribe/submit/query use it when set, all other paths keep
  `BUZZ_RELAY_URL`.
- `crates/buzz-cli/src/client.rs:863-874, 1144-1158`: CLI currently uses one
  relay URL for both `/events` and `/upload`. Route message operations via the
  edge URL; `/upload` and every non-message path stay canonical.
- ACP harness delivery configuration: agent event subscription/delivery via the
  edge URL.
- Tests: new unit/integration suites under the touched crates plus repeatable
  release-gate scripts (M5).

Explicitly NOT touched: `desktop/src-tauri/src/commands/project_git_exec.rs`
clone-origin check (git stays canonical-only), all `crates/buzz-relay` server
code, and every canonical-only command path listed in "Out of scope".

## Behavior contract

Event allowlist and local ingress:

1. The sidecar accepts persistent kind-9 channel messages only, with their
   existing mention and thread tags, for explicitly selected channels of one
   community.
2. At local ingress it verifies the event signature; requires the event pubkey
   to match the authenticated NIP-42/NIP-98 principal; checks cached channel
   membership and NIP-OA owner attestation; stores the exact signed event bytes
   plus a device-signed local receipt; returns `OK`; and fans out to all local
   subscribers immediately.
3. It never re-signs or mutates a user's event, so event IDs and reply
   references remain stable end-to-end.

Authentication and keys:

4. The sidecar terminates normal NIP-42 WebSocket auth and NIP-98 HTTP auth
   against its loopback URL (NIP-42/98 proofs expire after 60 seconds and
   NIP-98 is URL-bound — `crates/buzz-auth/src/nip42.rs:35,81`,
   `crates/buzz-auth/src/nip98.rs:32,81-99` — which is why clients
   authenticate to the sidecar directly rather than having proofs forwarded).
5. It stores no AUTH events and no human or agent private keys. Its only secret
   is a dedicated edge-device key, held via Windows DPAPI and revocable
   upstream. The edge-device key is used for upstream mirror subscriptions,
   local receipts, and digest signing — never to author or replay another
   identity's messages.

Upstream mirroring (while online):

6. Using the edge-device key, the sidecar subscribes upstream to the selected
   channels and mirrors received events and member state into SQLite, so local
   reads are served from loopback even when upstream is merely slow.

Reconnect synchronization (option-3 contract — no upstream replay command):

7. The SQLite outbox and cache survive sidecar and Desktop restarts.
8. On reconnect the sidecar first refreshes membership and the upstream cursor.
   Then each authoring Desktop/ACP identity — which already holds only its own
   key — republishes its own queued events over a fresh upstream-authenticated
   session, parent-before-reply, FIFO within each author/channel.
9. Events still inside the relay's ±15-minute drift window are submitted as-is
   and retain their exact event IDs and thread tags.
10. Older events remain permanently marked local-only. They are collapsed, in
    order, into one fresh coordinator-signed catch-up digest per channel
    containing author, local timestamp, and quoted, mention-neutralized
    content. The digest becomes canonical; the original event IDs and thread
    structure of those older events do not.
11. Upstream acceptance marks an event delivered; duplicate acknowledgment is
    success (event-ID dedup guarantees exactly-once canonical storage).
    Permanent rejects are quarantined and surfaced in Desktop, never retried
    forever. Membership revocation wins: events authored under stale local
    authorization may remain local but are rejected from canonical history.
12. Every surface that shows delivery state labels the two success states
    separately: **delivered locally** vs **synced to canonical history**.

Routing:

13. `BUZZ_EDGE_RELAY_URL` is optional. When unset, behavior is exactly today's:
    all traffic to `BUZZ_RELAY_URL`. When set, only message subscribe, submit,
    query, and ACP delivery use the edge URL; media/Blossom uploads, git, admin,
    moderation, and all other command paths keep the canonical URL.

Milestones (each ends with a channel post reporting exit codes and test
collection counts):

- M1 — this spec reviewed by Ava (gates M3+; M2 may run in parallel).
- M2 — sidecar core (~3d): loopback relay, SQLite schema, signature/membership
  checks, `REQ`/`COUNT` subset, local fan-out.
- M3 — routing split (~2d): Desktop, CLI message paths, ACP harness.
- M4 — reconnect sync (~2d): outbox, self-republish, digest fallback, dual
  labels, quarantine surfacing.
- M5 — watchdog + release gates (~1–2d), wired as repeatable scripts.

## Test list

Release gates (all must pass; reports carry collection counts and exit codes,
not summaries):

1. **Full-cut gate**: sever upstream connectivity for more than 15 minutes.
   Desktop and two local agents exchange thread replies immediately throughout;
   the sidecar is restarted mid-outage with no loss; on reconnect, sub-15-minute
   events land upstream exactly once with identical event IDs and thread tags;
   older events appear in exactly one digest, in order.
2. **Slow-upstream gate** (James's actual failure mode): upstream artificially
   delayed, not cut; local thread replies between Desktop and two agents remain
   at loopback latency.
3. **Restart-survival gate**: restart the sidecar, then Desktop; no outbox
   loss; no duplicate canonical events (event-ID dedup demonstrated).
4. **Key-hygiene gate**: the sidecar process holds no human or agent private
   keys — only the edge-device key.

Failure-mode tests:

5. Duplicate upstream acknowledgment treated as success (no re-send loop).
6. Corrupt or missing local receipt → event quarantined and surfaced, sync of
   other events unaffected.
7. Membership revoked during outage → affected events rejected from canonical
   history, visibly quarantined locally.
8. Permanent upstream rejection → visible in Desktop, never retried forever.
9. Offline upload attempt and offline git operation → clear, immediate failure
   with no corruption and no queuing.
10. Unit/integration coverage per milestone: ingress validation (signature,
    principal match, membership), `REQ`/`COUNT` subset conformance, outbox
    ordering (parent-before-reply, per-author FIFO), digest construction
    (ordering, mention neutralization, coordinator signature), routing split
    (canonical-only paths proven untouched when `BUZZ_EDGE_RELAY_URL` is set).

## Safety implications

- **No impersonation surface.** The sidecar never re-signs user events and
  holds no human/agent keys; phase 1 adds no privileged replay capability
  anywhere. The edge-device key can author only its own receipts, mirror
  subscriptions, and digests, and is revocable upstream.
- **Loopback only.** The sidecar binds to localhost exclusively. No LAN or
  remote exposure; no new open ports beyond loopback.
- **Canonical relay untouched.** No server-side code ships or deploys in this
  phase; the blast radius of any defect is this one PC's local cache.
- **Auth boundaries preserved.** NIP-42/NIP-98 verification runs unchanged
  against the loopback URL; AUTH events are not stored; membership is enforced
  at local ingress from mirrored state and re-enforced by the canonical relay
  at republish, where revocation wins.
- **Notification hygiene.** Digest content is mention-neutralized so a
  catch-up digest cannot re-trigger every mentioned human and agent.
- **Data integrity.** Exact signed bytes are stored; event-ID dedup prevents
  duplicate canonical history; quarantine makes rejects visible instead of
  silently dropped or infinitely retried.

## Out of scope / do NOT touch

- No upstream/server-side code, no deploy to Block's relay, no edge replay
  command. (The option-1 upstream PR is a separate non-gating lane, drafted
  after M2; phase 1 ships without it.)
- No offline uploads or new attachments; no blob outbox. New Blossom uploads
  fail clearly until online. `crates/buzz-cli` `/upload` stays canonical.
- No offline git hosting or synchronization. Existing worktrees remain usable;
  clone/fetch/push stay direct-to-upstream. Do not touch the clone-origin check
  in `desktop/src-tauri/src/commands/project_git_exec.rs`.
- No offline repo/issue/PR events, workflows, moderation/admin or membership
  changes, DMs, reactions/edits/deletes, presence/typing, huddles, search, or
  social publishing. All stay canonical-only and fail clearly offline.
- One PC, one community, explicitly selected channels. No peer-to-peer LAN
  mode, no cross-device conflict resolution.
- Process rules: all work on feature branches in isolated worktrees off base
  `44337aa4f54ee17a7eb85c708ccc8fccc3bae5bb`; never the default branch; no
  force-push, history rewrite, or remote-branch deletion; commit identity and
  trailers per `AGENTS.md`.

## Rollback path

- **Config-level (instant):** unset `BUZZ_EDGE_RELAY_URL` (and stop the
  watchdog). Every client reverts to today's canonical-only routing; the
  sidecar is inert. This is the first-line rollback at any milestone.
- **Data:** SQLite files are additive and local. Before deleting them, export
  any queued-but-unsynced events to a plain-text digest file so no authored
  content is silently lost; then archive or delete the database.
- **Code:** revert or abandon the feature branch; no migrations, no schema or
  data changes exist outside the sidecar's own local database; canonical relay
  state requires no cleanup because phase 1 never gained privileged write
  access to it.
- **Partial-milestone failure:** each milestone is independently revertible —
  M3's routing split is behind the env var, M4's sync logic only runs when the
  sidecar is enabled, M5's watchdog is a wrapper that can be uninstalled
  without touching M2–M4 code.
