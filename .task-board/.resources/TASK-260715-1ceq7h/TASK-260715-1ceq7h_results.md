# TASK-260715-1ceq7h — SQLite schema: implementation notes

Status: ready for review. All quality gates green (`make check`, 8/8,
provenance `.temp/acceptance/local-all`).

## What was built

**Schema v1** — `crates/gramdrive-state/src/schema/v1.sql` (the file carries
per-table rationale; `crates/gramdrive-state/README.md` has the map):

- `accounts` — POL-3 retention mode (`mirror`/`audit`) and POL-2
  archive-mode per account; current namespace epoch (DOM-021); secrets as
  references only.
- `chats`, `chat_list_entries` — canonical source facts keyed by
  (account, namespace, chat); Telegram list order stored apart from
  presentation, `order.json` (POL-1) regenerates from an ordered index.
- `message_events` + `messages` — the append-only canonical log (POL-3,
  DEC-015), enforced by a trigger whose single sanctioned UPDATE is the
  Mirror-mode payload purge (payload+schema → NULL together). `messages` is
  the current-state projection; its FK refuses to purge the event backing
  it. `AUTOINCREMENT` keeps event sequence numbers (render watermarks) from
  ever being reused.
- `attachments`, `blobs` — attachment identity is (chat, message, ordinal);
  Telegram file id / file_reference are refreshable columns, never identity
  (DOM-007, SYNC-045); blobs content-addressed per account, linked only
  once verified.
- `items` — the provider projection under stable binary `ItemId`s
  (DEC-008): canonical structural roots and appearance rows in one table
  (DOM-002/DOM-022), real parent self-FK, partial-unique live sibling names
  (SYNC-012), one appearance per (canonical, view) via a COALESCE-sentinel
  unique index, closed kind vocabulary matching `CanonicalKey`.
- `transfers` — durable journal pinned to content version (SYNC-042),
  JSON-validated ranges, SYNC-044 failure taxonomy aligned with
  `gramdrive-source::SourceError` names + `disk_full`/`integrity`,
  partial index over live states for the queue head.
- `cache_entries`, `pins` — POL-2: eviction scans a partial index that
  pinned/unverified content never enters; `pins` is durable offline intent
  independent of materialization.
- `change_cursors`, `chat_sync_state` — one feed position per (account,
  stream); per-chat resumable history windows (SYNC-021).
- `render_state` — renderer/schema versions, event-seq input watermark,
  covering partial index for the dirty worklist (SYNC-024).
- `schema_history` + `PRAGMA user_version` — versioned application:
  `StateStore::open` applies v1 atomically at 0, accepts 1, refuses newer
  (`UnsupportedSchemaVersion`) and older-needing-migration
  (`MigrationRequired`) files explicitly (NFR-041; runner is
  TASK-260715-18l9xz).

All tables STRICT; FKs enforced per connection; file DBs are WAL +
`synchronous=NORMAL` (multi-process app/extension contract), and WAL
refusal is a named error.

**Fixture generator** — `gramdrive_testkit::synthetic` (reused by later
perf tasks): deterministic expansion of a spec into a whole account.
`large_account()` = 2,048 chats / 110,000 messages / ~25k attachments,
Zipf-skewed (head chat ~48k messages, ~1,500 empty chats), folder
memberships, pinned order, protected chats, unicode titles. Synthetic
31-day-month calendar makes partitioning pure integer arithmetic. Unit
tests pin totals and a structural digest against drift.

**Dependency** — rusqlite 0.39 (bundled SQLite; MIT, inside POL-6).
0.40 is blocked by the 1.91 toolchain pin (libsqlite3-sys 0.38 build script
uses unstable `cfg_select!`) — noted in workspace `Cargo.toml`. deny.toml
`[bans.build]` extended: `libsqlite3-sys` (compiles the vendored
amalgamation) and the wasm-fallback names (`sqlite-wasm-rs`,
`wasm-bindgen`, `wasm-bindgen-shared`, `rustversion`) that enter the graph
but never compile for a GramDrive target. All four cargo-deny checks pass.

## Evidence

- `tests/schema_invariants.rs` (21 tests) — FKs, uniques, CHECKs,
  append-only trigger (rewrite refused / purge allowed / seq never reused /
  current-state pins its event), cascades.
- `tests/query_plans.rs` — 18 required query paths EXPLAIN-verified on the
  loaded large account (~310k rows total) after ANALYZE: **no bare table
  scans, no temp b-tree sorts**. Captured plans attached as
  `TASK-260715-1ceq7h_explain_evidence.md`. The check has teeth: it caught
  the planner refusing an `IN`-list partial-index predicate for the
  transfer queue, fixed by restating the predicate in OR form.
- `tests/store_lifecycle.rs` (6 tests) — WAL on file DBs, idempotent
  reopen, version-skew refusal, torn-file reporting.
- `gramdrive-testkit` synthetic unit tests (9) — determinism, totals,
  skew, ordering, digest pin.

## Decisions a reviewer should look at

1. **Items unified table** (canonical roots + appearances in one table,
   `canonical_item_id` as indexed identity bytes *without* FK): the
   canonical side lives in typed tables (`chats`, `attachments`, …) reached
   by decoding the ItemId; a per-kind FK is unrepresentable. Tree structure
   itself is a real self-FK.
2. **Append-only via trigger with a purge escape hatch** — POL-3 Mirror
   mode requires "markers only, no content", so the trigger allows exactly
   payload+schema → NULL and nothing else. DELETE stays possible (Audit
   purge tool, account removal) but the `messages` FK protects live state.
3. **`transfers.requested_ranges` as JSON text** with `json_valid` CHECK —
   readable, checkable; interpretation is the engine's.
4. **rusqlite 0.39 pin** (toolchain-driven), bump together with Rust.

## Commands run

```
make check                      # 8/8 green (toolchain, format, lint, test,
                                # architecture, supply-chain, traceability, scripts)
cargo test -p gramdrive-state   # 30 tests
cargo test -p gramdrive-testkit # 204 tests incl. synthetic
```
