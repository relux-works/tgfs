# TASK-260715-10p5zp — Ordered update loop and gap recovery — results

## What landed

`LiveMachine` (`crates/gramdrive-source-tdjson/src/live.rs`, new, ~1000 lines
product + 25 unit tests): the deterministic sans-IO ordered live message
update loop, plus `tests/live_updates.rs` (new, 8 integration suites against
the real `gramdrive-state` store and the real runtime over the mock tdjson).
Module registered in `src/lib.rs` (docs + re-exports) and the crate README's
module table. No state-schema change; no new dependencies.

## Acceptance criteria — evidence

**"Crash/replay tests never advance cursor without state."**
The cursor advance (`LiveCommit::advance_newest`) is emitted only by the
commit whose records connect the gap, and the caller applies records and the
merged cursor in one transaction (SYNC-022).
`restart_at_every_commit_boundary_converges_exactly` kills the loop after
*every* possible commit boundary, asserts the publication invariant on the
crashed durable state (`assert_cursor_covered`: the stored window never
contains a fixture id the store did not observe), then resumes a fresh
machine from nothing but the durable rows with the same updates re-fed —
byte-identical convergence, one event per message, window exact.

**"Gaps recover before publication."**
Per-chat boundary states Unverified → Bridging → Verified: the first live
message above the committed `newest` opens a `getChatHistory` bridge (the
crawl's exact catch-up protocol and page-contract validation); pages before
the connection commit records under the *unchanged* cursor; the connecting
page's commit carries the advance. `gaps_recover_before_the_cursor_is_published`
checks the coverage invariant after every single persisted commit and that
exactly one advance occurs, on the last (connecting) commit. A failed bridge
(TDLib rejection, page-contract violation) freezes the chat's cursor
explicitly (`LiveStep::Degraded` with the crawl's `UnavailableReason`) —
records keep flowing boundary-free, the cursor never lies.

**"Duplicates are idempotent."**
`duplicate_and_out_of_order_updates_are_idempotent`: doubled new-message
updates, repeated deletions, deletion of a never-observed message (skipped,
no forged row — POL-3), edit signal after deletion (refresh answers 404,
counted), then a full replay of everything — the event log is a fixed point.
Unit suites additionally prove duplicate re-emission never re-advances the
cursor and out-of-order arrival converges.

**Checklist item: interacts correctly with in-progress backfill.**
`in_progress_backfill_and_live_updates_never_lose_state` interleaves a real
`CrawlMachine` backfill with the live loop, a new message arriving
mid-backfill. Both sides commit through the merge discipline the module docs
contractualize (live: raise stored newest only; crawl: min/max merge against
the stored row) — final window `[1, 13]`, `history_complete` true, exactly
one event per message, neither side clobbering the other's checkpoint.

**Checklist item: gap detection and recovery via targeted re-fetch.**
Offline arrivals are re-fetched by the bridge (`getChatHistory`); edits are
re-fetched as one consistent snapshot per message via a coalesced
`getMessage` (TDLib splits an edit across `updateMessageContent` /
`updateMessageEdited`; merging partials would be a torn write). Both
re-fetch paths honor flood waits (`LiveBackoff` + identical re-issue,
SYNC-044/TGC-22, proven in unit + `a_flooded_bridge_backs_off_and_completes`)
and one request outstanding at a time (SEC-031).

## Key design decisions (detail in the design doc and LOGBOOK 2026-07-18 0528)

1. **The cursor advance is a merge instruction, not a window write** — the
   crawl moves `oldest` down while the live loop moves `newest` up; each
   caller-side apply merges against the stored row inside the transaction.
2. **The live loop never establishes a window** — anchoring stays the
   crawl's (with its completeness-reset discipline); a windowless chat gets
   boundary-free commits. Establishing `[N,N]` live would recreate the
   anchor-gap orphaning class fixed in TASK-260715-26dnp6 review.
3. **Records are never gated, only the cursor is** — observations are
   idempotent by message identity (SYNC-021); rows outside the window are
   safe by design.
4. **`updateMessageInteractionInfo` deliberately not consumed in v1** —
   reaction/view tallies tick continuously and would grow the append-only
   event log (POL-3) pathologically; reactions refresh on any re-observation.
5. **`sending_state` messages are skipped** (provisional ids); the final
   message ingests via `updateMessageSendSucceeded`. Deletions count only
   when `is_permanent && !from_cache`.
6. **Untracked chats buffer and report once** (`LiveStep::Unresolved`);
   after the caller upserts the canonical row (FK) and calls `track_chat`,
   the buffer replays in arrival order (proven FK-safe end to end).

## Gates

`make check` (suite all, run-id local-all): 8/8 ok — toolchain, format,
lint (clippy `-D warnings --all-features --all-targets`), test (workspace,
includes 26 live-related unit tests and the 8 new integration suites),
architecture, supply-chain (cargo deny), traceability, scripts.
Provenance: `.temp/acceptance/local-all`.

## Files

- `crates/gramdrive-source-tdjson/src/live.rs` — new module
- `crates/gramdrive-source-tdjson/tests/live_updates.rs` — new integration suite
- `crates/gramdrive-source-tdjson/src/lib.rs` — module docs + re-exports
- `crates/gramdrive-source-tdjson/README.md` — module table row
- `LOGBOOK.md` — entry 2026-07-18 0528
