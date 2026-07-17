# TASK-260715-30amrq — Initial chat-list snapshot: implementation notes

Status: ready for review. `make check` 8/8 green (toolchain, format, lint, test, architecture, supply-chain, traceability, scripts); provenance `.temp/acceptance/local-all`.

## What was built

`SnapshotMachine` — a deterministic, sans-IO initial chat-list snapshot in
`gramdrive-source-tdjson` (`src/snapshot.rs`, new; follows the `AuthMachine`
precedent). It drives TDLib's chat-list protocol per planned list
(Main/Archive/folders passed in; folder-catalog discovery stays with
TASK-260715-54nopz):

1. `loadChats(list, page_size)` pagination until the end-of-list `404`
   (terminator, not a failure), consuming `updateNewChat` /
   `updateChatPosition` (plus opportunistic `updateUser`/`updateSupergroup`
   for usernames — zero per-chat fan-out).
2. `getChats(list, limit)` as the exact-order witness; a full answer doubles
   the limit so a short guess cannot silently truncate.
3. Lazy `getChat(chat_id)` for anything the witness names that updates did
   not announce (SYNC-020: never anything heavier than the chat object).
4. One typed `ListCommit` per list: `ChatSnapshot` canonical facts +
   `ListEntrySnapshot` rows carrying Telegram's exact ordering metadata
   (opaque int64 `order`, pinned flag; DEC-013/POL-1), emitted pinned-first,
   order desc, id desc — byte-for-byte the state `chat_list` read order —
   plus the durable `resume_token`.

The caller persists each commit atomically through the state repositories
(`upsert_chat` + `replace_chat_list` + `put_cursor` under one `WriteTxn`,
SYNC-022), cursor stream `SNAPSHOT_CURSOR_STREAM`.

## Key decisions (full rationale in LOGBOOK.md 2026-07-18 0250)

- **Resume is list-granular** because `loadChats` has no offset — TDLib owns
  the load position and its local DB is the page cache; an interrupted list
  re-enumerates locally. Token = versioned JSON of completed lists inside a
  `ChangeCursor` payload (SYNC-004 scope rejection for free; unreadable or
  future-version tokens rejected explicitly, never treated as empty).
- **Membership truth is the position map, not the witness sequence** —
  position updates consumed after `getChats` answered are newer; a mid-load
  order bump must not fail the run. Witness contributes duplicate detection
  (typed, SYNC-003), gap detection (`MissingPosition` after lazy resolution
  fails), and the lazy work list. Explicit order-0 = left the list →
  excluded + counted, not a gap.
- **Flood wait via wrapper backoff (SYNC-044):** 429/`FLOOD_WAIT` (stated
  delay parsed; `trailing_integer` moved to `error.rs`, shared with auth)
  and 500 arm one typed `Backoff{retry_after_secs, attempt}` then re-issue
  the identical request; the machine never sleeps and never caps attempts —
  caller policy owns both. Everything else is a typed terminal error.
- **Fail-safe exclusions:** secret chats (POL-4) and unknown chat types are
  excluded and counted (`excluded_secret`/`excluded_unsupported`) — the
  state `ChatType` vocabulary cannot represent them.
- **Layering:** commits are typed outputs the composing caller executes
  (same pattern as removal's directives). `gramdrive-state` is a
  dev-dependency only (direction table binds `[dependencies]`); the
  integration suite proves the loop against the real in-memory store.

## Tests

Unit (8, `src/snapshot.rs`): plan validation, token round-trip + 8 rejection
shapes, resume plan-intersection, int64-string order parsing, retryable
classification table, username fallback, misuse poisoning.

Integration (8, `tests/chat_snapshot.rs`, fixture server mirroring TDLib's
wire protocol over `MockTdJson` + real `TdRuntime` + real `StateStore`):
- exact order/metadata/appearance persistence (all flavors, protected flag,
  usernames incl. lazily-resolved chat, one canonical row for a
  multi-list chat, secret excluded+counted, request surface exactly
  {loadChats, getChats, getChat});
- **large synthetic fixture** (1500 Main across 128-chat pages incl. lazy
  tail + 300 Archive): interrupt after Main commit, resume from the stored
  cursor — Main never re-requested, final state has exact order, no
  duplicates, no gaps;
- flood-wait 429 (advice carries `retry_after=7`) and transport 500 backoff
  then success;
- concurrent removal mid-load excluded, not a gap;
- duplicate witness id and unresolvable listing → typed contract failures,
  machine stays poisoned;
- fatal request error typed with source;
- empty lists commit empty membership and resume to immediate Done;
- int64 orders at the i64 ceiling survive exactly through the string wire
  shape.

## Commands run

- `cargo test -p gramdrive-source-tdjson` — 64 lib unit + 51 integration, green
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` — clean
- `cargo fmt --all` applied; gate `format` check green
- `make check` — 8/8 green

Not run: `make tdjson-smoke` (real-linkage smoke) — requires the staged
TDLib artifact and exercises only FFI linkage; this change is mock-only
runtime logic, unaffected by linkage. No app-level runtime surface exists
yet to drive beyond the integration suite (the `DriveSource` adapter and FFI
composition are follow-up story tasks).

## Files

- `crates/gramdrive-source-tdjson/src/snapshot.rs` — new (machine + unit tests)
- `crates/gramdrive-source-tdjson/tests/chat_snapshot.rs` — new (integration)
- `crates/gramdrive-source-tdjson/src/error.rs` — shared `trailing_integer` + tests (moved from auth)
- `crates/gramdrive-source-tdjson/src/auth.rs` — uses the shared helper
- `crates/gramdrive-source-tdjson/src/lib.rs`, `README.md` — docs + re-exports
- `crates/gramdrive-source-tdjson/Cargo.toml` — dev-dep `gramdrive-state` (documented)

## Wiring contract for the composing caller (follow-up tasks)

Authorized client required. Pump the `UpdateStream` into `on_update` before
feeding each response (arrival order). Persist every `ListCommit` + its
token atomically. On cursor scope/parse rejection: clear the cursor,
re-baseline with a fresh full snapshot. Backoff advice: wait
`retry_after_secs` (caller default when `None`), cap attempts by policy.
