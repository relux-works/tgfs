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
TASK-260715-21clwh (reconciliation), TASK-260721-1hm7dx (schema-v4
date-first content contract), TASK-260721-yrcjlo (schema-v5 chat-content
progress and lifecycle composition), TASK-260721-1dzolg (schema-v6 render
policy generation), TASK-260721-ddqgxa (schema-v7 TDLib attachment locator
and durable attachment projection), and TASK-260721-3e9bi8 (schema-v8
canonical story lifecycle and resumable discovery), plus TASK-260721-2tamdj
(transactional retention and Archive-Mode ownership).

## Dependencies

Internal: `gramdrive-model`. External: `rusqlite` with the bundled SQLite
amalgamation (version rationale in the workspace `Cargo.toml`).
Platform-specific code: forbidden — the database location is chosen by the
embedding host. See `crates/README.md`.

## The schema (v23)

`src/schema/v1.sql` is the baseline; v2 adds the item change journal, v3 the
Telegram folder/bootstrap metadata, v4 the date-first content contract, and v5
the privacy-safe per-chat content progress plus seeded resumable windows. v6
adds the monotonic account render generation used when policy changes bytes at
an unchanged message watermark. v7 separates TDLib's numeric local file id
from its refreshable remote locator and stable remote unique id. v8 adds
explicit story content/privacy state, active/profile reconciliation generations,
minimal inaccessible tombstones, and per-chat bounded/resumable story cursors.
v9 adds typed byte-free story content locators, profile pin order on the month
appearance, and account-level resumable `storyListMain` loading progress.
v10 gives active and monthly appearances of the same canonical story distinct
provider identities. v11 adds an account-scoped purge queue so a committed
Audit-to-Mirror transaction can resume physical cache deletion after a crash;
owned hydrator startup drains it automatically. v12 retains allowed Audit
attachment revisions without retaining download authority. v13 seeds and
schedules history only for current chat-list members while preserving dormant
cursors. v14 orders dirty generated documents by durable publication progress,
so a repeatedly changing low-sorting chat rotates behind never-published and
less-advanced months. Authoritative attachment
restriction cleanup reuses the same transaction journal while releasing only
the affected account's cache, pin, and blob ownership.
v15–v22 add hidden chat metadata, directory rollups, independent backfill-turn
ordering, aggregate provider diagnostics, render-policy skips, bounded direct
chat catalogs, the Stories view, and recoverable authorization finalization.
v23 indexes non-null cache materialization references so generated-generation
reclamation performs one ownership point probe while holding the native
hand-off lease boundary instead of scanning every cached item.
No v24 migration is required for the monthly render correction: snapshots force
the existing v23 chat/time and per-message-event indexes as a streaming nested
loop and sort only each message's local revision list. This avoids a full
joined-month temporary sort without rewriting an installed database or changing
rendered document order.
Each file
carries the full rationale per table — this is the map. `StateStore::open` applies it atomically to a fresh file
(`PRAGMA user_version` 0 → 1), migrates an older file forward, recognizes a
current file, and refuses — with a named `StateError` category — a file from
a newer build (NFR-041). Every table is STRICT; foreign keys are enforced on
every connection; file databases run in WAL with `synchronous=NORMAL`.

| Area | Tables | What holds it together |
|---|---|---|
| Accounts | `accounts` | Per-account display timezone (separate from absolute source timestamps), retention/archive policy, monotonic render generation, current namespace epoch; secrets are references, never material |
| Canonical chat facts | `chats`, `chat_list_entries` | Keyed by (account, namespace, chat id); list membership and exact Telegram order live apart from presentation, and POL-1's `order.json` regenerates from them without a scan |
| The message log | `message_events`, `messages` | The append-only canonical store (POL-3, DEC-015): appends only, enforced by trigger — the sanctioned payload purge enforces Mirror and authoritative Telegram restrictions while retaining event identity as a sync tombstone; `messages` is the current-state projection, whose FK refuses to purge the event backing it. `AUTOINCREMENT` keeps sequence numbers watermark-safe forever |
| Attachments and bytes | `attachments`, `blobs` | Logical kind, Telegram representation, fidelity, source name, MIME, exact size, and availability are orthogonal; locators remain refreshable, and blobs link only after verification |
| Stories | `stories`, `story_content_locators`, `story_appearances`, `story_tombstones`, `story_sync_progress`, `story_list_progress` | Canonical `(poster_chat_id, story_id)` owns the optional blob and one typed primary content source; byte-free active/month appearances transition without copying content and retain first-page profile pin order, restricted stories atomically lose locators and bytes, completed profile generations remove stale rows retention-safely, and privacy-safe per-chat/account cursors resume bounded scans after a crash |
| Provider projection | `items` | Every provider-visible node under its stable binary `ItemId` (DEC-008): canonical structural roots and appearance rows in one table (DOM-002/022), a real parent self-FK for the tree, live-sibling name uniqueness (SYNC-012), one appearance per (canonical, view); Archive pins follow allowed attachment appearances, and protection atomically restricts items and removes those pins. `logical_size` stays a file's own bytes while `aggregate_size` carries a directory's exact indexed-descendant rollup, so a chat folder's size is answerable from one item read and neither fact can ever be mistaken for the other |
| Item change journal | `item_change_journal`, `item_changes` | Durable change enumeration for provider sync anchors (PLAT-MAC-004): one coalesced row per item at its latest `AUTOINCREMENT` sequence — bounded by item count, never rewound — refreshed by the item write paths only on provider-visible change, so an engine re-baseline replays nothing; the identity row names the database life so anchors from a quarantined file expire explicitly |
| Hydration | `transfers` | Durable transfer journal pinned to a content version (SYNC-042), JSON-validated ranges, the SYNC-044 failure taxonomy, a partial index over live states for the queue head |
| Cache | `cache_entries`, `pins`, `retention_purge_queue` | POL-2: LRU eviction scans a partial index that pinned/unverified content never enters; generated materialization ownership is a partial-index point probe so reclamation cannot monopolize the lease boundary; `pins` is durable offline intent independent of materialization; destructive retention queues physical deletion before dropping cache ownership |
| Sync positions | `change_cursors`, `chat_sync_state`, `chat_content_progress` | One durable feed position per (account, stream) — scope verification stays with `ChangeCursor::require_scope` (SYNC-004); a chat gets bounded resumable history and privacy-safe pending/ready/retry/protected state when its first live Telegram list membership makes it provider-eligible, while an existing cursor survives later membership removal/reappearance (SYNC-021). The backward-crawl rotation is keyed on `last_backfill_at_ms` — when a chat was last handed a turn — and never on `last_sync_at_ms`, which live delivery also stamps: ordering on the latter let incoming messages reset a chat's place in the queue, so the busiest correspondences were the ones that never crawled backward |
| Backfill control | `backfill_control` | The engine backfill scheduler's durable per-scope pause switch, request spacer, and honored flood-wait deadline — a restart resumes neither paused work nor a violated flood wait (SYNC-043/SYNC-005 pause, SEC-031 spacer, NFR-033 flood, NFR-031/SYNC-070 restart durability) |
| Rendering | `message_events`, `items`, `render_state`, `cache_entries` | One-transaction monthly snapshots pin message watermark plus account policy generation and stream the selected chat/month in document order without a joined-month temp sort; paired appearance catalog; renderer/schema versions and dirty worklist; chat privacy transitions walk only the indexed direct/month subtree and retain stable ItemId order instead of scanning and sorting every generated document in the account; publication rechecks policy and ignores unrelated-month event races while item facts/cache locators/change-journal rows advance atomically (SYNC-024, SYNC-030..033) |
| Versioning | `schema_history` + `user_version` | The pragma answers "what is current"; the table answers "how did we get here" for the migration runner (SYNC-072) |

## Migrations

Forward-only (NFR-013). `v1.sql` creates version 1 and is frozen; every
version after it is a `Migration` in `src/migrate.rs`, applied in order by
`StateStore::open`. There is no downgrade: an older build meeting a newer
file refuses it rather than guessing what the newer schema's data means in
an older shape. `MIGRATIONS` contains the contiguous v2–v23 steps, and a const
assertion fails the build if that list and the version ever disagree. The v4
atomic rebuild retires live legacy year/media/whole-chat rows, creates direct
months and both bounded documents, and preserves existing account/chat/item
identities as live rows or migration tombstones. The v21 installed-profile
rebuild copies `items` through its primary-key order in 4,096-row transactions.
Durable source-table triggers journal keys changed by already-open WAL peers
between chunks. After building each shadow index in a separate resumable phase,
one short `BEGIN IMMEDIATE` chunk replays those keys, proves whole-table counts
plus exact full-column equivalence for every journaled mutation and foreign-key
integrity, and swaps the tables before releasing the writer lock and earning
schema 21.

Crash safety (SYNC-072) rests on one rule: `PRAGMA user_version` advances
only in the same transaction as the work that earns it, so the version is
never a claim the data cannot back.

| Step | For | On interruption |
|---|---|---|
| `MigrationStep::Sql` | DDL and bounded data work — one transaction | Rolls back whole; the next open starts it over |
| `MigrationStep::AtomicRebuild` | Bounded table/projection rebuild requiring temporary FK suspension | Commits schema, projection, version, and history together; foreign keys are checked before acceptance |
| `MigrationStep::Resumable` | Data too large for one transaction (a backfill across 110k messages) | Committed chunks stay; `migration_progress` holds the last checkpoint and the next open resumes from it |
| `MigrationStep::ResumableRebuild` | Multi-million-row table rebuild requiring temporary FK suspension | Shadow-copy and index phases commit independently; the old version remains authoritative until validated shadows swap atomically |

A resumable step commits each chunk's data changes together with the
checkpoint it resumes from, so the pair can never disagree. Its `prepare`
DDL runs in the same transaction as the first chunk's commit — applied once,
and rolled back with it if that commit never happens. A resumable rebuild also
restores the connection's prior foreign-key enforcement setting after every
chunk, including error paths.

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

## Repositories

`src/repo/` (TASK-260715-1opnb2) is the sanctioned surface other crates use:
typed operations over the schema, speaking the `gramdrive-model` vocabulary
plus record types — no SQL, no `rusqlite` type, and no stored encoding
(enum strings, range JSON, cursor text) crosses the boundary. Everything
runs inside an explicit transaction:

- `StateStore::read_txn` — a WAL read snapshot: every query in it sees one
  consistent database state and never blocks the other process's writer.
- `StateStore::write_txn` — a short `BEGIN IMMEDIATE` write transaction:
  the write lock is taken up front (no mid-transaction upgrade failures),
  commit is explicit, and *drop is rollback* — which is the crate's
  cancellation boundary: abandoning a transaction at any point leaves the
  database exactly as the last commit left it.

| Area | Operations | The invariant they carry |
|---|---|---|
| Accounts | upsert / read / typed retention transitions / Archive-Mode lifecycle / `bump_namespace` | display timezone persists independently; generic upsert cannot bypass retention purge, Archive ownership, or timezone repartition; Audit-to-Mirror is account-confirmed and transactional; Archive Mode stays independent; byte-shaping transitions advance `render_generation` |
| Chats and lists | `upsert_chat`, `replace_chat_list`, ordered `chat_list` | one list replaced whole per snapshot; read order is pinned-first, Telegram order descending (POL-1, DEC-013) |
| Items | upsert / read / `children_page` / `child_by_name` / `appearances_of` / `tombstone_item` / `update_item_content` | identity columns (kind, scope, canonical link, view) derive from the `ItemId` itself, so the caller cannot write them inconsistently; content updates are compare-and-set on `ContentVersion` (DOM-003) |
| Changes | `apply_message_changes`, event/message reads, sync windows | idempotent by Telegram identity (SYNC-021): exact replays, post-deletion revisions, and stale pre-edit revisions are skipped, so at-least-once delivery has exactly-once effect |
| Cursors | `put_cursor` / `cursor` / `clear_cursor` | scope-checked both ways against the account's *current* epoch — a retired-epoch cursor is an explicit `CursorOutOfScope`, never a silent apply (SYNC-004); atomicity with state is the shared transaction (SYNC-022) |
| Chat content progress | `put_chat_content_progress` / `chat_content_progress` | exposes only stable failure categories and retry facts; raw Telegram errors or chat content never enter operational progress |
| Attachments and blobs | `upsert_attachment`, `record_blob`, `link_attachment_blob` | orthogonal fidelity/representation facts round-trip; a locator refresh cannot detach verified bytes |
| Stories | `upsert_story_with_locators`, `replace_active_stories`, `set_story_appearance`, `clear_profile_pin_order`, `finish_profile_scan`, `mark_story_inaccessible`, `put_story_sync_progress`, `start_story_list_pass`, `advance_story_list_progress`, `link_story_blob` | one canonical typed content source and byte link, atomic active→month placement, authoritative active expiry, durable pin reorder, generation-safe profile removal, bounded account/per-chat progress, and fail-closed protected-content enforcement |
| Transfers | enqueue / claim / progress / suspend / resume / fail / cancel / done | coalescing per (item, version) (SYNC-046); two-phase cancel observed at work boundaries; `mark_transfer_done` re-checks the item's content version inside the promoting transaction (SYNC-042) |
| Cache and pins | entry upsert / touch / verify / evict / usage, `pin_item` / `unpin_item`, Archive worklist, retention-purge queue/ack | eviction eligibility (unpinned + verified) is in the DELETE itself; Archive candidates are allowed persistent items only; purge acknowledgements are idempotent and account-scoped |
| Render | `ensure_render_state`, `mark_render_dirty`, `dirty_render_items`, `publish_render` | watermarks only advance; a publication re-checks the event log in its own transaction and stays dirty if events arrived while rendering (SYNC-024) |

SYNC-022's atomic checkpoint is compositional, not a special API: call
`apply_message_changes`, `record_chat_sync`, `put_chat_content_progress`, and
`put_cursor` under one `write_txn` and they commit or vanish together.

## Reconciliation

`src/reconcile.rs` (TASK-260715-21clwh) is the pass that makes the database
and the bytes on disk agree again after a process died between them
(SYNC-070). `StateStore::reconcile` repairs; `StateStore::plan_reconcile`
runs the same survey and only reports — the dry run TASK-260715-1nuhxj
presents before committing to anything. Both are idempotent: a second pass
over a reconciled file finds nothing.

It cannot touch Telegram, structurally rather than by discipline — the
architecture forbids this crate depending on `gramdrive-source`, and the
entrypoints take a `LocalStorage` and nothing else (SYNC-071).

**The `LocalStorage` port.** This crate never chooses paths, so it cannot
walk a cache directory; the host implements the port and this crate joins
the two inventories against the opaque handles already in the schema
(`cache_entries.materialization_ref`, `transfers.temp_ref`). A listing that
fails is fatal to the pass (`StateError::LocalStorage`) — a survey against a
partial inventory would read every unlisted object as an orphan and delete
live cache. A failure on one individual object is survivable and becomes an
unresolved finding.

**The precondition: no engine is running against the file.** Every check
reads a database/disk disagreement as damage, and a live engine is a
permanent legitimate source of exactly those — it is always between two
steps of something (bytes staged, range not yet recorded; object written,
row not yet committed). So this requires what `fsck` requires: nothing else
may be touching what it repairs. It is a caller contract, the same shape as
"the host chooses where the file lives". The containing app runs it at
startup before claiming anything (TDLib, and so the engine, cannot live in
the extension — `.spec/architecture.md`); the extension never runs it, since
it claims and materializes nothing; a user-triggered repair quiesces the
engine first.

That precondition is what makes a `running` transfer legible. The row is
otherwise ambiguous — a dead claim and a live one look identical, and this
crate has no liveness primitive to tell them apart. With it, no claim can be
live, so every `running` row is a dead one.

| Finding | Evidence | Repair |
|---|---|---|
| `InterruptedTransfer` | a `running` row (so: a dead claim) | requeued **keeping** `completed_ranges`/`temp_ref` — a resume, not a restart; `retry_count` untouched, because a crash is not a failed attempt |
| `LeakedStaging` | a staging object no live transfer claims | object deleted, stale `temp_ref` cleared off the terminal row |
| `MissingCacheObject` | `materialization_ref` absent from the inventory (SYNC-053) | the `cache_entries` row is dropped — it claims bytes that do not exist. **The `pins` row is not**: POL-2 intent is independent of materialization, so the engine re-hydrates it. A generated document also goes back on the dirty worklist |
| `OrphanCacheObject` | an object no row claims | deleted; the database is the authority on what is cached, so an unclaimed object can never be served |
| `UnlocatableCacheEntry` | entry with no handle | reported only — an entry we cannot check is not one we may delete |
| `ProjectionRebuildPending` | a `rebuild_projection` marker | reported only, marker left raised: rebuilding `items` needs the engine-side projection builder, and the work is still owed |
| `MigrationInterrupted` | a `migration_interrupted` marker | reported only — `open` is what resumes a migration, before any of this runs |

## Multi-process safety

On Apple platforms the app and the File Provider extension are separate
processes over one database file (`.spec/architecture.md`); nothing
in-memory is authoritative. What makes that safe:

- **WAL + `synchronous=NORMAL`** on every file connection; a file that
  refuses WAL refuses to open (`StateError::WalUnavailable`).
- **Snapshot reads.** A `read_txn` pins one database state; the other
  process's commits do not tear it, and it never blocks their writer.
- **`BEGIN IMMEDIATE` writes + busy timeout.** Writers serialize on the
  file lock; a contending writer waits (up to 5 s) instead of failing
  mid-transaction, and decisions (queue claims, CAS checks, promotion
  version checks) are made *inside* the write transaction, against the
  committed truth — never against a stale in-memory picture.
- **Short transactions.** Long work (hydration, rendering, backfill) is a
  sequence of short transactions with re-checks at each boundary; the
  durable `cancel_requested` flag and the render watermark re-check exist
  precisely because another process may act between two of them.

SQLite's locking is file-based, so `tests/repo_concurrency.rs` exercises
the real primitives with two connections: a stable read snapshot across a
foreign commit, two contending writers never double-claiming a transfer,
and a reader that can never observe a cursor ahead of the state it seals.
What two connections cannot exercise, `tests/multiprocess.rs` does with
real processes (TASK-260715-gnsa2s): several writer processes contending
with no shared memory at all, and a writer SIGKILLed mid-transaction —
WAL recovery on the next open must discard the dead process's half-written
work, and the cursor-behind-state invariant must hold through every kill.

Two primitives support cross-process coordination directly:

- **`StateStore::data_version()`** — SQLite's connection-relative change
  stamp: it moves exactly when *another* connection (any process) has
  committed since this connection last read it. The cheap "anything new?"
  probe change signaling pairs with; meaningful only relative to earlier
  reads on the same connection.
- **`recovery::probe_database` / `recovery::quarantine_if_corrupt`**
  (`src/recovery.rs`) — file-level corruption handling. Detection is
  separate from destruction: the probe only reads (`PRAGMA quick_check`),
  and quarantine re-probes and moves files — sidecars first, main file
  last, so a crash mid-quarantine leaves a re-probeable file and never a
  stale `-wal` beside a fresh database — only when SQLite itself reports
  `SQLITE_CORRUPT`/`SQLITE_NOTADB`. Exactly one process role (the
  engine host; enforced at the FFI boundary) may quarantine: two
  concurrent recoverers could quarantine each other's fresh files. The
  damaged files are preserved under `quarantine/` for diagnosis, never
  deleted.

## Evidence

- `tests/schema_invariants.rs` — FKs, uniques, CHECKs, the append-only
  trigger, and cascades proven against the real database.
- `tests/query_plans.rs` — every required query path EXPLAIN-verified as
  index-driven (no bare table scans, no temp sorts) on the synthetic large
  account from `gramdrive-testkit` (2,048 content-bearing chats, 110k
  messages) plus 6,609 additional runnable metadata-only backfills. Set
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
  v22→v23 preserves cache rows while installing the covering materialization
  reference plan, a file from the future is refused *without being written
  to*, and repair markers round-trip.
- `tests/repo_changes.rs` — atomic cursor application through the failure
  path (a rejected cursor rolls back the whole batch), idempotent replay
  (exact, post-deletion, and stale-revision), scope rejections, sync
  windows and the backfill backlog.
- `tests/repo_backfill.rs` — the backfill scheduler's durable control row:
  absent-reads-`None`, full-field round-trip, single-row upsert per scope.
- `tests/repo_snapshots.rs` — account epoch discipline, chat-list
  replacement and order, item identity derivation, paged enumeration to
  exhaustion, content-version compare-and-set, tombstone name reuse,
  attachment refresh keeping its blob link.
- `tests/repo_transfers.rs` — coalescing, claim order with backoff and
  cancel flags, the full lifecycle with typed wrong-state answers, and the
  SYNC-042 version re-check at promotion.
- `tests/repo_cache_render.rs` — eviction eligibility in the DELETE,
  accounting sums, pin origin semantics, watermark publication and the
  render/append race.
- `tests/repo_concurrency.rs` — two connections over one WAL file: stable
  read snapshots, serialized writers with no double-claim, and cursor
  never ahead of state under a concurrent reader.
- `tests/multiprocess.rs` — the same invariants with real processes
  (re-executed test binary): three writer processes racing 75 batches and
  75 serialized counter bumps with an observer asserting cursor-behind-
  state throughout, and a crash-writer SIGKILLed mid-stream across three
  rounds — after every kill the file passes `quick_check` and the messages
  equal exactly the batches the cursor seals.
- `tests/recovery.rs` — corruption probe and quarantine against real
  files: deterministic corrupt fixtures (garbage bytes, damaged header),
  healthy/missing files declined, sidecars quarantined with the main
  file, and a fresh open on the cleared path.
- `tests/reconcile.rs` — the NFR-034 fixtures: *missing* (a row for bytes
  that are gone), *extra* (bytes no row claims), and *corruption* (in-flight
  state that outlived its process), each asserted to converge, to converge
  idempotently, and never at the cost of a pin. The crash test is not a
  simulation: it re-executes the test binary, lets the child `abort()` with a
  transfer claimed and a transaction open, and reconciles the file the dead
  process actually left — committed progress intact, uncommitted work gone.
  Verified by mutation: requeueing without the staged ranges, protecting
  every `temp_ref` instead of only the claimed ones, deleting a requeued
  transfer's staging area, or dropping the pin with the entry each fail this
  suite.

## Test command

```sh
cargo test -p gramdrive-state
```
