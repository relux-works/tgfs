# TASK-260715-26dnp6 — Rework review verdict: ACCEPTED (→ done)

Reviewer run: 2026-07-18. Verdict: **accepted**. The single confirmed defect from
the prior cycle (`TASK-260715-26dnp6_review-verdict.md`, RUN-260718-ed9981 — silent
permanent message gap on the empty-complete→active resume path) is fixed, the
regression class is pinned by a fixture, scope was respected (no redesign), and every
gate is green.

## Rework scope, both items satisfied

1. **Fix — anchor fold resets stale completeness.**
   `crates/gramdrive-source-tdjson/src/history.rs:722`, `on_page` `Phase::Anchor`
   `Some((oldest, newest))` branch now sets `chat.history_complete = false` when
   installing the fresh `[min,max]` window. Correct and minimal: Anchor runs only
   when `window == None`, so the sole plan shape reaching this branch with
   `history_complete=true` is the machine's own empty-chat output — a non-empty
   anchor page proves history exists below it, so completeness must be re-proven by
   an empty backward answer, exactly as for a never-crawled chat. No false reset is
   possible (a windowed complete chat enters CatchUp, never Anchor). Clear inline
   rationale added.

2. **Regression pinned in the every-commit-boundary interruption suite.**
   `tests/history_crawl.rs` gains `resume_of_a_grown_empty_complete_chat_resumes_exactly`
   — the empty-complete→active flavor: `seed_empty_complete` persists
   `{window: None, history_complete: true}` through the *real* commit path
   (`apply_commit`), the chat grows by 13 messages at page_size 5 (>1 page), then
   kill/resume at *every* commit boundary asserting gap-free convergence (full id set
   `[1..13]`, one stored event per message, window `[1,13]`, `history_complete` only
   after the empty answer). Plus a focused unit test
   `anchor_over_a_carried_complete_flag_resets_it` (defense in depth).

## Independent verification (reviewer-run, no product-code change)

- **Fix closes the gap**: traced the review's repro against current code — Run-2 anchor
  commit now persists `history_complete=false`; Run-3 catch-up consults the false flag
  → `Phase::Backward`, backfill continues from the window oldest. No orphaning.
- **Regression genuinely caught, not just the symptom**: reverted *only* the one-line
  fix in an isolated worktree (`.temp/TASK-260715-26dnp6/verify-worktree`, working-tree
  crate synced in). Both new tests FAIL — the integration test reproduces the exact
  orphaning `left: [9,10,11,12,13]` vs `right: [1,2,…,13]` at `stop_after=1` (the anchor
  commit boundary), the unit test on "a fresh anchor window must re-prove completeness".
  Fix restored → both pass.
- **Gates**: `make check` **8/8 green** on the real checkout (toolchain, format,
  clippy `-D warnings`, workspace tests 14.8s, architecture, supply-chain, traceability,
  scripts). `gramdrive-source-tdjson`: 114 unit + 8 integration suites green.

## Scope / fit

Only the requested change surface touched — the one semantic line + comment, the two
tests, the `seed_empty_complete` helper. No existing assertion weakened; the original
`restart_at_every_commit_boundary_resumes_exactly` and all other suites still pass. The
accepted architecture from the prior verdict (sans-IO machine, `chat_sync_state` cursor,
contract enforcement, flood/blast-radius, scheduling) is unchanged. AC now holds on the
one path that previously lost messages: "restart continues without duplicates" — and
gap-free (story DoD).

## Routing

accepted → `done`.
