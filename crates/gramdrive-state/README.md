# gramdrive-state

Durable local metadata: the versioned SQLite schema and its application,
repositories over items/messages/cursors/pins, startup reconciliation.
Short transactions, multi-process safe — on Apple platforms the app and the
File Provider extension share this database in WAL mode, so no in-memory
state is authoritative.

## Ownership

STORY-260715-16ik2x (metadata-state-store), EPIC-260715-1poogc
(shared-rust-core). Populated by TASK-260715-1ceq7h (schema),
TASK-260715-18l9xz (migrations), TASK-260715-1opnb2 (repositories),
TASK-260715-21clwh (reconciliation).

## Dependencies

Internal: `gramdrive-model`. External: `rusqlite` with the bundled SQLite
amalgamation (version rationale in the workspace `Cargo.toml`).
Platform-specific code: forbidden — the database location is chosen by the
embedding host. See `crates/README.md`.

## The schema (v1)

`src/schema/v1.sql` is the schema and carries the full rationale per table;
this is the map. `StateStore::open` applies it atomically to a fresh file
(`PRAGMA user_version` 0 → 1), recognizes a current file, and refuses —
with a named `StateError` category — files from a newer build or files
needing a migration this build cannot run (NFR-041). Every table is STRICT;
foreign keys are enforced on every connection; file databases run in WAL
with `synchronous=NORMAL`.

| Area | Tables | What holds it together |
|---|---|---|
| Accounts | `accounts` | Per-account POL-3 retention mode, POL-2 archive-mode toggle, current namespace epoch (DOM-021); secrets are references, never material |
| Canonical chat facts | `chats`, `chat_list_entries` | Keyed by (account, namespace, chat id); list membership and exact Telegram order live apart from presentation, and POL-1's `order.json` regenerates from them without a scan |
| The message log | `message_events`, `messages` | The append-only canonical store (POL-3, DEC-015): appends only, enforced by trigger — the one sanctioned update is the Mirror-mode payload purge; `messages` is the current-state projection, whose FK refuses to purge the event backing it. `AUTOINCREMENT` keeps sequence numbers watermark-safe forever |
| Attachments and bytes | `attachments`, `blobs` | Attachment identity is (chat, message, ordinal); Telegram locators are refreshable metadata, never identity (DOM-007, SYNC-045); blobs are content-addressed per account and linked only after verification |
| Provider projection | `items` | Every provider-visible node under its stable binary `ItemId` (DEC-008): canonical structural roots and appearance rows in one table (DOM-002/022), a real parent self-FK for the tree, live-sibling name uniqueness (SYNC-012), one appearance per (canonical, view) |
| Hydration | `transfers` | Durable transfer journal pinned to a content version (SYNC-042), JSON-validated ranges, the SYNC-044 failure taxonomy, a partial index over live states for the queue head |
| Cache | `cache_entries`, `pins` | POL-2: LRU eviction scans a partial index that pinned/unverified content never enters; `pins` is durable offline intent independent of materialization |
| Sync positions | `change_cursors`, `chat_sync_state` | One durable feed position per (account, stream) — scope verification stays with `ChangeCursor::require_scope` (SYNC-004); per-chat resumable history windows (SYNC-021) |
| Rendering | `render_state` | Renderer/schema versions, the event-sequence input watermark, and a dirty worklist behind a covering partial index (SYNC-024, SYNC-030..033) |
| Versioning | `schema_history` + `user_version` | The pragma answers "what is current"; the table answers "how did we get here" for the migration runner (SYNC-072) |

## Evidence

- `tests/schema_invariants.rs` — FKs, uniques, CHECKs, the append-only
  trigger, and cascades proven against the real database.
- `tests/query_plans.rs` — every required query path EXPLAIN-verified as
  index-driven (no bare table scans, no temp sorts) on the synthetic large
  account from `gramdrive-testkit` (2,048 chats, 110k messages). Set
  `GRAMDRIVE_PLAN_EVIDENCE=/path/file.md` to capture the plans.
- `tests/store_lifecycle.rs` — WAL, idempotent reopen, version-skew
  refusal.

## Test command

```sh
cargo test -p gramdrive-state
```
