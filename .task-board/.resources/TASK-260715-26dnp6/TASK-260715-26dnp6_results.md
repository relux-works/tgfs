# TASK-260715-26dnp6 — Resumable per-chat history crawl: implementation notes

Status: ready for review. `make check` 8/8 green (toolchain, format, clippy `-D warnings`,
workspace tests, architecture, supply chain, traceability, scripts).

## What landed

`CrawlMachine` — a deterministic sans-IO resumable history crawl in
`crates/gramdrive-source-tdjson/src/history.rs`, following the crate's established
machine shape (`SnapshotMachine`/`UpdateMachine`): `next_step()` names one obligation
(`Submit` / `Backoff` / `Commit` / `Unavailable` / `Done`), `on_response()` feeds the
outcome. The only request type ever issued is `getChatHistory` (TGC-21); never a media
request (SYNC-020 — attachments stay descriptors inside `MessageRecord`).

### Files

- `crates/gramdrive-source-tdjson/src/history.rs` — new: machine, plan/commit/progress
  vocabulary, 15 unit tests.
- `crates/gramdrive-source-tdjson/tests/history_crawl.rs` — new: 7 integration suites
  persisting through the real `gramdrive-state` store.
- `crates/gramdrive-source-tdjson/src/error.rs` — `retryable_after` (flood/transport
  classification) moved here from `snapshot.rs` so both machines share one copy;
  its unit test moved with it.
- `crates/gramdrive-source-tdjson/src/snapshot.rs` — re-points to the shared classifier.
- `src/lib.rs`, `README.md` — module docs and re-exports.

No state-schema change: the durable cursor (`chat_sync_state` — `[oldest, newest]`
window, `history_complete`, backlog index) and the idempotent batch writer
(`apply_message_changes`) already landed with STORY-260715-16ik2x.

## Design decisions (detail in LOGBOOK.md 2026-07-18 0442)

1. **Cursor = `chat_sync_state` row, no opaque token.** Every answered page yields
   exactly one `HistoryCommit` (records + window + completion facts); the caller
   persists both in one transaction (SYNC-022). Resume = read the rows back into
   `CrawlPlan`. SYNC-004 scoping is the row key's job.
2. **Three phases per chat, window always contiguous.** Anchor (fresh chat: first page
   from 0 → initial window) → Catch-up (existing window: descend from 0 until an id
   connects to the committed `newest`; unconnected pages commit records under the
   *unchanged* window, so a crash mid-catch-up refetches and replay skips) → Backward
   (page below `oldest` until an empty answer marks `history_complete`). Catch-up is
   the contract with the ordered update loop (TASK-260715-10p5zp): it always receives
   a newest boundary reconnected to the present.
3. **Per-chat blast radius.** Non-retryable TDLib rejections (left/inaccessible chats)
   and page-contract violations (duplicate/ascending ids, id ≥ `from`, foreign chat)
   fail one chat as explicit `CrawlStep::Unavailable`; the run continues. Only
   runtime-level failures are machine-fatal. A malformed message object inside a sound
   page is counted (`skipped_malformed`) and its id still advances the cursor.
4. **Bounded scheduling.** One outstanding request (SEC-031); after every page the
   scheduler re-picks: `CrawlPriority` (Visible > Requested > Background), then fewest
   pages served, then plan order. Equals round-robin page by page — a huge history
   cannot starve the account; a Visible chat is served exclusively until done.
   `set_priority` applies at the next pick. Flood 429/500 arms `CrawlBackoff` with
   Telegram's stated delay and re-issues the identical request (TGC-22); the machine
   reads no clock, so the local-backfill pacing policy composes at the caller seam.
5. **Per-chat progress observable** via `CrawlMachine::progress()` (phase, window,
   completion, pages served, records emitted).

## Acceptance criteria → evidence

- **Restart continues without duplicates** — `restart_at_every_commit_boundary_resumes_exactly`:
  a 23-message crawl interrupted after *every* possible commit boundary, resumed from
  nothing but the durable rows, converges to the uninterrupted result with exactly one
  observed event per message (replayed pages append nothing). Plus mid-catch-up
  interruption in `catch_up_after_downtime_extends_the_newest_boundary`.
- **Flood waits honored** — `flood_wait_is_honored_and_the_crawl_completes` + unit
  test: stated delay surfaces as backoff advice, identical request re-issued.
- **Priority favors visible/requested** — `priority_favors_visible_chats_and_equals_round_robin`
  + unit test: baseline round-robin, then a mid-run Visible boost takes every page
  until the boosted chat finishes.
- **Huge histories bounded** — per-page re-picking (round-robin among equals) +
  one-page-at-a-time commits (bounded memory) asserted across suites.
- **Scope flavors** — `full_crawl_covers_every_flavor_and_records_boundaries`: private,
  group, supergroup with forum-topic + album facts, channel (chat sender), empty chat
  (complete, windowless); left/unsupported → `an_unavailable_chat_is_explicit_and_the_rest_complete`.
- **Oldest/newest boundaries recorded** — every suite asserts the persisted
  `chat_sync_state` window and completion flag.
- **Runtime wiring** — `the_crawl_round_trips_through_the_real_runtime`: same loop
  through `TdRuntime` + mock tdjson.

## Verification commands run

- `cargo test -p gramdrive-source-tdjson` (lib + integration) — green.
- `make check` (full acceptance gate, 8/8) — green.
