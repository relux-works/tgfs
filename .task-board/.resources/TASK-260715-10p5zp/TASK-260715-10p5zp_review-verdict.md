# TASK-260715-10p5zp — Review verdict: ACCEPTED

## Verdict
Accepted → done. Implementation matches all acceptance criteria, fits the crate architecture, gates independently re-verified green. No code changes requested.

## AC verification (against code, not just claims)

**1. Crash/replay tests never advance cursor without state.**
`advance_newest` is set in exactly two places in `live.rs`: the bridge connect (`on_bridge_page`, only when `advanced > newest`) and the verified-extension path in `ingest_record` — both attach the advance to the commit carrying the justifying records, and the test caller (`apply_live_commit`) applies records + merged cursor in one transaction. `restart_at_every_commit_boundary_converges_exactly` kills after every commit boundary and asserts `assert_cursor_covered` on the crashed durable state, then converges a fresh machine byte-identically. Reasoning verified sound: durable state mutates only at commit application, so every-commit-boundary coverage IS every distinct durable checkpoint; intra-commit atomicity is the state layer contract (SYNC-022), tested there.

**2. Gaps recover before publication.**
Unverified → Bridging → Verified per chat; pre-connection bridge pages commit under the unchanged cursor; the connecting commit carries the advance to max(bridge top, live top). `gaps_recover_before_the_cursor_is_published` asserts the coverage invariant after every persisted commit and exactly one advance on the last (connecting) commit. Failed bridge → Frozen + one `LiveStep::Degraded` with the crawl vocabulary `UnavailableReason` — cursor honest, records flow boundary-free.

**3. Duplicates are idempotent.**
`duplicate_and_out_of_order_updates_are_idempotent`: doubled news, repeated deletes, delete-of-never-observed (no forged row, POL-3), edit-after-delete (404 counted), then full replay → event log is a fixed point against the real store. Unit tests additionally prove re-pushes never re-advance.

**Checklist: crawl/live boundary.** `in_progress_backfill_and_live_updates_never_lose_state` interleaves a real CrawlMachine with the live loop under the documented merge discipline — final window [1,13], one event per message, no clobber either way. Bridge fold verified as a faithful mirror of the crawl Phase::CatchUp (history.rs:726-752), same connect rule, same empty-page semantics, same page-contract validation.

## Architecture fit
Sans-IO machine, sibling of CrawlMachine/UpdateMachine/FolderCatalogMachine; state only a dev-dependency; module registered in lib.rs docs + re-exports + README table; reuses `history::UnavailableReason` and `error::retryable_after`; no schema change, no new deps. Architecture + traceability gates green.

## Gates (independently re-run by reviewer)
- `make check` → 8/8 ok (toolchain, format, clippy -D warnings, workspace tests, architecture, supply-chain, traceability, scripts); provenance `.temp/acceptance/local-all`.
- Targeted: `live::tests` 24/24, `tests/live_updates.rs` 8/8.

## Non-blocking observations
1. `parse_bridge_page` (live.rs:929) duplicates `parse_entries` (history.rs:805) verbatim (~50 lines, only error-message prefix differs). Deliberate mirror per module docs; a future cleanup could share via pub(crate) with a parameterized prefix. Not worth a rework cycle.
2. Board notes claim 25 unit tests; actual `live::tests` count is 24. Immaterial.
3. Untracked-chat buffers grow unboundedly if the caller never resolves a reported chat — documented caller contract, same stance as UpdateMachine gap reporting. Acceptable.
