# TASK-260715-1opnb2 — Transactional state repositories: implementation notes

Status: implementation ready for review. All 8 quality gates green
(`make check`, suite `all`, run-id `local-all`).

## What was built

A typed repository layer in `crates/gramdrive-state/src/repo/` — the
sanctioned surface other crates use over the v1 schema. No SQL, no
`rusqlite` type, and no stored encoding (enum strings, range JSON, cursor
text) crosses the boundary; callers speak `gramdrive-model` vocabulary plus
record types.

### Transaction model (short transactions, cancellation boundaries)

- `StateStore::read_txn()` → `ReadTxn` — a WAL read snapshot (deferred
  transaction). Consistent for its lifetime; never blocks the other
  process's writer.
- `StateStore::write_txn()` → `WriteTxn` — short `BEGIN IMMEDIATE` write
  transaction. Write lock taken up front (no mid-transaction lock-upgrade
  failures); `commit()` is explicit; **drop is rollback** — the crate's
  cancellation boundary.
- Both take `&mut self`: one connection carries one transaction at a time;
  in-process/multi-process concurrency is separate `StateStore` instances
  over one file, which is exactly the app + File Provider extension shape.
- SYNC-022 atomicity is compositional: `apply_message_changes` +
  `record_chat_sync` + `put_cursor` under one `WriteTxn` commit or vanish
  together. No special "apply with cursor" API needed.

### Modules and key invariants

| Module | Surface | Invariant enforced in the layer |
|---|---|---|
| `accounts` | upsert/read/list, `bump_namespace`, `current_scope` | upsert never rewinds the namespace epoch or creation time; only `bump_namespace` moves the epoch, forward (DOM-021) |
| `chats` | `upsert_chat`, `replace_chat_list`, ordered `chat_list` | list replaced whole per snapshot (DEC-013); read order pinned-first then sort_order DESC (POL-1); Folder(0) sentinel rejected |
| `items` | upsert/read, `children_page`, `child_by_name`, `appearances_of`, `tombstone_item`, `update_item_content` | identity columns (kind, scope, canonical link, view) **derived from the ItemId itself** — callers cannot write them inconsistently; account-root epoch read from the account row; content update is CAS on `ContentVersion` (DOM-003); structure violations are typed `InvalidArgument`, not CHECK failures |
| `changes` | `apply_message_changes`, event/message/window reads, `record_chat_sync`, `backfill_backlog` | idempotent by Telegram identity (SYNC-021): exact replays, post-deletion revisions, never-observed deletions, and **stale pre-edit revisions** all skip; edits append `edited` events; tombstones carry no payload (POL-3) |
| `cursors` | `put_cursor`, `cursor`, `clear_cursor` | scope checked both ways against the account's *current* epoch — retired-epoch cursors are explicit `CursorOutOfScope` (SYNC-004); corrupt stored text is `CursorCorrupt`, never a silent None |
| `attachments` | `upsert_attachment`, `record_blob`, `link_attachment_blob`, blob back-references | locator refresh (SYNC-045) rewrites metadata only, never detaches a verified blob link; links require the blob row first |
| `transfers` | enqueue/claim/progress/suspend/resume/fail/cancel/done | coalescing per (item, content_version) (SYNC-046); claim skips backoff and cancel-requested rows; two-phase cancel (durable flag → boundary ack); `mark_transfer_done` re-checks the item's current content version inside the promoting transaction (SYNC-042) — conflict leaves the journal untouched |
| `cache` | entry upsert/read/touch/verify/pin-fold/evict/usage; `pin_item`/`unpin_item`/`pins` | eviction eligibility (unpinned + verified) enforced **in the DELETE itself** (SYNC-051/052); accounting via the covering index (SYNC-050); user pin over archive pin upgrades origin, keeps creation time |
| `render` | `ensure_render_state`, `mark_render_dirty`, `dirty_render_items`, `publish_render` | watermarks only advance (`WatermarkRegression` otherwise); publish re-checks the chat's event log inside its transaction and stays dirty if events arrived while rendering (SYNC-024); renderer/schema version change re-dirties without discarding published facts (SYNC-030) |
| `ranges` | internal `[start,end)` JSON codec | hand-rolled strict codec (no serde dependency — POL-6 surface); malformed stored text is `CorruptRow` |

### Error vocabulary added to `StateError`

`InvalidArgument`, `RowNotFound`, `VersionConflict {entity, expected,
found}`, `WatermarkRegression {current, proposed}`, `CursorOutOfScope`,
`CursorCorrupt`, `CorruptRow {table, detail}`, `InvalidTransition {entity,
from}`. Unknown enum text or undecodable identity on read is reported as
corruption, never skipped or coerced.

### Design decisions worth reviewer attention

1. **Stale-revision guard** (`changes.rs`): a revision whose effective
   revision time (`edited_at_ms` else `sent_at_ms`) is older than the
   projected one is skipped as replay. Rationale: a history page fetched
   before an edit and replayed after the edit's change-feed application
   must not rewind current state. Relies on Telegram edit times being
   per-message monotonic.
2. **Deletion of a never-observed message is a skip** (POL-3: history
   never observed is never implied) — no fabricated projection row.
3. **`upsert_account` ignores `namespace_version` and `created_at_ms` on
   the update path** — epoch moves only through `bump_namespace`.
4. **`publish_render` takes the source `ChatKey`** so the repo can check
   "events beyond watermark" itself; the alternative (trusting the caller
   to re-check) would leave the SYNC-024 race open by default.
5. **Statement cache bumped to 64** in `StateStore::configure` — the repo
   layer's distinct statements outnumber rusqlite's default 16.

## Evidence (AC mapping)

- **Atomic cursor application** — `repo_changes.rs::cursor_commits_atomically_with_applied_changes`,
  `a_failed_cursor_write_rolls_back_the_whole_batch` (failure-path
  rollback of the whole batch), and
  `repo_concurrency.rs::a_reader_never_observes_a_cursor_ahead_of_its_state`
  (invariant held under a concurrent reader across 20 batches).
- **Idempotent replay** — `replaying_a_batch_applies_nothing`,
  `edits_append_new_revisions_and_stale_revisions_are_skipped`,
  `deletions_tombstone_and_never_imply_or_resurrect`.
- **Version conflict** — `item_content_updates_are_compare_and_set`,
  `promotion_rechecks_the_content_version_it_pinned` (SYNC-042),
  `publication_never_regresses_and_never_hides_late_events` (watermark),
  `cursors_reject_foreign_and_retired_scopes_explicitly` (epoch).
- **Concurrent readers/writers (WAL)** — `repo_concurrency.rs`: stable
  read snapshot across a foreign commit; two contending writers never
  double-claim a transfer (IMMEDIATE + busy handler); cursor-vs-state
  invariant under concurrent read load. Two connections in one process
  exercise the same file-based locking two processes would.
- **Multi-process safety documented** — new "Repositories" and
  "Multi-process safety" sections in `crates/gramdrive-state/README.md`.

## Verification run

- `cargo test --workspace` — all green (36 new repo tests: 10 changes,
  11 snapshots, 6 transfers, 6 cache/render, 3 concurrency; plus 4 range
  codec unit tests).
- `make check` — 8/8 (toolchain, format, lint, test, architecture,
  supply-chain, traceability, scripts). Provenance:
  `.temp/acceptance/local-all`.

## Files

- New: `crates/gramdrive-state/src/repo/{mod,accounts,chats,items,changes,cursors,attachments,transfers,cache,render,ranges}.rs`
- New tests: `crates/gramdrive-state/tests/repo_{changes,snapshots,transfers,cache_render,concurrency}.rs`
- Modified: `src/error.rs` (repo error variants), `src/lib.rs` (exports),
  `src/store.rs` (statement cache), `tests/common/mod.rs` (typed
  fixtures + TempDb), `README.md` (repositories + multi-process sections)

## Out of scope (owned by sibling tasks)

- Startup reconciliation and epoch sweeps — TASK-260715-21clwh.
- Range merging policy, retry backoff policy, pin subtree expansion —
  engine semantics (STORY-260715-2hs8cf); the journal stores, the engine
  decides.
- Mirror-mode payload purge operation — retention enforcement flow, not a
  repository primitive of this task (the schema's sanctioned UPDATE path
  remains available to it).
