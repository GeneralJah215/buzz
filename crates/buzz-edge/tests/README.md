# Buzz Edge release gates (M5)

The phase-1 local-continuity spec
(`docs/specs/SPEC-2026-08-05-buzz-edge-phase1-local-continuity.md`, "Test list")
lists eleven release gates. This directory holds the ones that can be proven
against a real sidecar without a real canonical relay.

Run them with `just edge-gates`, or one at a time with `just edge-gate2`,
`just edge-gate3`, `just edge-gate4`. Every recipe passes `--nocapture`: these
gates report collection counts and measurements, and a captured run reduces all
of that to the word `ok`.

## Built

| Gate | File | What it proves |
|---|---|---|
| 2 — slow-upstream transport SLO | `gate2_slow_upstream_slo.rs` | Local ingress-to-peer-delivery p95 ≤ 250 ms and max ≤ 1 s, with no send blocked on an upstream acknowledgment, while the real upstream mirror sits against a reachable relay that answers nothing for 60 s |
| 3 — restart survival | `gate3_restart_survival.rs` | No outbox loss and no duplicate canonical events across a sidecar restart and a Desktop reconnect, with event-ID dedup demonstrated four ways |
| 4 — key hygiene | `gate4_key_hygiene.rs` | The sidecar holds only the provisioned edge identity key: nothing durable, nothing on the wire, nothing signed under an author's identity |

`gate_harness/mod.rs` is the shared fixture — the same real store, real signed
kind-39002 roster, real authorization lease, and authenticated WebSocket session
that `status_and_requeue.rs` stands up.

## Not built: gates 1, 5, and 6

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

## Notes for whoever builds them

- Do not let a missing precondition become a skip. If the relay is not up, the
  gate fails with a message saying the relay is not up.
- Gates 1 and 5 both need digest submission. Check whether M4 has wired it
  before assuming it exists.
- Gate 1's 15-minute window is the only part of these three that is expensive
  purely because of wall-clock time. Making the demotion window injectable is
  the cheapest way to make that gate runnable in CI, and it is a change to the
  sidecar, not to the gate.
