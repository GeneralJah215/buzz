# SPEC-2026-08-05 — buzz-edge Phase 1: Local Continuity

Status: APPROVED build (option 3, James, 2026-08-05); spec revision 2 answering
Ava FAIL verdict on commit `d5382d11ebf5a56f944919d69a6dc15fa9bf532c` (findings
F1–F6, buzz-infra event `28d54430…`).
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
drains and republishes its own queued events under the author-drain protocol
defined below, and events too old for the relay's ±15-minute ingest drift gate
(`crates/buzz-relay/src/handlers/ingest.rs:1859-1864`) are represented upstream
by a catch-up digest authored by the provisioned edge identity instead of their
original IDs.

Success criteria: (1) upstream slowness or outage no longer delays local
conversation, proven against the numeric transport SLO in the Test list; (2)
deafness-after-reconnect stalls shrink because local clients subscribe to a
loopback endpoint that is always up; (3) after reconnect, canonical history
converges exactly once, with the two delivery states — **delivered locally** vs
**synced to canonical history** — labeled separately everywhere they surface.

## Files to touch

New code:

- `crates/buzz-edge/` (new crate + binary): loopback WebSocket relay subset,
  SQLite store (events, receipts, outbox with drain-state machine, membership
  cache, upstream cursor), local fan-out, community-binding handshake, digest
  builder, quarantine, author-drain API. Reuses `buzz-core` signature/filter
  logic (`crates/buzz-core/src/filter.rs`).
- `Cargo.toml` (workspace root, members list at `Cargo.toml:1-29`) and
  `Cargo.lock`: register the new workspace crate.
- `crates/buzz-acp/src/edge.rs` (new): edge WebSocket client + author-drain
  client for agent identities.
- Desktop drain + status IPC: new `desktop/src-tauri/src/commands/edge_status.rs`
  (registered alongside existing commands) exposing delivery-state,
  quarantine, and drain-trigger commands to the frontend.
- Desktop UI: new feature module `desktop/src/features/edge-status/` — the two
  delivery-state labels, the quarantine list, and the "waiting for author"
  indicator. No existing feature module is repurposed.
- Release-gate harness: repeatable scripts under `crates/buzz-edge/tests/` plus
  a `Justfile` recipe per gate.

Modified code (routing split — edge URL for message paths only):

- `desktop/src-tauri/src/relay.rs:44-52`: today one workspace override derives
  both WebSocket and HTTP origins. Add optional `BUZZ_EDGE_RELAY_URL`; message
  subscribe/submit/query use it only while the community-binding handshake
  (Behavior contract §13) holds; all other paths keep `BUZZ_RELAY_URL`.
- `crates/buzz-acp/src/relay.rs`: `HarnessRelay` currently owns a single
  `relay_url` (`:557`) used for channel discovery (`:669`), HTTP bridge
  (`:735`), durable submit (`:424`), typing (`:879`), and observer traffic
  (`:1298+`). Introduce the two-client topology of Behavior contract §14: the
  canonical client keeps every existing responsibility except persistent
  kind-9 subscribe/submit/query, which move to the edge client from
  `crates/buzz-acp/src/edge.rs` when the edge is bound and healthy.
- `crates/buzz-cli/src/client.rs:863-874, 1144-1158`: CLI currently uses one
  relay URL for both `/events` and `/upload`. Route kind-9 message operations
  via the edge URL; `/upload` and every non-message path stay canonical.
- `desktop/src-tauri/tauri.conf.json:55-62`: add `buzz-edge` to `externalBin`.
- `Justfile:151-158, 235+`: add the sidecar to build/stub/release lists.

Packaging and watchdog (decided now, not deferred to M5): `buzz-edge.exe` is
bundled via Tauri `externalBin` and registered at install time as a Windows
Scheduled Task at user logon, so it starts before Desktop and restarts
independently of it. Desktop additionally health-checks the sidecar at launch
and respawns it if absent. M5 wires the gates; it does not choose the
packaging form.

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
   membership and owner attestation; stores the exact signed event bytes plus
   a device-signed local receipt; returns `OK`; and fans out to all local
   subscribers immediately.
3. It never re-signs or mutates a user's event, so event IDs and reply
   references remain stable end-to-end.

Authentication and keys:

4. The sidecar terminates normal NIP-42 WebSocket auth and NIP-98 HTTP auth
   against its loopback URL (NIP-42/98 proofs expire after 60 seconds and
   NIP-98 is URL-bound — `crates/buzz-auth/src/nip42.rs:35,81`,
   `crates/buzz-auth/src/nip98.rs:32,81-99` — which is why clients
   authenticate to the sidecar directly rather than having proofs forwarded).
5. It stores no AUTH events and no human or agent private keys. Its only
   secret is the provisioned edge identity key (§6), held via Windows DPAPI.
   That key authors only mirror subscriptions, local receipts, and catch-up
   digests — never another identity's messages.

Edge identity provisioning (answers F1):

6. Phase 1 uses a **provisioned member model** for the edge identity. At
   setup, while online, the community owner (a) issues the owner attestation
   (NIP-OA auth tag) for the edge-device pubkey so it can authenticate
   upstream, and (b) explicitly adds the edge-device pubkey as a member of
   every selected channel, using the existing membership flow. This is a
   one-time setup action; runtime membership mutation remains out of scope.
   Rationale: upstream reads are scoped to channels accessible to the
   authenticated pubkey (`crates/buzz-relay/src/handlers/req.rs:94,133-167`;
   membership resolution `crates/buzz-db/src/channel.rs:746-771`), and ingest
   requires `event.pubkey == authenticated identity`
   (`crates/buzz-relay/src/handlers/ingest.rs:1878-1881`) — so mirroring
   private selected channels and ingesting edge-signed digests both require
   real membership; attestation alone is insufficient.
7. A channel is **eligible for edge routing only if the edge identity's
   membership in it is confirmed**; the sidecar verifies this at startup and
   on every reconnect, and drops non-eligible channels to canonical-only
   routing, visibly. Revocation flow: the owner removes the edge pubkey's
   membership (or the attestation); on next verification the sidecar fails
   closed for the affected channels — no local ingress, no mirroring, cached
   rows retained read-only, clients transparently fall back to canonical.
8. Using the edge identity, the sidecar subscribes upstream to the selected
   channels and mirrors received events and member state into SQLite, so
   local reads are served from loopback even when upstream is merely slow.

Reconnect synchronization — durable outbox:

9. The SQLite outbox and cache survive sidecar and Desktop restarts.
10. Outbox rows carry an explicit drain-state machine (answers F3):
    `pending → claimed(lease) → submitted → delivered | quarantined`, with
    `claimed → pending` on lease expiry.
11. **Author-drain protocol** (answers F3): the sidecar cannot and does not
    submit another identity's events upstream (blocked by
    `ingest.rs:1878-1881`). Instead:
    - An author client (Desktop or ACP identity) opens its normal
      NIP-42-authenticated loopback session and calls the drain API. It may
      claim only rows whose author pubkey equals the session principal — the
      sidecar enforces this ownership check.
    - A claim takes a renewable 60-second lease over an ordered batch
      (parent-before-reply, FIFO within author/channel). Lease expiry returns
      unacknowledged rows to `pending`.
    - The author submits each event upstream itself over a fresh
      NIP-42/NIP-98 session it owns, then acknowledges per-event results to
      the sidecar: accepted or duplicate → `delivered` (duplicate is success;
      upstream event-ID dedup gives exactly-once canonical storage);
      permanent reject → `quarantined`, surfaced in Desktop, never retried
      forever; transient failure or silence → lease expiry → `pending`.
    - Drain triggers: author process startup, sidecar reconnect notification,
      and manual retry from the quarantine UI.
    - Absent author: rows remain `pending` indefinitely and Desktop shows the
      **waiting for author** state for that identity. Author restart resumes
      via fresh claim; duplicate-drain and mid-batch crash are safe because
      state transitions are per-event and idempotent.
12. Events still inside the relay's ±15-minute drift window are submitted
    as-is and retain their exact event IDs and thread tags. Older events
    remain permanently marked local-only and are collapsed, in order, into one
    fresh catch-up digest per channel — authored and signed by the provisioned
    edge identity (a member, so ordinary ingest accepts it) — containing
    author, local timestamp, and quoted, mention-neutralized content. The
    digest becomes canonical; the original event IDs and thread structure of
    those older events do not. Membership revocation wins: events authored
    under stale local authorization may remain local but are rejected from
    canonical history.
13. Every surface that shows delivery state labels the two success states
    separately: **delivered locally** vs **synced to canonical history**.

Routing and community binding (answers F2, F4):

14. **Community binding:** at startup the sidecar binds to exactly one
    `(canonical relay origin, community ID)` pair, stores it in the SQLite
    header, and derives its database path from that pair. Every client
    connection begins with a handshake declaring the client's expected pair;
    the sidecar rejects mismatches and the client fails closed to canonical
    routing. Desktop switches active communities without a process restart
    (`desktop/src/app/App.tsx:330,549`;
    `desktop/src/features/communities/useCommunityInit.ts:178-199`), so
    Desktop re-evaluates the handshake on every community switch and uses the
    edge only while the active community matches the bound pair. One sidecar
    instance, one community, one database; no cross-community read, write, or
    cache reuse.
15. **Two-client topology** (`BUZZ_EDGE_RELAY_URL` optional; unset = exactly
    today's behavior): each client that adopts the edge keeps its canonical
    client for channel/membership/metadata discovery, HTTP bridge and memory
    operations, observer/control traffic, typing/presence, auth bootstrap,
    uploads, git, admin, and every non-kind-9 event kind. Only persistent
    kind-9 subscribe/submit/query (and the drain API) use the edge client —
    and only while the §14 handshake holds and the channel is §7-eligible.
    For ACP this is the exact call-site split listed in "Files to touch" for
    `crates/buzz-acp/src/relay.rs`.

Milestones (each ends with a channel post reporting exit codes and test
collection counts):

- M1 — this spec reviewed by Ava (gates M3+; M2 may run in parallel).
- M2 — sidecar core (~3d): loopback relay, SQLite schema incl. drain-state
  outbox and community binding, signature/membership checks, `REQ`/`COUNT`
  subset, local fan-out, edge-identity provisioning verification.
- M3 — routing split (~2d): Desktop, CLI message paths, ACP two-client
  topology, community-switch fail-closed behavior.
- M4 — reconnect sync (~2d): author-drain protocol, digest fallback, dual
  labels, quarantine + waiting-for-author surfacing.
- M5 — watchdog wiring + release gates (~1–2d) as repeatable scripts (the
  packaging form is already fixed in "Files to touch").

## Test list

Release gates (all must pass; reports carry collection counts and exit codes,
not summaries):

1. **Full-cut gate**: sever upstream connectivity for more than 15 minutes.
   Desktop and two local agents exchange thread replies immediately
   throughout; the sidecar is restarted mid-outage with no loss; on
   reconnect, sub-15-minute events land upstream exactly once with identical
   event IDs and thread tags; older events appear in exactly one digest, in
   order.
2. **Slow-upstream transport SLO** (James's actual failure mode; answers F5):
   inject ≥10 seconds of artificial upstream delay (upstream reachable, not
   cut). Over ≥100 warmed kind-9 messages exchanged between Desktop and two
   agent clients, **sidecar-ingress-to-peer-delivery latency must be p95
   ≤250 ms and max ≤1 s, with zero sends blocked awaiting any upstream
   acknowledgment**. Measurement boundary: from the sidecar returning `OK`
   for the submitted event to each subscribed peer client receiving the event
   frame. Harness: a single test-harness process on this PC hosting all
   client connections, timing with the monotonic clock
   (`std::time::Instant`); one host, one clock source, no cross-machine
   skew. Model think time is explicitly outside the measurement boundary.
3. **Restart-survival gate**: restart the sidecar, then Desktop; no outbox
   loss; no duplicate canonical events (event-ID dedup demonstrated).
4. **Key-hygiene gate**: the sidecar process holds no human or agent private
   keys — only the provisioned edge identity key.
5. **Private-channel end-to-end gate** (answers F1): in a private selected
   channel, prove the provisioned edge identity mirrors events, builds cached
   authorization, and lands a digest via ordinary ingest; then revoke its
   membership and prove fail-closed behavior (no ingress, no mirror, clean
   canonical fallback, digest rejected upstream).
6. **Community-switch gate** (answers F4): two communities containing equal
   channel UUIDs; switch Desktop's active community both directions; prove
   handshake rejection, fail-closed canonical fallback, and zero
   cross-community read, write, or cache reuse.

Failure-mode and protocol tests:

7. Author-drain protocol (answers F3): absent-author (rows stay pending,
   waiting-for-author surfaced), author-restart mid-batch, duplicate-drain,
   mid-batch crash with lease expiry, ownership check (a session cannot claim
   another author's rows), permanent-reject → quarantine transition.
8. Duplicate upstream acknowledgment treated as success (no re-send loop).
9. Corrupt or missing local receipt → event quarantined and surfaced, sync of
   other events unaffected.
10. Membership revoked during outage → affected events rejected from
    canonical history, visibly quarantined locally.
11. Offline upload attempt and offline git operation → clear, immediate
    failure with no corruption and no queuing.
12. **Negative routing tests per canonical-only class** (answers F2): with
    the edge active, channel/membership discovery, HTTP bridge and memory
    ops, observer/control, typing/presence, uploads, git, admin, and
    non-kind-9 events each provably reach only the canonical URL.
13. **Build/package acceptance** (answers F6): workspace builds with the new
    crate registered in root `Cargo.toml`/`Cargo.lock`; the Tauri bundle
    contains the `externalBin` sidecar; the logon scheduled task is
    registered; Desktop health-check respawn works.
14. **UI-state acceptance** (answers F6): delivery labels, quarantine list,
    and waiting-for-author indicator each shown driven by real sidecar state,
    not mocks.
15. Unit/integration coverage per milestone: ingress validation (signature,
    principal match, membership), `REQ`/`COUNT` subset conformance, outbox
    ordering (parent-before-reply, per-author FIFO), digest construction
    (ordering, mention neutralization, edge-identity signature), routing
    split.

## Safety implications

- **No impersonation surface.** The sidecar never re-signs user events and
  holds no human/agent keys; phase 1 adds no privileged replay capability
  anywhere. The drain API's ownership check means no session can claim or
  acknowledge another author's events. The edge identity can author only its
  own receipts, subscriptions, and digests.
- **The provisioned edge identity is a real member of selected channels.**
  This is a deliberate, owner-visible grant (F1): compromise of the
  edge-device key exposes read access to those channels and the ability to
  post digests as itself — not the ability to impersonate anyone. Mitigation:
  DPAPI storage, loopback-only exposure, owner-side revocation (§7) that the
  sidecar honors fail-closed, and the private-channel revocation gate.
- **Loopback only.** The sidecar binds to localhost exclusively. No LAN or
  remote exposure; no new open ports beyond loopback.
- **Canonical relay untouched.** No server-side code ships or deploys in this
  phase; the blast radius of any defect is this one PC's local cache.
- **Auth boundaries preserved.** NIP-42/NIP-98 verification runs unchanged
  against the loopback URL; AUTH events are not stored; membership is
  enforced at local ingress from mirrored state and re-enforced by the
  canonical relay at republish, where revocation wins.
- **Community isolation.** Binding + handshake (§14) fail closed, preventing
  cross-community cache poisoning when Desktop switches workspaces.
- **Notification hygiene.** Digest content is mention-neutralized so a
  catch-up digest cannot re-trigger every mentioned human and agent.
- **Data integrity.** Exact signed bytes are stored; event-ID dedup prevents
  duplicate canonical history; quarantine makes rejects visible instead of
  silently dropped or infinitely retried.

## Out of scope / do NOT touch

- No upstream/server-side code, no deploy to Block's relay, no edge replay
  command. (The option-1 upstream PR is a separate non-gating lane, drafted
  after M2; phase 1 ships without it.)
- No runtime membership mutation by the sidecar or any agent. The one-time
  owner-performed setup grant for the edge identity (§6) is the sole
  membership action associated with this project, and the owner performs it.
- No offline uploads or new attachments; no blob outbox. New Blossom uploads
  fail clearly until online. `crates/buzz-cli` `/upload` stays canonical.
- No offline git hosting or synchronization. Existing worktrees remain
  usable; clone/fetch/push stay direct-to-upstream. Do not touch the
  clone-origin check in `desktop/src-tauri/src/commands/project_git_exec.rs`.
- No offline repo/issue/PR events, workflows, moderation/admin changes, DMs,
  reactions/edits/deletes, presence/typing, huddles, search, or social
  publishing. All stay canonical-only and fail clearly offline.
- One PC, one community, explicitly selected channels. No peer-to-peer LAN
  mode, no cross-device conflict resolution.
- Process rules: all work on feature branches in isolated worktrees off base
  `44337aa4f54ee17a7eb85c708ccc8fccc3bae5bb`; never the default branch; no
  force-push, history rewrite, or remote-branch deletion; commit identity and
  trailers per `AGENTS.md`.

## Rollback path

- **Config-level (instant):** unset `BUZZ_EDGE_RELAY_URL` and stop the
  scheduled task. Every client reverts to today's canonical-only routing; the
  sidecar is inert. This is the first-line rollback at any milestone.
- **Identity:** the owner removes the edge-device pubkey's channel
  memberships and attestation; the sidecar's own fail-closed check (§7) makes
  this an independent remote kill switch even if the PC is unattended.
- **Data:** SQLite files are additive and local. Before deleting them, export
  any queued-but-unsynced events to a plain-text digest file so no authored
  content is silently lost; then archive or delete the database.
- **Code:** revert or abandon the feature branch; no migrations, no schema or
  data changes exist outside the sidecar's own local database; canonical
  relay state requires no cleanup because phase 1 never gained privileged
  write access to it.
- **Partial-milestone failure:** each milestone is independently revertible —
  M3's routing split is behind the env var and handshake, M4's sync logic
  only runs when the sidecar is enabled, M5's watchdog wiring is a scheduled
  task that can be unregistered without touching M2–M4 code.
