# TASK-260715-1c8fea — Chat metadata & list update mapping — results

Status: ready for review (`to-review`).

## What was built
A deterministic sans-IO `UpdateMachine` (`crates/gramdrive-source-tdjson/src/updates.rs`)
that folds TDLib's live push updates into the same provider-neutral normalized
change stream the initial snapshot commits in — keeping the `gramdrive-state`
baseline current without a second, disagreeing projection (SYNC-026).

It is a pure reducer: `on_update(&Value)` folds updates, `take_batch()` drains a
deterministic `UpdateBatch { chats, memberships, invalidations, unresolved }`
the composing caller applies to state in one transaction (canonical `chats`
first for the `chat_list_entries → chats` FK, memberships after).

## Update mapping
`updateNewChat`, `updateChatTitle`, `updateChatPhoto`, `updateChatPosition`,
`updateChatRemovedFromList`, `updateChatHasProtectedContent`, and the
`updateUser`/`updateSupergroup` username feed → `ChatMetadata` upserts +
`MembershipChange::{Set,Removed}` deltas. Secret/unknown chat types are
excluded and never mistaken for gaps (POL-4/DEC-016).

## Acceptance criteria → evidence
- **Replay fixtures converge.** Last-write-wins + no-op coalescing + idempotent
  state upserts. `duplicate_and_out_of_order_updates_converge`,
  `a_restart_re_pushes_current_state_and_converges_without_churn` (0 rows
  rewritten, metadata versions do not churn), unit `independent_updates_converge_regardless_of_order`,
  `duplicate_position_and_title_coalesce_to_noop`.
- **Reorder does not change canonical ID.** A position change never marks the
  chat's metadata dirty → no `upsert_chat` → row + version byte-identical.
  `reorder_keeps_canonical_row_and_version_and_regenerates_order_only`
  (asserts `written == 0` and the ChatRecord is unchanged), unit
  `reorder_emits_order_only_and_never_metadata`.
- **Gap/restart behavior passes.** Unknown-chat updates → `unresolved`, no
  forged row; resolved by feeding the `getChat` object back.
  `an_update_before_its_chat_is_a_gap_then_resolves`, unit
  `an_update_about_an_unknown_chat_is_a_gap_then_resolves`.

## DoD checklist → evidence
- **Metadata updates applied incrementally; ordering consistent with POL-1.**
  `baseline_and_live_deltas_apply_into_state` reads exact presentation order
  from the store (`pinned DESC, sort_order DESC`).
- **Rename → folder rename event; reorder → order.json regen only.** POL-1
  invalidation split: `FolderName` (rename), `ListOrdering` (reorder, driven by
  `order.json` being keyed by list and name-embedding — reorder is content-only),
  `Metadata` (first sight/avatar/protection). Asserted in
  `baseline_and_live_deltas_apply_into_state` and the reorder suite.
- **Out-of-order and duplicate handling proven by scripted tests.** See above.

## State-layer additions
`WriteTxn::upsert_chat_list_entry` / `remove_chat_list_entry`
(`crates/gramdrive-state/src/repo/chats.rs`) — incremental, idempotent,
FK-enforced. Whole-list `replace_chat_list` would wipe a list under a partial
in-memory model. Covered by `chat_list_entries_apply_incrementally_and_idempotently`
(`tests/repo_snapshots.rs`).

## Refactor
Shared wire parsers (`parse_order`/`parse_list`/`active_username`/`parse_chat_kind`/
`KindFact`) extracted from `snapshot.rs` into `pub(crate) src/wire.rs` so the two
machines cannot drift on the subtle int64-string order parse. Snapshot public API
and its integration suite unchanged.

## Quality gates
`make check` → 8/8 green (toolchain, format, lint `-D warnings`, test 45s,
architecture, supply-chain, traceability, scripts). No new dependencies; no
cross-crate dependency direction changed (state stays a dev-dependency).

## Boundaries / handoff notes
- No live server-side resume token: the stream has no offset (as `loadChats`
  has none). Durability = idempotent convergence + snapshot re-baseline on a gap
  (SYNC-023). The "transactional checkpoint" is the atomic per-batch apply.
- Avatar is not persisted anywhere in state/model/render; `ChatMetadata.photo`
  is an opaque token that feeds only the caller's content-derived
  `metadata_version` (advances DOM-003 without a column).
- POL-3 tombstone (left/deleted) is the engine's retention decision; this layer
  reports the observable (a chat left a list) and leaves the canonical row.
- Composing-caller contract: pump `UpdateStream` → `on_update`; per checkpoint
  apply `UpdateBatch` (chats then memberships) + advance `metadata_version` only
  on real content change, all in one `WriteTxn`; on `unresolved`, `getChat` and
  feed back as `updateNewChat`, or re-baseline.
