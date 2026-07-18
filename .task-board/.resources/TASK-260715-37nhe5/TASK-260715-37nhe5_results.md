# TASK-260715-37nhe5 — Edit and deletion policy mapping (POL-3 / DEC-015)

Status: ready for review.

## What the task needed

Map observed edits and deletions into the append-only message event log while
honoring the per-account POL-3 retention mode: **Mirror** purges content on an
observed delete and replaces prior revisions on edit; **Audit** keeps every
revision and a content-preserving tombstone. Render invalidation is
deterministic via watermarks. No recovery claims for unseen revisions/deletions;
cache eviction stays a separate axis.

## Starting point (already in the tree before this task)

- Schema (`schema/v1.sql`): `message_events` append-only log with the trigger
  that permits exactly one kind of UPDATE — a payload purge (`payload` and
  `payload_schema` → NULL together); `messages` projection with `is_deleted` and
  `latest_event_seq`; `render_state` with `input_watermark_seq` + `dirty`.
- `apply_message_changes` (`repo/changes.rs`) appended observed/edited/deleted
  events and maintained the projection, with full SYNC-021 idempotence (replay,
  no-resurrect, stale-edit skips) — but was **retention-agnostic**: it always
  kept every payload. Retention only affected rendering (gramdrive-render
  projects Mirror/Audit at render time).
- Watermark render-invalidation protocol (`repo/render.rs`,
  `engine/render_plan`) was complete: `mark_render_dirty` + `dirty_affected` +
  `publish_render`'s in-transaction race re-check.

The gap this task closes is the **storage-level content purge** POL-3 mandates
for privacy — the schema even documents "the single sanctioned UPDATE is the
Mirror-mode content purge", but nothing performed it.

## Changes

### `repo/changes.rs` — retention-aware appliers
- `apply_message_changes` now reads `accounts.retention_mode` **once per batch**
  (via the stored column, never a caller value) and threads it into the
  appliers. Missing account → `RowNotFound` (the chat FK already guarantees it
  exists in practice).
- On **edit** in Mirror: after appending the new revision, purge every *prior*
  revision payload of that message, keeping only the just-appended current one
  ("edits replace prior revisions").
- On **delete** in Mirror: after appending the content-free tombstone, purge
  *all* of the message's revision payloads (the message keeps no content).
- Audit: no purge — everything retained.
- New private `purge_message_content(chat, message_id, keep)` — the payload-NULL
  UPDATE, guarded by `payload IS NOT NULL` so it is idempotent and its
  changed-count is the content actually removed. Event **rows** are never
  removed, so `event_seq` watermarks never rewind and replay stays recognizable.
  The current revision of a live message always keeps its payload (it is the
  projection join target and the replay-check comparand), so idempotence is
  preserved in both modes.

### `repo/accounts.rs` — purge-aware mid-life switch
- `ReadTxn::retention_mode(account)` — lean read used by the appliers.
- `WriteTxn::set_retention_mode(account, mode, updated_at_ms) -> RetentionChange`
  — the only mid-life mutator. In one transaction: writes the column;
  **Audit→Mirror** runs the retroactive sweep (purge every event payload that is
  not the current revision of a live message — deleted messages' rows are all
  superseded and go); marks **every** generated document of the account dirty
  (both directions, because the retention mode is stamped in each document's
  header, so any switch changes the bytes). Recovers nothing already purged.
- `upsert_account` no longer updates `retention_mode` on conflict — a silent
  column flip would leave purged-but-still-rendered content. Insert still honors
  the record's mode (account setup).
- New `RetentionChange { previous, current, purged_events, invalidated_docs }`
  with `changed()`. Exported from `repo/mod.rs`.

## delete-for-everyone vs delete-for-me

TDLib's `updateDeleteMessages` carries no revoke-scope flag; the source layer
counts a deletion only when `is_permanent && !from_cache`
(`gramdrive-source-tdjson::live`). The archive mirrors *this account's* view, in
which both a revoke and a delete-for-me are permanent removals. So both
normalize to one `MessageChange::Deleted` and map through the identical path —
Mirror purges, Audit tombstones-with-content — and the archive claims nothing
about which scope it was. Pinned by
`observed_deletion_maps_identically_whatever_telegram_delete_scope_was`.

## Tests — `tests/repo_retention.rs` (8, all green)

- `audit_retains_every_revision_and_a_deleted_message_content`
- `mirror_edit_chain_keeps_only_the_current_revision` (+ stale re-observation
  still caught)
- `mirror_deletion_purges_all_of_a_messages_content`
- `observed_deletion_maps_identically_whatever_telegram_delete_scope_was`
- `switching_to_mirror_purges_retained_history_and_invalidates_documents`
  (asserts exact `purged_events`, per-message content, and doc dirty flip)
- `switching_to_audit_recovers_nothing_but_invalidates_and_retains_forward`
- `setting_the_same_mode_is_a_noop`
- `setting_retention_for_an_unconfigured_account_is_reported`

All pre-existing suites (state/engine/tdjson — which apply changes under the
default Mirror account) stay green; none asserted prior-revision payload
presence, so the new purge does not regress them.

## Scope boundaries honored

- **Cache eviction stays separate**: `set_retention_mode` touches only
  `accounts`, `message_events`, `render_state`. Attachment bytes/blobs are POL-2
  LRU accounting on a different axis and are untouched here.
- **No recovery**: Mirror→Audit purges/recovers nothing; already-purged content
  stays gone; history predating first sync is never invented.

## Gates

`make check-core` 6/6 and `make check-repo` 2/2 green (toolchain, format, lint
`-D warnings`, `cargo test --workspace --all-features`, architecture,
`cargo deny check`, traceability, script self-tests).
