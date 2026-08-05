# SPEC-2026-08-05 — buzz-edge Phase 1: Local Continuity

Status: APPROVED build (option 3, James, 2026-08-05); spec revision 5 answering
Ava FAIL verdicts on `d5382d11e` (rev-1, event `28d54430…`), `5b96643d8`
(rev-2 R1–R6, event `d3aa9d73…`), `951eece30` (rev-3 findings 1–3, event
`8f242013…`; finding 3's lease duration is an owner decision, recorded in §7),
and `48cbd7c4d` (rev-4 finding 1 — roster freshness, event `39657457…`; the
residual author-revocation risk is an owner decision, recorded in §7).
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
- Desktop supervisor: new `desktop/src-tauri/src/edge_supervisor.rs` — verifies
  and repairs the scheduled task at every launch and after upgrades, performs
  the readiness gate and health-check respawn of Behavior contract §16.
- Installer hooks: new `desktop/src-tauri/windows/edge-task.nsi`, wired via a
  new `bundle > windows > nsis > installerHooks` entry in
  `desktop/src-tauri/tauri.conf.json` (no NSIS section exists in the pinned
  tree today) — registers, upgrades, and unregisters the scheduled task.
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
bundled via Tauri `externalBin`. The NSIS installer hook registers a Windows
Scheduled Task scoped to the installing user (no elevation, `LimitedToken`),
with the executable path quoted, trigger = user logon, and restart-on-failure
with backoff (three restarts at 1-minute intervals per failure window); the
hook re-registers on upgrade and unregisters + stops the process on uninstall.
A logon task alone guarantees neither start-before-Desktop nor crash recovery,
so `edge_supervisor.rs` closes both gaps: at every Desktop launch it repairs a
missing/outdated task registration, then gates agent/harness startup on edge
readiness — handshake ping with a 2-second deadline, after which Desktop
cleanly selects canonical-only routing and retries the edge in the background —
and respawns the sidecar if the health check finds it dead. M5 wires the
gates; it does not choose the packaging form.

Explicitly NOT touched: `desktop/src-tauri/src/commands/project_git_exec.rs`
clone-origin check (git stays canonical-only), all `crates/buzz-relay` server
code, and every canonical-only command path listed in "Out of scope".

## Behavior contract

Event allowlist and local ingress:

1. The sidecar accepts persistent kind-9 channel messages only, with their
   existing mention and thread tags, for explicitly selected channels of one
   community.
2. At local ingress it verifies the event signature; requires the event pubkey
   to match the authenticated NIP-42/NIP-98 principal; checks channel
   membership against the current working roster (§7); stores the
   exact signed event bytes plus a device-signed local receipt; returns `OK`;
   and fans out to all local subscribers immediately.
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
   setup, while online, the community owner (a) adds the edge-device pubkey as
   a **direct relay member** — chosen precisely because direct membership is
   an existing, admin-removable server-side grant evaluated at admission
   (`crates/buzz-relay/src/api/mod.rs:76-79`), unlike a NIP-OA auth tag,
   which is a static signature with no registry and no admission-time
   revocation (`api/mod.rs:81-100`; `crates/buzz-sdk/src/nip_oa.rs:179-235`)
   and therefore is NOT the admission basis here — and (b) explicitly adds
   the edge-device pubkey as a member of every selected channel, using the
   existing membership flow. This is a one-time setup action; runtime
   membership mutation remains out of scope. Rationale: upstream reads are
   scoped to channels accessible to the authenticated pubkey
   (`crates/buzz-relay/src/handlers/req.rs:94,133-167`; membership resolution
   `crates/buzz-db/src/channel.rs:746-771`), and ingest requires
   `event.pubkey == authenticated identity`
   (`crates/buzz-relay/src/handlers/ingest.rs:1878-1881`) — so mirroring
   private selected channels and ingesting edge-signed digests both require
   real membership.
   Revocation semantics, stated exactly (corrected per rev-3 review): relay
   membership is evaluated when AUTH succeeds
   (`crates/buzz-relay/src/handlers/auth.rs:216-238`); the removal handler
   deletes the membership row and publishes NIP-43 state but does **not**
   invalidate or disconnect an already-authenticated connection
   (`crates/buzz-relay/src/handlers/relay_admin.rs:362-419`). Therefore
   removing the edge identity's **relay membership blocks its next
   authentication only** — it is not an immediate kill of a live session,
   and this spec makes no immediate-kill claim. Removing a **selected
   private-channel membership** is the per-operation revocation lever, since
   channel access is resolved per read/ingest operation. Phase 1 changes no
   hosted-relay code, so a true immediate kill (server-side disconnect or
   per-operation membership recheck on live sessions) is explicitly out of
   scope; it is a candidate for the option-1 upstream lane. Local
   mitigation: while online, the sidecar tears down and re-authenticates its
   upstream session at every authorization-snapshot refresh (§7), bounding
   how long a removed edge identity's live session can persist through the
   sidecar's own behavior. Residual risk documented: while the edge identity
   remains a relay member, open channels stay readable to it, because open
   visibility itself grants access.
7. A channel is **eligible for edge routing only if the edge identity's
   membership in it is confirmed or covered by a valid authorization lease**
   (answers rev-2 R1; contents extended per rev-3 review; roster-freshness
   contract corrected per rev-4 review). Two different trust levels are in
   play, and this spec names them separately rather than letting one
   `verified_at` imply both:

   **(a) Edge eligibility — authoritative.** The relay enforces the edge
   identity's own access at every AUTH and on every read it performs
   (`crates/buzz-relay/src/handlers/auth.rs:216-238`;
   `crates/buzz-relay/src/handlers/req.rs:94,133-167`). A refresh therefore
   proves edge eligibility directly: authenticate fresh, read each selected
   channel, record what succeeded. `verified_at` attests exactly this
   authoritative check and nothing more.

   **(b) Author roster — a relay-signed projection plus removal signals,
   not an authoritative read.** The per-channel active-author roster comes
   from the kind-39002 group-members list (NIP-29 group membership, not
   NIP-43 relay membership — label corrected per rev-4 review). The pinned
   relay commits a membership mutation first, then publishes the updated
   kind-39002 **best-effort**: emission failure is swallowed with a warning
   (`crates/buzz-relay/src/handlers/side_effects.rs:1624-1666` —
   `remove_member` commits at `:1624-1627`, discovery-emission failure
   warned at `:1651-1653`; same shape on self-leave at `:2307-2329`), and
   production runs no reconciliation that repairs a stale projection (the
   only reconciler is dev/CI-gated behind `BUZZ_RECONCILE_CHANNELS`,
   `crates/buzz-relay/src/main.rs:572-576`, and repairs only missing
   kind-39000). No client-readable authoritative per-channel member read
   exists in the pinned tree — the CLI's own `channels members` reads the
   same kind-39002 projection
   (`crates/buzz-cli/src/commands/channels.rs`, `cmd_list_channel_members`).
   Normative rules that follow:
   - **Re-fetching an unchanged kind-39002 proves nothing and renews
     nothing.** The snapshot stores the roster's source event ID, that
     event's `created_at`, and the fetch cursor. A refresh returning the
     same event ID advances no freshness field. Roster state advances only
     when a kind-39002 with a different event ID is fetched, and then only
     by whole-record replacement.
   - **Removal signals force immediate local revocation.** The sidecar
     consumes three independent relay-signed carriers of a removal, all
     available for its selected channels: (1) a changed kind-39002; (2) the
     kind-40099 channel system message with type `member_removed` /
     `member_left` emitted by every removal handler
     (`side_effects.rs:759-779` definition; emitted at `:1639-1649` and
     `:2316-2325`) — channel-scoped, so the §8 mirror receives it live and
     in reconnect catch-up; (3) the kind-44100/44101 global membership
     notifications (`side_effects.rs:1140-1196`), the same stream the ACP
     harness already consumes (`crates/buzz-acp/src/relay.rs:3238-3250`).
     On any removal signal for (channel, author), that author leaves the
     working roster immediately — local ingress fails closed for them — and
     is re-authorized only by a subsequently fetched kind-39002 with a
     different event ID that lists them.
   - **A canonical rejection is authoritative.** If an author's drain
     submission is rejected upstream for membership, the sidecar marks that
     (channel, author) revoked exactly as if a removal signal had arrived.
   - **The claim this spec makes — and the one it does not.** Online author
     revocation is signal-driven: it takes local effect on receipt of any
     carrier, ordinarily within seconds, and never later than the next
     refresh that observes a changed kind-39002. It is **not guaranteed
     bounded**: every carrier is best-effort in the pinned relay, so if the
     kind-39002 republication, the kind-40099 system message, and the
     kind-44101 notification are all lost, a removed author retains
     **local-only** ingress on this one PC until a later signal, roster
     change, or canonical rejection arrives. Canonical history is never
     exposed — the canonical relay re-checks membership at ingest and
     rejects them regardless. Fail-closed roster expiry was considered and
     rejected: with no authoritative read to renew against, a roster-age
     timer would shut off quiet channels on a schedule, defeating the
     availability goal of the project.
     **Owner-decision record (residual author-revocation risk):** requested
     from James 2026-08-05 (buzz-infra event `64e06a08…`; options accept
     residual / fail closed, recommendation accept). DECIDED 2026-08-05:
     **accept the residual** (Option A). Reply event
     `adb16c155dfa97ec576e8667fecb95f535d2a1823cc45128f54a43ef30c9e023`
     (buzz-infra, 2026-08-05T19:42:18Z, verbatim: "lets go with A").

   Snapshot contents (one atomic signed record, verified by the edge key on
   load, replaced whole, never partially updated): the community binding;
   the eligible channel set; the per-channel author roster with its source
   kind-39002 event ID, that event's `created_at`, and the fetch cursor;
   the signal cursor (last processed kind-40099/44101 position per
   channel); and `verified_at` (edge-eligibility attestation time). Local
   ingress (§2) authorizes submitting principals against the **working
   roster**: the snapshot roster minus every author since removed by a
   removal signal or canonical rejection. Refresh cadence: on every
   reconnect, and at least every 6 hours while online, each refresh
   preceded by a full upstream session recycle (§6). Startup rules,
   exactly: if upstream is reachable, verify edge eligibility fresh and
   process the mirrored signal backlog before first ingress. If upstream is
   unreachable, serve from the snapshot only while
   `now − verified_at ≤ LEASE` (LEASE = owner-selected duration; see the
   owner-decision record below); past the lease, fail closed to
   canonical-only (which, offline, means messaging halts — disclosed, not
   hidden). Trade-off stated: within the lease, a revocation performed
   upstream during a total outage — of the edge identity or of any local
   author — is enforced locally only at the next successful reconnect:
   lag ≤ LEASE for the edge identity's own eligibility; for authors,
   reconnect catch-up delivers the mirrored removal signals, subject to the
   lost-signal residual disclosed above. On reconnect the sidecar refreshes
   eligibility and processes the signal backlog first, before any drain;
   channels or authors revoked while offline drop immediately — no local
   ingress, no mirroring, cached rows retained read-only, their queued
   events follow the §12 revocation rule, and clients transparently fall
   back to canonical.
   **Owner-decision record (LEASE):** requested from James 2026-08-05
   (buzz-infra event `4e7c5563…`; options 24 h / 72 h / 7 d, recommendation
   72 h). DECIDED 2026-08-05: **LEASE = 7 days (168 hours)**, chosen by
   James over the 72 h recommendation, accepting the longer revocation lag
   for maximum offline uptime. Reply event
   `39ddb3b33a0d1a0657cc2451aeb67059e8f8a994687751ca328e31e488118e5d`
   (Projects Hub, 2026-08-05T18:50:43Z, verbatim: "7 days").
8. Using the edge identity, the sidecar subscribes upstream to the selected
   channels and mirrors received events and member state into SQLite, so
   local reads are served from loopback even when upstream is merely slow.
   The mirror explicitly includes the §7 removal-signal kinds — kind-40099
   channel system messages and kind-44100/44101 membership notifications —
   consumed live while connected and as backlog during reconnect catch-up,
   feeding the working-roster rules of §7.

Reconnect synchronization — durable outbox:

9. The SQLite outbox and cache survive sidecar and Desktop restarts.
10. Outbox rows carry an explicit drain-state machine (answers F3; tightened
    for rev-2 R3): `pending → claimed(lease) → delivered | quarantined`, with
    `claimed → pending` on lease expiry. There is **no separate `submitted`
    state**: a row stays `claimed` until the author's per-event
    acknowledgment arrives, so the crash window between upstream acceptance
    and sidecar acknowledgment cannot strand a row — the lease expires, the
    row returns to `pending`, the next drain re-submits the **identical
    signed bytes**, and upstream's event-ID dedup answers duplicate, which
    the protocol records as `delivered`.
11. **Author-drain protocol** (answers F3): the sidecar cannot and does not
    submit another identity's events upstream (blocked by
    `ingest.rs:1878-1881`). Instead:
    - An author client (Desktop or ACP identity) opens its normal
      NIP-42-authenticated loopback session and calls the drain API. It may
      claim only rows whose author pubkey equals the session principal — the
      sidecar enforces this ownership check.
    - A claim takes a renewable 60-second lease over an ordered batch.
      Ordering is **globally dependency-gated, not merely per-author**
      (answers rev-2 R2): a row is claimable only when every locally known
      ancestor in its thread chain is already `delivered` to canonical
      history — because canonical ingest rejects a reply whose parent is not
      stored (`crates/buzz-relay/src/handlers/ingest.rs:623-632`, "reply
      parent not found") and threads routinely cross identities. Within the
      claimable set, ordering is FIFO per author/channel. Lease expiry
      returns unacknowledged rows to `pending`.
    - **Mixed-age thread policy** (rev-2 R2): drain evaluates thread
      components before claiming. If any required ancestor cannot be
      replayed with its original ID — it is older than the drift window,
      quarantined, or revocation-blocked — then that ancestor **and every
      dependent descendant, even descendants still inside the drift window**,
      are demoted to the local-only/digest path together, in order. Fresh
      descendants of a stale parent are never submitted upstream; nothing is
      ever submitted as an orphan.
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
12. Events still inside the relay's ±15-minute drift window — and not demoted
    by the mixed-age thread policy in §11 — are submitted as-is and retain
    their exact event IDs and thread tags. Demoted and older events remain
    permanently marked local-only and are collapsed, in order, into a
    catch-up digest per channel — authored and signed by the provisioned edge
    identity (a member, so ordinary ingest accepts it) — containing author,
    local timestamp, and quoted, mention-neutralized content. The digest
    becomes canonical; the original event IDs and thread structure of those
    events do not. Digest construction is **transactional and byte-stable**
    (answers rev-2 R4): the sidecar persists, in one SQLite transaction, the
    exact source-row set and the exact signed digest event bytes **before**
    first submission; every retry — after an ambiguous response, a crash, or
    a restart — re-submits those identical bytes, so the event ID cannot
    change and upstream dedup makes acceptance exactly-once. Because relay
    ingest rejects content over 256 KiB
    (`crates/buzz-relay/src/handlers/ingest.rs:1868-1872`), digest content is
    deterministically chunked: rows in order, greedily packed to ≤200 KiB per
    chunk, each chunk a separate pre-signed event tagged `part i` / `total
    N`. Source rows transition to resolved only after **every** chunk is
    acknowledged accepted-or-duplicate. Membership revocation wins: events
    authored under stale local authorization may remain local but are
    rejected from canonical history.
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
  outbox, community binding, authorization-lease snapshot (with roster
  source-event fields, signal cursor, and working-roster removals), and
  digest materialization store; signature/membership checks, `REQ`/`COUNT`
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
   throughout; the sidecar is restarted mid-outage with no loss, continuing
   under its valid authorization lease (§7); on reconnect, sub-15-minute
   events not demoted by the mixed-age policy land upstream exactly once
   with identical event IDs and thread tags; demoted and older events appear
   in the digest (chunked if over the size bound), in order, exactly once.
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
   channel membership and prove fail-closed behavior (no ingress, no mirror,
   clean canonical fallback, digest rejected upstream).
6. **Direct-to-upstream post-revocation gate** (rev-2 R5, extended per rev-3
   review): two parts, both bypassing the sidecar. (a) Fresh-connection:
   after the owner removes the edge identity's relay membership, a new
   direct upstream connection must be refused admission (reads and ingest
   both). (b) Live-connection: authenticate a direct upstream connection
   first, remove relay membership second, then attempt REQ and ingest over
   that same live connection — the observed behavior must match the §6
   boundary statement (removal blocks next authentication only; the live
   session is expected to retain access until it disconnects). This gate
   documents the real boundary; the spec claims nothing stronger.
7. **Authorization-lease gates** (rev-2 R1; roster gates added per rev-3
   review): (a) offline sidecar restart with a valid lease → local messaging
   continues from the snapshot; (b) offline restart with an expired lease →
   fail closed, canonical-only, clearly surfaced; (c) revocation performed
   upstream while offline → enforced at reconnect refresh before any queued
   submission, affected rows follow the §12 revocation rule; (d) reconnect
   always refreshes the snapshot before drain begins; (e) **local author
   removed from a channel while offline** → within the lease their local
   ingress continues (disclosed lag), and it stops at the reconnect refresh
   — the stated boundary; (f) **local author removed while online, healthy
   announcement path** → their local ingress stops on receipt of the first
   mirrored removal signal (proven via the kind-40099 system message), not
   merely at the next 6-hour refresh; (g) **local author added to a channel
   while offline** → they cannot write through the edge until a refreshed
   kind-39002 with a different event ID authorizes them (canonical-only in
   the meantime, once online); (h) **roster-staleness fault injection**
   (rev-4 finding 1): commit a channel-member removal upstream, force the
   kind-39002 republication to fail, keep upstream reachable through at
   least two refresh cycles (compressed clock). Prove three things: the
   unchanged kind-39002 re-fetch advances no freshness field of the stored
   snapshot; the removed author is blocked from local ingress from the
   moment the mirrored kind-40099/kind-44101 signal arrives; and in the
   variant with all three carriers suppressed, the removed author's
   local-only ingress persists exactly as the §7 residual discloses, their
   drain submission is rejected by canonical ingest, and that rejection then
   revokes them locally — the contract's stated boundary, nothing stronger.
8. **Mixed-age thread gate** (rev-2 R2): author A posts a thread root at
   T−16 minutes, author B replies at T−5 minutes, reconnect at T. Prove the
   root and the reply both go to the digest path, in order; no orphan
   submission reaches upstream; nothing lands in quarantine as
   `reply parent not found`.
9. **Community-switch gate** (answers F4): two communities containing equal
   channel UUIDs; switch Desktop's active community both directions; prove
   handshake rejection, fail-closed canonical fallback, and zero
   cross-community read, write, or cache reuse.
10. **Digest durability gates** (rev-2 R4): (a) ambiguous upstream response
    followed by retry → identical bytes re-sent, exactly one canonical
    digest; (b) sidecar restart between digest materialization and
    acknowledgment → same; (c) backlog whose digest content exceeds 256 KiB →
    deterministic `part i/total N` chunks each ≤200 KiB, source rows resolved
    only after all chunks acknowledged.
11. **Packaging lifecycle gates** (rev-2 R6): login race (task and Desktop
    starting together → readiness gate holds agents until edge answers or
    the 2-second fallback fires), sidecar crash → scheduled-task
    restart-on-failure brings it back and clients re-attach, reboot →
    sidecar up before Desktop interaction, upgrade → task re-registered
    pointing at the new binary, uninstall → task unregistered and process
    stopped, nothing left running.

Failure-mode and protocol tests:

12. Author-drain protocol (answers F3, tightened for rev-2 R3): absent-author
    (rows stay pending, waiting-for-author surfaced), author-restart
    mid-batch, duplicate-drain, mid-batch crash with lease expiry, **crash
    after upstream acceptance but before sidecar acknowledgment** (lease
    expiry → re-submission of identical bytes → duplicate → `delivered`),
    ownership check (a session cannot claim another author's rows),
    permanent-reject → quarantine transition.
13. Duplicate upstream acknowledgment treated as success (no re-send loop).
14. Corrupt or missing local receipt → event quarantined and surfaced, sync
    of other events unaffected.
15. Offline upload attempt and offline git operation → clear, immediate
    failure with no corruption and no queuing.
16. **Negative routing tests per canonical-only class** (answers F2): with
    the edge active, channel/membership discovery, HTTP bridge and memory
    ops, observer/control, typing/presence, uploads, git, admin, and
    non-kind-9 events each provably reach only the canonical URL.
17. **Build/package acceptance** (answers F6): workspace builds with the new
    crate registered in root `Cargo.toml`/`Cargo.lock`; the Tauri bundle
    contains the `externalBin` sidecar and the NSIS installer hook; the logon
    scheduled task is registered with quoted path and user scope.
18. **UI-state acceptance** (answers F6): delivery labels, quarantine list,
    and waiting-for-author indicator each shown driven by real sidecar state,
    not mocks.
19. Unit/integration coverage per milestone: ingress validation (signature,
    principal match, working-roster membership), removal-signal processing
    (kind-40099 and kind-44101 parsing, working-roster removal, no freshness
    advance on unchanged kind-39002), `REQ`/`COUNT` subset conformance,
    outbox dependency gating (global ancestor rule, per-author FIFO within
    the claimable set), digest construction (ordering, mention
    neutralization, chunking determinism, edge-identity signature), routing
    split.

## Safety implications

- **No impersonation surface.** The sidecar never re-signs user events and
  holds no human/agent keys; phase 1 adds no privileged replay capability
  anywhere. The drain API's ownership check means no session can claim or
  acknowledge another author's events. The edge identity can author only its
  own receipts, subscriptions, and digests.
- **The provisioned edge identity is a real relay member and a member of
  selected channels.** This is a deliberate, owner-visible grant (F1):
  compromise of the edge-device key exposes read access to those channels
  (plus open channels, which any relay member can read) and the ability to
  post digests as itself — not the ability to impersonate anyone.
  Mitigation: DPAPI storage, loopback-only exposure, and two upstream-side
  revocation levers with honestly different strengths (rev-2 R5, boundary
  corrected per rev-3 review): relay membership removal blocks the edge
  identity's **next authentication** — live sessions are not disconnected by
  the pinned relay, so this is not an immediate kill (both halves proven by
  the two-part post-revocation gate); selected private-channel membership
  removal is the per-operation lever. The sidecar's 6-hour online session
  recycle bounds its own live-session exposure.
- **Revocation propagation, exactly** (rev-2 R1; corrected per rev-4
  review): the edge identity's own access is enforced authoritatively by
  the relay at every AUTH and read. Local **author** authorization rests on
  a relay-signed projection plus three independent removal signals (§7); it
  takes local effect on the first signal received — ordinarily seconds —
  and is enforced without exception at canonical ingest, but it is not
  guaranteed bounded locally when every carrier is lost. That residual is
  disclosed in §7, carries an owner-decision record there, and its blast
  radius is local-only delivery on this one PC. Offline, the owner-selected
  lease bounds continuation; reconnect processes mirrored removal signals
  and refreshes eligibility before any queued submission drains.
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
- **Identity:** the owner removes the edge-device pubkey's **relay
  membership** and its channel memberships. Honest boundary (§6): this
  blocks the edge identity's next authentication and its per-operation
  channel access; it does not sever an already-live session — the pinned
  relay has no disconnect-on-removal. Local enforcement follows at the next
  snapshot verification: within the 6-hour recycle while online, or bounded
  by the owner-selected lease if the PC is fully offline.
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
