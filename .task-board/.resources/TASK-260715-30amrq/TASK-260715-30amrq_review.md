# Review — TASK-260715-30amrq initial chat-list snapshot

Verdict: ACCEPTED -> done. Reviewer run RUN-260717-0da0f3, 2026-07-18.

## What was verified (independently, not from implementer claims)

- `make check` re-run by the reviewer: 8/8 green (toolchain, format, lint, test, architecture, supply-chain, traceability, scripts). Provenance `.temp/acceptance/local-all`.
- Full read of `src/snapshot.rs` (1323 lines, machine + 8 unit tests) and `tests/chat_snapshot.rs` (1099 lines, 8 integration suites over a scripted TDLib fixture server + real TdRuntime + real StateStore).
- Diffs of `error.rs` (shared `trailing_integer`, tests moved with it), `auth.rs` (uses the shared helper, no behavior change), `lib.rs`/`README.md` (docs + re-exports), `Cargo.toml` (dev-dep `gramdrive-state`, documented).

## AC check

AC: large synthetic snapshot resumes, no duplicates/gaps, exact source ordering metadata.

- `large_snapshot_interrupts_and_resumes_without_duplicates_or_gaps`: 1500 Main (128-chat pages, pinned head, sparse lazy tail) + 300 Archive; interrupt after the Main commit; resume from the cursor read back through the real cursor repo; asserts Main never re-requested (every list-level request names Archive), exact server order both lists, uniqueness, final token readable. AC satisfied.
- Exact ordering metadata: opaque int64 `order` parsed from tdjson string wire shape (exact at the i64 ceiling, pinned by `multi_page_snapshot_recovers_ordering_from_string_int64_orders`), pinned flag verbatim (DEC-013/POL-1).
- SYNC-020: request surface asserted to be exactly {loadChats, getChats, getChat}; no history/media, no per-peer fan-out (usernames ride pushed updateUser/updateSupergroup).
- Resume-token hygiene (SYNC-004): versioned JSON, 8 rejection shapes tested, out-of-plan lists dropped, never silently treated as empty.
- Flood wait (SYNC-044): 429 stated delay parsed (retry_after=7 asserted), 500 -> None advice, identical re-issue; loadChats 404 is the pagination terminator.
- Fail-safe exclusions: secret (POL-4) and unknown chat types excluded + counted; explicit order-0 mid-load removal excluded + counted, not a gap.
- Normalized appearances (PRD-013): one canonical chat record, per-list membership rows, verified through real repo reads.

## Architecture fit

- Sans-IO machine follows the AuthMachine precedent; typed provider-neutral outputs; caller owns IO/time/persistence — consistent with removal directives pattern.
- Layering: `gramdrive-state` is dev-only; verified `check_crate_architecture.py` binds the direction table to [dependencies]/[build-dependencies] only, so the dev-dep is legitimate by design, not by gap.
- Traceability: SYNC-020 row names this task.

## Minor non-blocking observations

1. `gramdrive-state` `chat_list` read is `ORDER BY pinned DESC, sort_order DESC` with no `chat_id DESC` tiebreak, while the machine (and TDLib) tiebreak by id desc. Divergence requires tied (pinned, order) pairs — practically unique in TDLib. Pre-existing state-layer code, outside this diff. If exactness at ties ever matters, add the tiebreak to the state read.
2. A successful `loadChats` response value is accepted without validating `@type == ok` — lenient, harmless.
3. A witnessed chat with a Removed position and no facts costs one wasted `getChat` before exclusion — rare race, harmless.

None of these warrant rework in this task.