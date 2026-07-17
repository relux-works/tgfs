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
(`PRAGMA user_version` 0 → 1), migrates an older file forward, recognizes a
current file, and refuses — with a named `StateError` category — a file from
a newer build (NFR-041). Every table is STRICT; foreign keys are enforced on
every connection; file databases run in WAL with `synchronous=NORMAL`.

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

## Migrations

Forward-only (NFR-013). `v1.sql` creates version 1 and is frozen; every
version after it is a `Migration` in `src/migrate.rs`, applied in order by
`StateStore::open`. There is no downgrade: an older build meeting a newer
file refuses it rather than guessing what the newer schema's data means in
an older shape. `MIGRATIONS` is empty today — `SCHEMA_VERSION` is still the
baseline — and a const assertion fails the build if that list and the
version ever disagree.

Crash safety (SYNC-072) rests on one rule: `PRAGMA user_version` advances
only in the same transaction as the work that earns it, so the version is
never a claim the data cannot back.

| Step | For | On interruption |
|---|---|---|
| `MigrationStep::Sql` | DDL and bounded data work — one transaction | Rolls back whole; the next open starts it over |
| `MigrationStep::Resumable` | Data too large for one transaction (a backfill across 110k messages) | Committed chunks stay; `migration_progress` holds the last checkpoint and the next open resumes from it |

A resumable step commits each chunk's data changes together with the
checkpoint it resumes from, so the pair can never disagree. Its `prepare`
DDL runs in the same transaction as the first chunk's commit — applied once,
and rolled back with it if that commit never happens.

Repair markers (`repair_markers`, `StateStore::repair_markers`) are the
handoff to reconciliation: a migration that changes the shape of a
rebuildable projection raises `rebuild_projection` instead of rebuilding
100k rows inside a schema upgrade (SYNC-071, NFR-034), and the runner raises
`migration_interrupted` for as long as a migration has an uncommitted tail.
Both live in `src/schema/journal.sql`, outside the numbered schema, because
a file written before the runner existed has no journal and still has to be
migratable.

### Adding one

1. Write `src/schema/vN.sql` (or a chunk function) and add the `Migration`
   to `MIGRATIONS`; bump `SCHEMA_VERSION` — the build fails if you do one
   without the other.
2. Add `fixtures/v{N-1}_seed.sql`: representative rows of the schema it
   migrates *from*. A unit test fails until it exists — a migration tested
   only against a database the current build created has never met the
   schema it exists for.
3. Add an interruption test: interrupt it, reopen, resume, and assert the
   result is what an uninterrupted run produces.

## Evidence

- `tests/schema_invariants.rs` — FKs, uniques, CHECKs, the append-only
  trigger, and cascades proven against the real database.
- `tests/query_plans.rs` — every required query path EXPLAIN-verified as
  index-driven (no bare table scans, no temp sorts) on the synthetic large
  account from `gramdrive-testkit` (2,048 chats, 110k messages). Set
  `GRAMDRIVE_PLAN_EVIDENCE=/path/file.md` to capture the plans.
- `tests/store_lifecycle.rs` — WAL, idempotent reopen, version-skew
  refusal.
- `src/migrate.rs` unit tests — the runner against the `fixtures/v1_seed.sql`
  database, with a migration of the shape this exists for (an `ALTER TABLE`
  plus a chunked backfill): interruption, resume from the durable
  checkpoint, twice-interrupted equals never-interrupted, stall and gap
  refusals. Verified by mutation: stamping the version early, re-running the
  preamble, or dropping the interruption marker each fail this suite.
- `tests/migrations.rs` — the public surface: a v1 file opens untouched, a
  journal-less file (one written before the runner existed) gets a journal,
  a file from the future is refused *without being written to*, and repair
  markers round-trip.

## Test command

```sh
cargo test -p gramdrive-state
```
