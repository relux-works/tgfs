# TASK-260715-26dnp6 — Review verdict: changes requested (→ to-dev)

Reviewer run: RUN-260718-ed9981, 2026-07-18. Verdict: **one confirmed correctness
defect** (silent permanent message gap on a reachable interrupt path); everything
else accepted as-is. `make check` re-run by the reviewer: 8/8 green.

## Confirmed defect: anchor fold keeps a stale `history_complete=true`

`crates/gramdrive-source-tdjson/src/history.rs`, `on_page`, `Phase::Anchor`
non-empty branch (~line 709): it installs the fresh `[min, max]` window but never
resets `chat.history_complete`, which the plan may have carried in as `true`.

That plan state is not caller misuse — it is the machine's *own* durable output
for an empty chat (`window=None, history_complete=true`, asserted by
`empty_chat_completes_without_a_window`), and the module docs instruct resuming
callers to read the rows back verbatim (the integration suites' `plan_from_store`
does exactly that).

### Failure scenario (reproduced, minimal)

1. Run 1: chat is empty → durable row `{window: None, history_complete: true}`.
2. Downtime: **more than one page** of messages arrives (repro: ids 7..10, page_size 2;
   real default page 100).
3. Run 2 resumes → Anchor page from 0 answers the newest page → **commit persists
   `{window: [9,10], history_complete: true}`** while ids 7,8 are still unfetched.
   The durable fact is false the moment it commits.
4. Crash at exactly that commit boundary (the boundary class this task exists to
   survive).
5. Run 3 resumes → CatchUp connects → the connected arm consults
   `chat.history_complete` → `Phase::Complete`, **no backward phase ever runs**.
   `next_step()` = `Done`. Ids 7,8 are permanently orphaned; every later run
   repeats the same conclusion. Silent gap — no `Unavailable`, no error.

Repro artifact: `TASK-260715-26dnp6_anchor-gap-repro.rs` (attached; scratch crate
`.temp/TASK-260715-26dnp6/anchor-repro/`, path-dep on the crate, no product-code
changes). Output:

```
run-2 anchor commit: window=Some(CrawlWindow { oldest_message_id: 9, newest_message_id: 10 }) history_complete=true records=2
!! anchor commit persisted history_complete=true while ids 7,8 are still unfetched
run-3 catch-up commit: window=Some(CrawlWindow { oldest_message_id: 9, newest_message_id: 10 }) history_complete=true
!! run 3 is Done: messages 7 and 8 are permanently orphaned (silent gap)
```

### Why this fails AC / story DoD

- Task AC: "Restart continues without duplicates" — the interruption invariant is
  duplicate-free **and gap-free** convergence (story DoD: "no duplicate or missing
  messages"); this path loses messages.
- SYNC-021 (resumable *idempotent* crawl): the durable cursor lies about
  completeness, so resume converges to the wrong fixpoint.
- Secondary: even without a crash, the anchor commit publishes a transiently false
  `history_complete=true` that any concurrent reader of `chat_sync_state` can
  observe mid-run.

### Why the (otherwise excellent) interruption suite missed it

`restart_at_every_commit_boundary_resumes_exactly` only exercises a never-crawled
chat (`window=None, history_complete=false`); the downtime suite resumes a
*windowed* chat. The empty-complete → active resume flavor is the one plan shape
where Anchor runs with `history_complete=true`, and no fixture covers it.

### Requested change (shape is the implementer's call)

- In the Anchor `Some` branch, reset `chat.history_complete = false` —
  completeness must be re-proven by an empty backward answer, exactly as for a
  fresh chat.
- Add the missing fixture flavor: empty-complete chat gains > page_size messages
  during downtime; run the every-commit-boundary interrupt/resume loop over it
  and assert gap-free convergence (event count exact, window `[first,last]`,
  `history_complete` only after the empty answer).

## Reviewed and accepted (no changes requested)

- **Architecture fit**: `CrawlMachine` matches the crate's sans-IO machine family
  (SnapshotMachine/UpdateMachine): next_step/on_response obligations, no clock, no
  I/O; pacing at the caller seam is the *correct* decomposition — SEC-031
  pacing/scheduling belongs to TASK-260715-mua1ng per TRACEABILITY.md, which this
  task blocks.
- **Cursor design**: `chat_sync_state` as the durable cursor (no opaque token),
  one commit per page, SYNC-022 transactional pairing in tests — sound; the
  unconnected-catch-up "commit records under the unchanged window" rule keeps the
  durable window contiguous and is correctly tested.
- **Paging contract enforcement**: strictly-descending/below-from/foreign-chat
  checks fail one chat (`PageContract`), malformed objects degrade with a counted
  skip and still advance the cursor — verified in unit + integration tests.
- **Flood/blast radius**: 429/FLOOD_WAIT/500 → identical re-issue with stated
  delay (shared `retryable_after`, cleanly deduplicated into error.rs with its
  test); other TDLib rejections → per-chat `Unavailable`; runtime failures fatal.
- **Scheduling**: Visible > Requested > Background, fewest-pages tie-break,
  page-by-page round-robin among equals, boost at next pick — deterministic and
  test-verified; bounded memory (one page per commit) and one outstanding request
  (SEC-031).
- **Gates**: `make check` 8/8 re-run green (format, clippy -D warnings, workspace
  tests, architecture, supply-chain, traceability, scripts). Docs (lib.rs, README
  module table), LOGBOOK entry, and board artifacts all in place.

## Routing

`to-dev` for the one-line semantic fix + the missing interruption fixture, then
another review cycle.
