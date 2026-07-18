# TASK-260715-26dnp6 — Rework results: anchor fold stale `history_complete`

Status: ready for review. Rework of the confirmed correctness defect in
`TASK-260715-26dnp6_review-verdict.md` (RUN-260718-ed9981). Everything else in the
crawl was accepted as-is and left untouched — no redesign. `make check` 8/8 green.

## The defect (recap)

`CrawlMachine::on_page`, `Phase::Anchor` non-empty branch, installed the fresh
`[min, max]` window but never reset `chat.history_complete`, which the plan may have
carried in as `true`. That plan state is the machine's *own* durable output for an
empty chat (`{window: None, history_complete: true}`). On a resume where such a chat
gained > page_size messages during downtime, the anchor commit persisted a false
`history_complete=true` over a partial window; a crash at that boundary let the next
resume's catch-up conclude `Phase::Complete` and skip the backfill — the oldest ids
permanently orphaned, silently (no `Unavailable`, no error).

## What changed

### 1. The fix (product code)

`crates/gramdrive-source-tdjson/src/history.rs`, `on_page` `Phase::Anchor` `Some`
branch:

```rust
chat.window = Some(CrawlWindow { oldest_message_id: oldest, newest_message_id: newest });
chat.history_complete = false; // <-- added: a fresh window re-proves completeness
chat.phase = Phase::Backward;
```

Completeness is now re-proven only by an empty backward answer, exactly as for a
never-crawled chat. One line; the phase machine is otherwise unchanged.

### 2. Regression pinned (integration suite)

`crates/gramdrive-source-tdjson/tests/history_crawl.rs`:

- New helper `seed_empty_complete` — persists `{window: None, history_complete: true}`
  through the *real* commit path (`apply_commit`), so the seeded durable row is
  byte-identical to a genuine empty-chat commit.
- New suite `resume_of_a_grown_empty_complete_chat_resumes_exactly` — the
  empty-complete→active flavor of the every-commit-boundary interruption fixture:
  seed empty-complete, grow the chat by 13 messages (page_size 5 → several pages)
  during downtime, then kill/resume at *every* commit boundary. Asserts gap-free
  convergence: full id set `[1..13]`, exactly one stored event per message, window
  `[1, 13]`, and `history_complete` true only after the empty backward answer.

### 3. Focused unit test (defense in depth)

`crates/gramdrive-source-tdjson/src/history.rs` (unit module):
`anchor_over_a_carried_complete_flag_resets_it` — resumes a chat carrying
`{window: None, history_complete: true}`, feeds a non-empty anchor page, asserts the
commit's `history_complete == false` and that the backfill actually runs (a
`from_message_id` submit rather than `Done`).

## Verification

- **Regression proven caught**: with only the one-line fix reverted, both new tests
  fail. The integration test reproduces the review's exact orphaning —
  `stored ids [9, 10, 11, 12, 13]` vs expected `[1..13]` — so the fixture pins the
  defect class, not merely the symptom. Fix restored → all green.
- `cargo test -p gramdrive-source-tdjson` — 16 unit + 8 integration suites green.
- `make check` — full acceptance gate, **8/8 green** (toolchain, format,
  clippy `-D warnings`, workspace tests, architecture, supply-chain, traceability,
  scripts).

## Acceptance criteria

- "Restart continues without duplicates" — now also gap-free on the empty-complete
  resume path (story DoD: "no duplicate or missing messages"). All prior AC evidence
  from `TASK-260715-26dnp6_results.md` remains valid; this rework only closes the one
  reachable gap the reviewer found.
