# TASK-260715-22l8zy — Review verdict: ACCEPTED

Reviewer: reviewer (claude). Read-only review of the full change set.

## Verdict
ACCEPTED → done. AC and DoD met; implementation matches the task, fits the
project architecture, and all gates are green (re-run independently, not
trusted from the results doc).

## Independent verification (re-run by reviewer)
- `make check` → 8/8 (toolchain, format, lint, test, architecture, supply-chain, traceability, scripts).
- `cargo test -p gramdrive-engine --test render_plan` → 11/11.
- `cargo test -p gramdrive-render` → 14/14 (civil refactor byte-preserving; goldens unchanged).
- `cargo clippy -p gramdrive-engine -p gramdrive-render --all-targets` → clean.

## AC → evidence (verified)
- **Only affected partitions regenerate** — `affected_documents` = whole-chat NDJSON (any change) + the transcript of each touched civil month only (BTreeSet dedup, deterministic order); `staleness` skips docs already current at the target watermark/versions. Proven by `plan_for_changes_reports_only_affected_partitions_as_new`, `edits_and_deletes_regenerate_only_their_month`, `a_new_month_is_added_without_disturbing_existing_months`, `month_boundaries_follow_the_render_timezone`.
- **Interrupted regeneration leaves old valid version / resumes safely** — planner never publishes; delegated to `state::publish_render`'s in-transaction watermark re-check. A crashed render keeps the prior `content_version` readable and the doc dirty. Proven by `an_interrupted_regeneration_keeps_the_previous_version_and_resumes`.
- **No partial/stale file published** — a publish that raced newer events lands `clean=false` and stays on the worklist to be re-planned at the newer watermark. Proven by `a_render_that_races_newer_events_stays_on_the_worklist`.
- Version bumps + watermark drift → `a_stale_renderer_version_replans_the_document`, `a_change_beyond_the_published_watermark_replans_the_document`. Idempotent replan → `dirty_affected_feeds_the_worklist_and_a_clean_publish_converges`. Identity consistency → `the_catalog_ids_match_the_renderers`.

## Architecture fit (verified)
- Stateful planner correctly lives in `gramdrive-engine` (render stays pure, model-only, cannot read `render_state`/watermarks). Render exposes only the shared `civil` calendar; planner reuses `year_month()` so it never disagrees with the Markdown day-grouping about a month boundary. `check_crate_architecture` gate passes with the new engine→render dep.
- `ItemId` round-trips reliably: it carries the typed `ItemKey` alongside bytes, and `dirty_render_items` reparses via `ItemId::parse_bytes`, so `plan_worklist`'s `item.key()` decode is infallible and correct.
- civil hoist (`markdown/text.rs` → `civil.rs`) is a verbatim, byte-preserving move; render goldens unchanged.
- `chat.json` (Json) deliberately out of catalog; `DocClass::for_key` returns `None`, leaving a future dirty chat.json for its own planner rather than mis-rendering it.

## Notes (not blocking — downstream driver/applier contract)
- For deletes, the applier must pass the deleted message's original **send instant** (not `observed_at_ms`) in `touched`, so the correct month is picked. Documented on `affected_documents`/`dirty_affected` and honored by the tests.
- `plan_for_changes` can name a `New` job for a doc whose `render_state` row does not exist yet; publishing requires the row, which the standard `dirty_affected` (txn-atomic with the change, SYNC-022) path creates. Consistent and documented.
- Full render *driver* (payload decode → bytes → publish) is legitimately downstream/out of scope; the atomicity/resume AC is nonetheless proven end-to-end against the real `StateStore`, not a mock — no forced fit.
