# Intent

Build a durable, channel-scoped thread directory for Buzz Desktop at upstream
commit `5e0efb0bb95182f588390b55cc5affa09114c87e`. A person must be able to start
with only a channel, expand that channel in the sidebar, discover active or
explicitly managed threads, open one, give it a shared name, pin it, archive
it, restart the app, and find the same state again.

The implementation must reuse the existing thread read model rather than
creating a second thread system:

- `thread_metadata` remains authoritative for root/reply relationships,
  descendant counts, participants, and last reply time.
- `kind:39005` remains the timeline-specific live/page summary overlay.
- `threadActivityStorage.ts` remains a viewer-scoped notification cache. It is
  not a channel thread index: it is capped at 100, contains only activity that
  passed the viewer's notification gate, and cannot prove channel-wide thread
  history after a restart.
- The new directory is a lazy, paginated relay read keyed by channel ID. It
  must not obtain completeness by walking every channel-window page on the
  client.

The product contract for v1 is:

- Stream-channel rows can disclose a nested thread list. Forum and DM rows do
  not use this feature.
- Shared thread state consists of an optional title override, `pinned`, and
  `archived`.
- An unnamed thread is automatically active only when it has at least three
  descendants and activity within the last 30 days.
- A renamed or pinned thread stays active until explicitly archived, even when
  it is older than 30 days.
- Pinned active threads sort first; every other active thread sorts by latest
  activity. Archived threads are excluded from the default list but remain
  discoverable in an archived view and can be restored.
- Opening a directory entry navigates to the channel with both `messageId` and
  `threadRootId` set to the root ID, using the existing channel route and
  thread-panel hydration.
- Thread titles are shared channel state. Private aliases, notifications, and
  workflow automation are not part of v1.

This specification changes production behavior and must be approved before
implementation begins.

# Files to touch

Protocol and documentation:

- `docs/nips/NIP-TD.md` (new): normative wire contract for kinds 40009, 39007,
  and 39008 and the `thread_index` bridge filter extension.
- `NOSTR.md`: add the supported thread-directory operations and event shapes.
- `crates/buzz-core/src/kind.rs`: register the three kinds, make 39007/39008
  relay-only, and pin their range/duplicate invariants.
- `crates/buzz-sdk/src/builders.rs`: add the client update-event builder and
  builder tests.

Relay and database:

- `crates/buzz-db/src/thread.rs`: add the paginated directory query and the
  deterministic latest-state reduction over stored kind:40009 events.
- `crates/buzz-db/src/lib.rs`: expose the scoped directory query through `Db`.
- `crates/buzz-relay/src/handlers/ingest.rs`: scope, validate, and authorize
  kind:40009 before storage; exclude its root reference from reply-count
  mutation.
- `crates/buzz-relay/src/handlers/side_effects.rs`: emit a fresh directory-item
  overlay after an accepted state update and after reply/delete mutations.
- `crates/buzz-relay/src/api/bridge.rs`: implement `thread_index: true` and
  synthesize signed 39007 items plus exactly one 39008 bounds event.

Desktop bridge and shared API:

- `desktop/src-tauri/src/commands/thread_directory.rs` (new): build and submit
  the bridge filter without introducing a new HTTP endpoint.
- `desktop/src-tauri/src/commands/mod.rs` and `desktop/src-tauri/src/lib.rs`:
  register the new command.
- `desktop/src/shared/api/threadDirectory.ts` (new): parse the flat event
  response and publish kind:40009 full-state updates.
- `desktop/src/shared/constants/kinds.ts`: mirror all three kind constants.
- `desktop/src/shared/api/relayClientSession.ts`: subscribe to live 39007
  overlays without treating them as timeline rows.

Desktop state and UI:

- `desktop/src/features/sidebar/lib/threadDirectory.ts` (new): pure directory
  parsing, merge, ordering, title, and unread-state helpers.
- `desktop/src/features/sidebar/lib/threadDirectory.test.mjs` (new): unit tests
  for those helpers.
- `desktop/src/features/sidebar/useThreadDirectory.ts` (new): lazy per-channel
  React Query pagination, archived-mode loading, live overlay merge, and
  metadata mutations.
- `desktop/src/features/sidebar/ui/SidebarThreadList.tsx` (new): nested rows,
  loading/empty/error states, rename dialog, and pin/archive/restore actions.
- `desktop/src/features/sidebar/ui/SidebarSection.tsx`: make a stream channel
  row a disclosure host while preserving DM behavior.
- `desktop/src/features/sidebar/ui/CustomChannelSection.tsx`: use the same
  disclosure host in starred, unassigned, and custom channel sections.
- `desktop/src/testing/e2eBridge.ts`: model the query extension, metadata
  update, restart persistence, and live overlay in mock mode.
- `desktop/tests/e2e/thread-directory.spec.ts` (new) and
  `desktop/playwright.config.ts`: register the end-to-end acceptance scenario.

Do not add feature logic to `desktop/src/features/sidebar/ui/AppSidebar.tsx`.
It is already 993 lines against a 1000-line enforced ceiling. The new behavior
belongs in the extracted files above.

No SQL migration is required. Shared metadata is an append-only signed event;
the directory query reduces the newest non-deleted accepted state for each
root. This keeps rollback simple and preserves an auditable rename history.

# Behavior contract

## Event kinds

`kind:40009` is a stored, client-authored thread-directory state update.

- It carries exactly one `h` tag containing the channel UUID.
- It carries exactly one `e` tag in the form
  `["e", "<root-id>", "", "root"]`.
- Its content is a full JSON snapshot:

  ```json
  {"title":"Shared title or null","pinned":false,"archived":false}
  ```

- `title` is either `null` or a trimmed single-line string of 1-120 Unicode
  scalar values. Control characters and line breaks are rejected. `null`
  clears the override and returns the entry to its generated title.
- `pinned` and `archived` are required booleans. They cannot both be true.
- Unknown or duplicate contract fields/tags are rejected. The event is not a
  timeline row and its root tag must not increment thread counters.
- The relay accepts the event only when the target exists in the same
  community and channel, is a depth-zero root, and has at least one descendant.
- The actor must be an active channel member and either the root's effective
  author, the registered owner of the managed agent that is the effective
  author, or a current channel owner/admin. The relay is authoritative; the UI
  may hide actions but cannot replace this check.
- Current state is the newest non-deleted, authorized kind:40009 for the root,
  ordered by `created_at DESC, id ASC`, matching Buzz's deterministic
  same-second ordering. A deletion exposes the next valid state, providing an
  audit-preserving rollback.

`kind:39007` is a relay-signed directory-item overlay, synthesized from the
root event, `thread_metadata`, and the reduced 40009 state. It is never stored
and clients cannot submit it.

- Tags: `e=<root-id>`, `d=<root-id>`, and `h=<channel-id>`.
- Content:

  ```json
  {
    "title": "Resolved display title",
    "title_override": "Shared title or null",
    "root_author": "<hex pubkey>",
    "root_created_at": 0,
    "reply_count": 0,
    "descendant_count": 0,
    "last_reply_at": 0,
    "participants": ["<hex pubkey>"],
    "pinned": false,
    "archived": false,
    "present": true,
    "state_created_at": 0,
    "state_event_id": "<hex id or null>"
  }
  ```

- `present` is optional and defaults to `true` when absent. `false` means the
  root is no longer a member of this channel's directory in any state: root
  deletion, eligibility disqualification (for example the descendant count
  dropping below the active threshold), or aging out. When `present` is
  `false`, the remaining fields carry best-effort last-known values; clients
  key removal solely on `present` and apply it through the same
  newest-state-wins merge ordering as any other overlay.
- `participants` follows the existing 39005 cap and newest-first order.
- The generated title is deterministic: take the root's first non-empty line,
  trim leading Markdown quote/heading/list markers, collapse whitespace, and
  cap at 80 Unicode scalar values with an ellipsis. Empty content becomes
  `Untitled thread`. Explicit titles are not Markdown-rendered.
- A live 39007 is emitted after a reply insert, reply deletion, root deletion,
  metadata update, or metadata-event deletion. A trigger that leaves the root
  outside both the active and archived membership — including root deletion —
  emits the overlay with `present: false`. Failure to fan out is recoverable
  because the next directory query recomputes the item.

`kind:39008` is a relay-signed directory-bounds overlay. It is synthesized,
never stored, and client submission is rejected.

- Tags: `d=<channel-id>:<active|archived>:<request-cursor-or-head>` and
  `h=<channel-id>`.
- Content is
  `{"has_more":bool,"next_cursor":"<opaque cursor>"|null}`.
- Exactly one bounds overlay is returned. `next_cursor = null` if and only if
  `has_more = false`; clients never infer exhaustion from item count.

## Directory query

The desktop invokes the existing authenticated bridge `/query` with one raw
filter:

```json
{
  "kinds": [39007, 39008],
  "#h": ["<channel-uuid>"],
  "limit": 25,
  "thread_index": true,
  "directory_state": "active",
  "directory_cursor": null
}
```

- `thread_index: true` requires exactly one accessible `#h` channel.
- `directory_state` is exactly `active` or `archived`.
- `directory_cursor` is absent/null for the first page and otherwise the
  opaque cursor returned by 39008. Malformed cursors return HTTP 400.
- The server caps `limit` at 100 and probes `limit + 1` after all predicates.
- Every SQL path constrains `community_id` and `channel_id` before root/event
  matching. Deleted roots and roots with no descendants are excluded.
- Active inclusion is:
  `archived=false AND (title_override IS NOT NULL OR pinned=true OR
  (descendant_count>=3 AND activity_at>=now-30-days))`.
- Archived inclusion is `archived=true`.
- `activity_at` is `COALESCE(last_reply_at, root_event_created_at)`.
- Active order is `(pinned DESC, activity_at DESC, root_event_id ASC)`.
  Archived order is `(activity_at DESC, root_event_id ASC)`.
- The opaque cursor contains every ordering dimension and is validated before
  use, so same-second roots paginate without gaps or duplicates.
- A generic relay ignores the extension and returns no stored 39007/39008
  events. It does not return a plausible but incomplete directory.

## Sidebar and navigation

- Each stream channel row has a separate disclosure control with an accessible
  name. Selecting the row still opens the channel; selecting the disclosure
  toggles the nested list and must not begin a drag operation.
- Data is fetched only when a channel's disclosure opens. The selected channel
  may remember its disclosure state for the current session, but expansion is
  not shared metadata.
- The initial page renders pinned items first and active items second. A
  `More threads` row fetches the next cursor page. `Archived threads` switches
  to the archived query; restore returns the item to the active query.
- Each row shows the resolved title, last activity, and unread state. The
  existing NIP-RS thread marker supplies the unread decision. An exact count is
  shown only when the existing loaded per-thread counter can prove it;
  otherwise the UI shows a dot and never fabricates a count.
- Selecting an item calls the existing channel navigation with
  `{messageId: rootId, threadRootId: rootId}`. The root may be outside the
  current channel window; existing route-target hydration must fetch it by ID
  and open the thread panel.
- Rename/pin/archive/restore publish a complete 40009 snapshot. The UI updates
  optimistically, rolls back on relay rejection, and displays the relay error.
- Archived state is nondestructive. It never deletes the root or replies.
- Reconnect, community switch, and identity switch discard query state from
  the old scope. No module-level community cache may survive
  `resetCommunityState()`.

# Test list

Protocol/core tests:

- All three kinds are registered exactly once; 39007 and 39008 are relay-only;
  40009 is regular stored and channel-scoped.
- The SDK builder emits exactly one `h`, one root-marked `e`, and the canonical
  full JSON snapshot.

Relay ingest tests:

- Reject missing/duplicate/malformed `h` or `e`, unknown root, cross-channel
  root, non-root target, zero-descendant target, invalid JSON, extra fields,
  multiline/control/overlong title, non-boolean flags, and pinned+archived.
- Accept root author, owning human of an agent-authored root, owner, and admin.
  Reject an ordinary member, removed member, and unrelated agent owner.
- Prove a 40009 event never changes `reply_count` or `descendant_count`.
- Reject client-authored 39007 and 39008.

Database/bridge tests:

- Reduce competing authorized updates by `created_at DESC, id ASC` across
  different authors and ignore deleted updates.
- Scope every result by community and channel; a same-ID/root-shaped event in
  another community cannot affect the result.
- Enforce active threshold, 30-day aging, renamed persistence, pin precedence,
  archive exclusion, and restore behavior.
- Paginate pinned and unpinned same-second roots without gaps or duplicates.
- Emit one valid 39007 per item and exactly one 39008 bounds event, including
  the exact-multiple final-page case.
- Emit refreshed live 39007 state after reply, reply deletion, root deletion,
  update, and update deletion.
- Emit `present: false` when a trigger removes the root from directory
  membership: root deletion, and a reply deletion that drops the descendant
  count below the active threshold.

Desktop unit/component tests:

- Parse valid overlays and reject wrong kind, missing tags, channel mismatch,
  malformed JSON, invalid counts/timestamps, and mismatched bounds.
- Merge live items by state/activity ordering without reviving an item deleted
  by a newer page.
- Remove an item from every view on `present: false`, treat an absent
  `present` field as `true`, and ignore a stale `present: false` older than
  the item's current state.
- Keep relay/pubkey/channel query keys isolated across community and identity
  switches.
- Render disclosure, loading, empty, error, active, pinned, and archived states
  with keyboard-operable controls and no nested interactive HTML.
- Verify row selection navigates with both root route fields; disclosure and
  thread actions do not select or drag the channel.
- Verify unauthorized mutation failures restore the prior row and surface an
  error.

End-to-end acceptance test:

1. Begin with only a channel ID; do not seed or remember a root ID in client
   state.
2. Create a root and enough replies to make it automatically active.
3. Expand the channel, discover the generated thread entry, and open it.
4. Rename and pin it, restart the app/mock bridge, expand from the channel
   again, and verify the shared title and pin survive.
5. Archive it, verify it leaves Active, find it under Archived, open the full
   reply history, restore it, and verify it returns to Active.
6. Capture a cropped sidebar screenshot only after `waitForAnimations`; the
   test must run through `pnpm build:e2e`/the registered E2E scripts.

Required gates after the Windows toolchain is installed:

- Activate Hermit before repository commands.
- Run the complete package suites for every touched package, not scoped test
  files only.
- Run `just test` because `buzz-db` and `buzz-relay` are touched.
- Run `just ci` before presenting the branch as ready.
- Run the registered desktop E2E suite and the thread-directory spec.
- Confirm `git rev-parse HEAD` in the same shell as each reported validation.

# Safety implications

- Shared state changes are signed, auditable, channel-scoped, and authorized
  by the relay. Client-side hiding is not an authorization boundary.
- The directory cannot trust `threadActivityStorage.ts` for completeness or
  scope; doing so would leak stale cross-community state and omit quiet shared
  threads.
- Relay-only overlays must be rejected on ingest so a member cannot forge
  counts, titles, archive state, or pagination bounds.
- Every root lookup and state reduction includes `community_id` and
  `channel_id`. The target root must be fetched including its stored channel,
  then compared with the signed `h` tag before authorization.
- Explicit title text renders as plain text. It is never interpreted as HTML or
  Markdown and is bounded before storage to prevent sidebar abuse.
- The append-only update event avoids a migration and preserves prior states.
  Deleting an update intentionally rolls back to the next valid event; tests
  pin this behavior.
- Live fan-out is advisory. Page/query recomputation is authoritative after
  reconnect, preventing a dropped Redis/local push from becoming permanent
  state divergence.
- Unread UI must degrade to a dot when exact data is unavailable. A guessed
  count is a correctness bug.
- Installing Rust, pnpm, and Microsoft C++ Build Tools is a separate,
  user-approved machine change owned by the project lead; this spec neither
  installs nor modifies that toolchain.

# Out of scope / do NOT touch

- Do not change message bodies, NIP-10 ancestry, reply counter semantics, or
  the existing 39005/39006 channel-window contract.
- Do not add a bespoke HTTP endpoint; use the existing signed-event bridge.
- Do not use local storage as the authoritative directory or title store.
- Do not add private per-user thread aliases in v1.
- Do not add thread notifications, workflow triggers, assignments, due dates,
  canvases, or project-management semantics.
- Do not implement AI-generated titles. Generated titles are deterministic
  projections of the root content.
- Do not change forum-post or DM navigation.
- Do not implement mobile or web UI in this lane.
- Do not refactor unrelated sidebar sorting, channel sections, drag-and-drop,
  read-state, or timeline rendering.
- Do not add feature logic to `AppSidebar.tsx` or raise any file-size ceiling.
- Do not commit local configuration, credentials, build artifacts, or toolchain
  files.
- Do not push, open a PR, or install machine dependencies without the existing
  project-lead/user approval gates.

# Rollback path

The three feature commits remain separable: protocol/relay directory,
client mutation/read model, and sidebar UI/E2E.

1. Disable or remove the sidebar disclosure and live 39007 subscription. The
   existing channel timeline and thread panel continue to work unchanged.
2. Remove the `thread_index` bridge handler and stop accepting new 40009
   updates. Existing stored 40009 events are inert, non-timeline signed audit
   records and may remain in the event store.
3. Remove the new kind registrations only after every deployed client and relay
   has stopped using them. No SQL rollback or destructive data rewrite is
   required.
4. If upstream accepts only part of the feature, retain the protocol/relay
   commit independently and drop the sidebar commit, or vice versa only when a
   compatible relay API remains available.

Rollback must not delete roots, replies, or historical 40009 events. Restoring
the pre-feature branch is sufficient to restore pre-feature behavior.
