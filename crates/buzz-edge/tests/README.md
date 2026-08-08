# Buzz Edge release gates (M5)

The phase-1 local-continuity spec
(`docs/specs/SPEC-2026-08-05-buzz-edge-phase1-local-continuity.md`, "Test list")
lists **eleven** release gates. **Three are built** (2, 3, 4). **Eight are not**
(1, 5, 6, 7, 8, 9, 10, 11). Every one of the eleven is accounted for below,
because the failure this directory is supposed to guard against — a gate that
reads as coverage when it is not there — is committed just as easily by a
document that lists six of eleven and lets the reader infer the rest.

Run the built ones with `just edge-gates`, or one at a time with
`just edge-gate2`, `just edge-gate3`, `just edge-gate4`. Every recipe passes
`--nocapture`: these gates report collection counts and measurements, and a
captured run reduces all of that to the word `ok`.

## Status of all eleven

| Gate | Status | Where |
|---|---|---|
| 1 — full cut (>15 min outage) | **not built** | needs a real relay + a real network cut; see below |
| 2 — slow-upstream transport SLO | **built** | `gate2_slow_upstream_slo.rs` |
| 3 — restart survival | **built** | `gate3_restart_survival.rs` |
| 4 — key hygiene | **built** | `gate4_key_hygiene.rs` |
| 5 — private-channel end-to-end | **not built** | needs a real relay enforcing private-channel membership |
| 6 — direct-to-upstream post-revocation | **not built** | needs a real relay + relay-level membership removal |
| 7 — authorization-lease gates (a)–(h) | **not built as a gate**; parts covered by unit tests | see below |
| 8 — mixed-age thread | **not built as a gate**; demotion covered by unit tests | see below |
| 9 — community-switch | **not built as a gate**; binding refusal covered by unit tests | see below |
| 10 — digest durability (a)–(c) | **not built as a gate**; (b) and (c) covered by unit tests | see below |
| 11 — packaging lifecycle | **not built**; lives in `desktop/`, not in this crate | see below |

"Covered by unit tests" is **not** the same as "gated". A unit test proves a
function behaves; a release gate proves the assembled system behaves under the
spec's scenario. Where unit coverage exists it is named below so the next
session knows what it can reuse — not so anyone can count it as the gate.

## Built

| Gate | File | What it proves |
|---|---|---|
| 2 — slow-upstream transport SLO | `gate2_slow_upstream_slo.rs` | Local **submit**-to-peer-delivery p95 ≤ 250 ms and max ≤ 1 s, with no send blocked on an upstream acknowledgment, while the real upstream mirror sits in its live read loop against a relay that acknowledges no `EVENT` and sends no `EOSE` for five minutes |
| 3 — restart survival | `gate3_restart_survival.rs` | No outbox loss and no duplicate canonical events across a sidecar restart and a Desktop reconnect, with event-ID dedup demonstrated four ways |
| 4 — key hygiene | `gate4_key_hygiene.rs` | The sidecar writes no author or edge private key into its data directory tree and puts none on the client wire, and signs its own artifacts only under the provisioned edge identity |

Each built gate's own module header states its measurement boundary and its
limits. Two of those are worth repeating here, because both are places where
the gate deliberately claims **less** than its title suggests:

- **Gate 2 does not use the spec's literal measurement boundary.** The spec says
  "from the sidecar returning `OK` … to each subscribed peer", which assumes the
  sidecar acknowledges before it fans out. `accept_event` does the reverse — it
  awaits `fan_out` and only then produces the `OK` — so the literal interval is
  scheduling jitter, negative by construction, and stays green when a 400 ms
  sleep is added at the top of `fan_out`. The gate asserts the spec's budgets
  against **submit → peer delivery** instead, and reports `OK → peer delivery`
  as an unasserted diagnostic. Do not "fix" it back without first changing
  `accept_event`.
- **Gate 3's recovery is caused by compressed time, not by the restart.** It
  calls `expire_outbox_leases` directly, because nothing about a restart or a
  reconnect releases a live 60-second drain lease. What it proves is that the
  rows survive the process boundary intact and come back complete and exactly
  once when the lease expires.
- **Gate 4 does not search the Windows Credential Manager**, which is where
  `main.rs` actually persists the edge identity (as a bech32 `nsec` string), nor
  the sidecar↔relay wire, nor process memory. Those exclusions are listed in the
  file header.

`gate_harness/mod.rs` is the shared fixture — the same real store, real signed
kind-39002 roster, real authorization lease, and authenticated WebSocket session
that `status_and_requeue.rs` stands up. Note that it is a **harness** startup,
not `main.rs`: no gate in this directory executes the real binary's entry point,
so environment parsing, the keyring-backed identity load, and the mirror spawn
are unproven by all of them.

## Not built: gates 1, 5, and 6 — they need a real canonical relay

These three make claims about **upstream** behaviour. Every one of them requires
a real canonical relay with real membership enforcement, and two of them require
the network between the sidecar and that relay to be manipulated. A version
written against a stub would assert something about the stub while reading as if
it had proven something about the relay, so none of them is present here — not
even skipped or `#[ignore]`d, because an ignored gate in the list looks like
coverage.

What each one needs, so the next session does not have to re-derive it:

### Gate 1 — full-cut gate

> Sever upstream connectivity for more than 15 minutes. Desktop and two local
> agents exchange thread replies immediately throughout; the sidecar is
> restarted mid-outage with no loss, continuing under its valid authorization
> lease (§7); on reconnect, sub-15-minute events not demoted by the mixed-age
> policy land upstream exactly once with identical event IDs and thread tags;
> demoted and older events appear in the digest (chunked if over the size
> bound), in order, exactly once.

Needs to be stood up:

- A real canonical relay (`crates/buzz-relay`, its Postgres, and the HTTP
  `/query` bridge the mirror backfills from) with a community, a channel, and
  relay memberships for the edge identity and all three authors.
- A provisioned edge identity that the relay admits as a direct member, and a
  signed kind-39002 roster published by the relay's own signing identity.
- Three client connections through the sidecar (Desktop plus two agents),
  exchanging **threaded** replies — `e`-tagged roots and replies, not flat
  messages — because the gate's claim is about thread tags surviving.
- A drain client that actually submits claimed rows upstream, acknowledges
  them, and materialises and submits digests. M2 leaves digest *submission* to
  M4; if that is not wired at the time this gate is written, the gate must
  drive it explicitly rather than assert around it.

Needs to be severed: the sidecar's path to the canonical relay only — not the
loopback path between the clients and the sidecar. On Windows that is a
firewall rule or a proxy in front of the relay port, not a stopped relay
process: the spec's scenario is an unreachable relay, and a relay that is *down*
also stops being able to answer the reconnect at the end. The cut has to hold
for **more than 15 minutes of the timestamps the mixed-age policy reads**, which
is a real-clock quantity in the current implementation; if the gate is to run in
seconds, the demotion window has to be injectable, and making it injectable is
part of building this gate.

Would have to be true at the end:

- Every message authored during the outage is still exactly one row locally.
- The sidecar was restarted mid-outage and lost nothing, and local ingress
  continued throughout on the offline lease (`AuthorizationStartup::OfflineLease`).
- After reconnect, each sub-15-minute event exists upstream exactly once, under
  **the same event ID** it had locally, with its `e` tags intact.
- Each demoted or older event appears in canonical history only inside an
  edge-authored digest, in order, exactly once, chunked at ≤ 200 KiB per part
  with `part i` / `total N` tags, and its source rows resolve only after every
  part is acknowledged.
- Nothing lands in quarantine as `reply parent not found`.

### Gate 5 — private-channel end-to-end gate

> In a private selected channel, prove the provisioned edge identity mirrors
> events, builds cached authorization, and lands a digest via ordinary ingest;
> then revoke its channel membership and prove fail-closed behavior (no ingress,
> no mirror, clean canonical fallback, digest rejected upstream).

Needs to be stood up: the same real relay as gate 1, plus a **private** channel
whose membership the relay enforces on both read and write, and an owner
identity with the authority to add and remove channel members. The edge identity
must be a real member of that private channel at the start — a public channel
proves nothing here, because the whole gate is about enforcement.

Needs to be severed: nothing at the network level. The change is a **membership
revocation performed upstream**: the owner removes the edge identity from the
private channel, and the relay publishes the resulting kind-39002 and/or
kind-40099.

Would have to be true:

- Before revocation: upstream kind-9 events in the private channel are mirrored
  into the local store; the cached authorization snapshot lists the private
  channel with the correct author set; a digest authored by the edge identity is
  accepted by ordinary canonical ingest (no special path).
- After revocation: local ingress for that channel is refused, the mirror stops
  accepting events for it, existing subscriptions are closed with the
  `restricted:` reason so clients fall back to the canonical relay cleanly, and
  a digest submitted by the edge identity for that channel is **rejected**
  upstream. The rejection must then revoke the author locally, per §12.

### Gate 6 — direct-to-upstream post-revocation gate

> Two parts, both bypassing the sidecar. (a) Fresh-connection: after the owner
> removes the edge identity's relay membership, a new direct upstream connection
> must be refused admission (reads and ingest both). (b) Live-connection:
> authenticate a direct upstream connection first, remove relay membership
> second, then attempt REQ and ingest over that same live connection — the
> observed behavior must match the §6 boundary statement (removal blocks next
> authentication only; the live session is expected to retain access until it
> disconnects).

Needs to be stood up: the real relay with **relay-level** membership (not just
channel membership) for the edge identity, and an owner able to remove it. Both
parts connect **directly to the relay**, not through the sidecar; the sidecar
does not need to be running at all.

Needs to be severed: nothing. The change is again a membership removal, this
time at the relay level.

Would have to be true:

- (a) A fresh `NostrWsConnection::connect_authenticated` as the removed edge
  identity is refused at NIP-42, and both a `REQ` and an `EVENT` over any
  connection it can still open are refused.
- (b) A connection authenticated *before* the removal keeps working for `REQ`
  and ingest until it disconnects. This gate **documents** that boundary; it
  must assert exactly what the relay does and must not be written to assert the
  stronger property, because §6 does not claim it. If the relay's behaviour ever
  becomes stricter, this gate is where that gets recorded.

## Not built: gates 7, 8, 9, and 10 — partial unit coverage, no gate

Each of these has real unit coverage of the *decision layer* it depends on. None
of them is gated, because in every case the part the spec actually asks about is
the part that needs a real upstream, a real Desktop, or both.

### Gate 7 — authorization-lease gates (a)–(h)

The spec asks for eight sub-scenarios: (a) offline restart on a valid lease,
(b) offline restart on an expired lease, (c) revocation performed upstream while
offline and enforced at reconnect before any queued submission, (d) reconnect
always refreshes before drain, (e) author removed while offline, (f) author
removed while online via the kind-40099 announcement, (g) author added while
offline, (h) roster-staleness fault injection with both carriers suppressed.

Already covered as unit tests, reusable but not the gate:

- `src/eligibility.rs`: `valid_signed_lease_restores_offline_eligibility`,
  `expired_lease_and_definitive_denial_fail_closed`,
  `signed_roster_replacement_bounds_removed_and_added_authors`,
  `observed_revocation_cannot_restore_the_old_offline_lease`,
  `membership_snapshot_must_be_signed_by_the_advertised_relay_identity`.
- `src/storage.rs`: `system_removal_survives_an_unchanged_roster_refresh`,
  `suppressed_removal_carriers_leave_local_access_until_canonical_rejection`,
  `pre_snapshot_system_removal_cannot_override_a_newer_roster`,
  `changed_roster_projection_can_reauthorize_a_removed_author`,
  `signed_authorization_lease_is_bounded_and_survives_restart`.
- `src/lib.rs`: `authorization_refresh_closes_removed_author_and_channel_subscriptions`,
  `lease_expiry_closes_existing_subscription`.
- Gate 3 exercises (a) end-to-end incidentally: the restarted sidecar comes back
  on `AuthorizationStartup::OfflineLease` and keeps serving.

What is missing for the gate: (c), (d), (f), and (g) are all statements about
what happens **at a reconnect refresh against a live relay** and about ordering
relative to drain. They need the real relay of gate 1, a drain client, and a
compressed refresh clock.

### Gate 8 — mixed-age thread gate

> Author A posts a thread root at T−16 minutes, author B replies at T−5 minutes,
> reconnect at T. Prove the root and the reply both go to the digest path, in
> order; no orphan submission reaches upstream; nothing lands in quarantine as
> `reply parent not found`.

Already covered as unit tests: `src/storage.rs`
`a_stale_parent_drags_its_fresh_replies_to_the_digest_path`,
`a_fresh_thread_is_left_on_the_exact_path`,
`demotion_propagates_down_a_multi_level_thread`,
`a_quarantined_ancestor_demotes_its_descendants`, `demotion_is_idempotent`.

What is missing for the gate: the two upstream-facing halves — "no orphan
submission reaches upstream" and "nothing lands in quarantine as
`reply parent not found`" — both require a real reconnect and a real drain
against a relay. The demotion window is also real-clock today (same blocker as
gate 1).

### Gate 9 — community-switch gate

> Two communities containing equal channel UUIDs; switch Desktop's active
> community both directions; prove handshake rejection, fail-closed canonical
> fallback, and zero cross-community read, write, or cache reuse.

Already covered as unit tests: `src/storage.rs`
`database_header_rejects_equal_channel_ids_from_another_community` and
`canonical_binding_requires_a_plain_origin`; `src/lib.rs`
`mismatched_community_handshake_is_rejected_before_auth`.

What is missing for the gate: the *switch* itself. The spec's scenario is
Desktop changing its active community in both directions and falling back to
canonical routing cleanly, which is a `desktop/` behaviour driving two sidecar
data directories — not something a single in-process store test observes.

### Gate 10 — digest durability gates (a)–(c)

> (a) ambiguous upstream response followed by retry → identical bytes re-sent,
> exactly one canonical digest; (b) sidecar restart between digest
> materialization and acknowledgment → same; (c) backlog whose digest content
> exceeds 256 KiB → deterministic `part i/total N` chunks each ≤ 200 KiB, source
> rows resolved only after all chunks acknowledged.

Already covered as unit tests, and these are close to complete for (b) and (c):
`src/storage.rs` `digest_materialization_survives_restart_with_identical_signed_bytes`,
`digest_chunks_stay_under_the_ingest_limit_and_preserve_order`,
`digest_chunking_is_deterministic`, `an_oversized_single_message_still_gets_a_chunk`,
`sources_resolve_only_after_every_part_lands`,
`a_rejected_part_quarantines_the_batch_and_its_sources`,
`digest_sources_are_unique_across_batches_and_inputs_are_validated`.

What is missing for the gate: (a) is entirely upstream — an *ambiguous* response
(no answer, then a retry) and the proof that canonical history ends up with
exactly one digest. That is a relay-side count, and there is no way to take it
without a relay. (b) and (c) are proven at the store layer but never through the
real submit path.

## Not built: gate 11 — packaging lifecycle

> Login race (task and Desktop starting together → readiness gate holds agents
> until edge answers or the 2-second fallback fires), sidecar crash →
> scheduled-task restart-on-failure brings it back and clients re-attach, reboot
> → sidecar up before Desktop interaction, upgrade → task re-registered pointing
> at the new binary, uninstall → task unregistered and process stopped, nothing
> left running.

Nothing in this crate can prove any of it. It lives in `desktop/` — the
supervisor (`desktop/src-tauri/src/edge_supervisor.rs`), the NSIS installer hook
(`desktop/src-tauri/windows/edge-task.nsi`), and the Windows Scheduled Task they
register — and it needs a real install/upgrade/uninstall cycle on a real Windows
user account. It is listed here only so the count of eleven closes; the gate
belongs with the packaging work, not in `crates/buzz-edge/tests/`.

## Notes for whoever builds them

- Do not let a missing precondition become a skip. If the relay is not up, the
  gate fails with a message saying the relay is not up.
- Gates 1 and 5 both need digest submission. Check whether M4 has wired it
  before assuming it exists.
- Gate 1's 15-minute window is the only part of these three that is expensive
  purely because of wall-clock time. Making the demotion window injectable is
  the cheapest way to make that gate runnable in CI, and it is a change to the
  sidecar, not to the gate.
- Before believing a gate, break the property it names and watch the named
  assertion fail. Gate 2 was green for weeks against a boundary that could not
  move, and gate 4's positive control searched a buffer the test had just built.
  Both looked fine in review.
