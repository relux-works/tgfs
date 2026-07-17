# TASK-260715-1c8fea — Review verdict: ACCEPTED

Reviewer: [reviewer] reviewer (claude). Read-only review; no code modified.

## Verdict
**Accepted → `done`.** Implementation matches the AC, fits the crate
architecture, and all gates are green (independently re-run, not trusted from
the implementer log).

## Acceptance criteria — verified against the code and tests
- **Replay fixtures converge.** Last-write-wins over the machine's known state +
  no-op coalescing (a value equal to the known one emits nothing) + idempotent
  state upserts. Proven by `duplicate_and_out_of_order_updates_converge`,
  `independent_updates_converge_regardless_of_order`,
  `duplicate_position_and_title_coalesce_to_noop`. Confirmed a re-observation of
  current state is a fixed point (`replay.is_empty()`, `written == 0`).
- **Reorder does not change canonical ID.** A position/pin change never marks
  the chat's metadata dirty → `chats` excludes it → `upsert_chat` is not called
  → row + `metadata_version` byte-identical. `reorder_keeps_canonical_row_and_
  version_and_regenerates_order_only` asserts `written == 0` and `after ==
  before`; unit `reorder_emits_order_only_and_never_metadata`. Invalidation is
  `ListOrdering` only.
- **Gap/restart passes.** Unknown-chat updates → `UpdateBatch::unresolved`, no
  forged canonical row, membership held back by the `chat_list_entries → chats`
  FK; resolved by feeding the `getChat` object back as `updateNewChat`
  (`an_update_before_its_chat_is_a_gap_then_resolves`). Restart converges with
  zero churn (`a_restart_re_pushes_current_state_and_converges_without_churn`,
  `written == 0`, versions stable).

## DoD — verified
- Metadata updates (title/photo/position/pin/protection/membership) applied
  incrementally; ordering stays POL-1-consistent (read sorts `pinned DESC,
  sort_order DESC`). Rename → `Invalidation::FolderName`; reorder →
  `Invalidation::ListOrdering` (order.json regen only); first-sight/avatar/
  protection → `Invalidation::Metadata`.
- Out-of-order + duplicate handling proven by scripted tests (above).
- **`make check` 8/8 green — re-run independently:** toolchain, format, lint
  (`clippy --all-targets --all-features -D warnings`), test (workspace/all
  features), architecture, supply-chain (`cargo deny`), traceability, scripts.

## Architecture fit
- Sans-IO reducer in `gramdrive-source-tdjson`; product code depends only on
  `gramdrive-model`; `gramdrive-state` stays a `[dev-dependencies]` entry used
  by the integration suite. Mirrors the `SnapshotMachine` precedent exactly —
  one normalized projection, no second disagreeing one (SYNC-026).
- Deterministic output ordering (`BTreeSet`/`BTreeMap` + explicit `list_sort_key`
  sort) so a drained batch is reproducible.
- New state methods `upsert_chat_list_entry` / `remove_chat_list_entry`:
  `ON CONFLICT (account_id, namespace_version, list_kind, folder_id, chat_id)`
  matches the actual `chat_list_entries` PRIMARY KEY; FK to `chats` enforced
  (the state test asserts an unknown-chat membership is `is_err()`); both
  idempotent. Whole-list `replace_chat_list` would wipe a list under a partial
  in-memory model — the incremental methods are the correct call.
- Shared wire parsers (`parse_order`/`parse_list`/`active_username`/
  `parse_chat_kind`/`KindFact`) extracted to `pub(crate) src/wire.rs`; snapshot
  re-points and its moved unit tests travel with them. Removes the int64-string
  `order` drift risk between the two machines. No snapshot API change.

## Informational notes for the next producer (non-blocking, no rework)
- When a chat both renames and changes photo/protection in one batch, only
  `FolderName` is emitted (not also `Metadata`). By design: the full
  `ChatMetadata` still lands in `chats`, so the caller's content-derived
  `metadata_version` advances from all facts; `FolderName` is the superset
  signal. Downstream consumers should treat `FolderName` as implying a metadata
  refresh, not a disjoint work item.
- `photo_token` falls back to `Value::to_string()` when `small.remote.unique_id`
  is absent — deterministic under serde_json's default sorted-key map; only
  compared against the identical incoming shape, so no practical churn.
- Empty per-chat `positions` maps linger after a chat leaves all lists —
  negligible, not a correctness concern.
