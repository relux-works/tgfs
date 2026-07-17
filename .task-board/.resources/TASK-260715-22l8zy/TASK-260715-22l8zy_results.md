# TASK-260715-22l8zy — Incremental render planning — results

Status: ready for review (board: to-review).

## What was built

`gramdrive-engine::render_plan` — the incremental render planner. From a batch
of normalized message changes and the frozen renderer/schema versions, it
computes exactly which generated documents went stale and a plan to regenerate
only those, against the chat's current event watermark.

### Files

- `crates/gramdrive-engine/src/render_plan/mod.rs` — module docs, `RenderPlanError`, re-exports.
- `crates/gramdrive-engine/src/render_plan/catalog.rs` — `DocClass` catalog (the two live document classes) reading identities, versions and content-version tokens straight from `gramdrive-render`.
- `crates/gramdrive-engine/src/render_plan/plan.rs` — `affected_documents`, `dirty_affected`, `plan_for_changes`, `plan_worklist`, `RenderJob`, `RenderPlan`, `RenderReason`.
- `crates/gramdrive-engine/tests/render_plan.rs` — 11 integration tests.
- `crates/gramdrive-render/src/civil.rs` — `Civil` hoisted out of `markdown/text.rs`; new public `civil::year_month(instant_ms, offset_seconds)` (single source of truth reused by the Markdown day-grouping and the planner).
- Cargo/lib wiring: engine now depends on `gramdrive-render` (was "allowed, not yet used"); README updates for both crates.

### Public API (engine)

- `affected_documents(chat, touched, timezone) -> Vec<GeneratedDocKey>` — pure; whole-chat NDJSON (any change) + each touched civil month's transcript (only), dedup, deterministic order.
- `dirty_affected(write_txn, chat, touched, timezone) -> Result<Vec<ItemId>, StateError>` — ensures + marks the stale set on the durable dirty worklist; call in the change's own transaction (SYNC-022). Requires the generated-doc items projected (tree builder).
- `plan_for_changes(read_txn, chat, touched, timezone) -> Result<RenderPlan, RenderPlanError>` — change-driven plan, skips already-current docs.
- `plan_worklist(read_txn, limit) -> Result<RenderPlan, RenderPlanError>` — drains the crash-durable dirty worklist into jobs.
- `RenderJob { document, chat, partition, format, class, target_watermark_seq, content_version, reason }`.

## Key decisions

- **Planner lives in the engine, not render.** The render crate is pure
  (model-only) and cannot read `render_state`/watermarks. Render exposes only
  the shared `civil` calendar; the planner reuses it so it never disagrees with
  the Markdown day-grouping about a month boundary. A test asserts
  `DocClass::document_id` is byte-identical to `ndjson::/markdown::document_id`.
- **Layout fixes per-format granularity** (`.spec` tree): `messages.ndjson` is
  whole-chat (`DocPartition::Chat`); `YYYY/MM.md` is monthly
  (`DocPartition::Month`). No year-level generated document exists in v1.
- **`chat.json` (`DocFormat::Json`) is deliberately out of the catalog** — its
  renderer is a separate task. `DocClass::for_key` returns `None` for it, so a
  future dirty `chat.json` is left for its own planner, not mis-rendered.
- **Atomicity/resume is delegated, not re-implemented.** The planner never
  publishes. `state::publish_render`'s in-transaction watermark re-check is what
  makes an interrupted regeneration safe: the prior `content_version` stays
  readable, the doc stays dirty, and a raced publish lands `clean=false` and is
  re-planned at the newer watermark.

## Acceptance criteria → evidence

- Only affected partitions regenerate → `a_change_batch_maps_to_the_whole_chat_ndjson_and_its_months`, `month_boundaries_follow_the_render_timezone`, `plan_for_changes_reports_only_affected_partitions_as_new`, `edits_and_deletes_regenerate_only_their_month`, `a_new_month_is_added_without_disturbing_existing_months`.
- Renderer/schema version bump forces re-render → `a_stale_renderer_version_replans_the_document`.
- Watermark drift caught → `a_change_beyond_the_published_watermark_replans_the_document`.
- Interrupted regeneration leaves old valid version / resumes safely → `an_interrupted_regeneration_keeps_the_previous_version_and_resumes`.
- No partial/stale file published (raced publish stays on worklist) → `a_render_that_races_newer_events_stays_on_the_worklist`.
- Idempotent re-planning converges → `dirty_affected_feeds_the_worklist_and_a_clean_publish_converges`.
- Identity consistency with renderers → `the_catalog_ids_match_the_renderers`.

## Scope boundary

The full render *driver* (decode `message_events.payload` → `MessageHistory` →
bytes → publish) is downstream and out of scope: no payload decoder exists yet.
This task delivers the planning + dirty-marking layer that sits in front of
`gramdrive-render`'s pure renderers and `gramdrive-state`'s atomic publication.

## Verification

- `make check` → 8/8 (toolchain, format, lint, test, architecture, supply-chain, traceability, scripts).
- `cargo test -p gramdrive-engine --test render_plan` → 11/11.
- `cargo test -p gramdrive-render` → green (civil refactor byte-preserving; NDJSON/Markdown goldens unchanged).
- Zero new dependencies.
