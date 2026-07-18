# TASK-260715-mua1ng — Rework review (citation fix): ACCEPTED → done

**Verdict: ACCEPTED.** The doc-only citation rework requested by the prior
review (`TASK-260715-mua1ng_review-notes.md`) is complete, correct, and behavior-
neutral. All quality gates green on an independent reviewer re-run.

## Blocking defect — FIXED and verified

The SYNC-041 pause miscitation is gone from every site and re-grounded on the
real requirements.

- **Zero SYNC-041** remains in any backfill/pace/schema/test/results site
  (`grep` over `backfill/`, `repo/backfill.rs`, `schema/v1.sql`,
  `backfill_scheduler.rs`, `results.md` → no match).
- Every remaining SYNC-041 in the repo is **legitimate ranged-fetch**
  (`transfers.rs:463`, `fetch/*`, `transfer/ranges.rs`, `download.rs`,
  `engine/README.md:21`, testkit conformance). None touch backfill.
- Re-grounded citations verified **semantically against spec text**:
  - SYNC-043 = "Cancellation … leaves resumable or safely disposable state"
    (`.spec/sync-and-filesystem-semantics.md:59`) ✓
  - SYNC-005 = "long work is cancellable or converted into durable
    background/transfer state" (`:12`) ✓
  - Present at all sites: `mod.rs:533-535` (`set_paused` doc),
    `repo/backfill.rs:33-34,64` (paused field + read doc, NFR-033 kept),
    `schema/v1.sql:514-515` (paused col), `backfill_scheduler.rs:503`,
    `state/README.md` backfill_control row, `results.md:56`.

## Optional tightenings — applied and correct

- **SYNC-020** now cited only for metadata-first / no-eager-media (its true
  scope, `.spec/…:38`); visible-item priority re-attributed to the task
  description. Verified: no SYNC-020 on any priority-ladder line.
- **POL-8 restart-durability stretch → NFR-031 / SYNC-070**, ban-risk/re-hammer
  clause re-homed on NFR-033. Verified against spec:
  - NFR-031 = "progress survive process restart" (`quality-and-release.md:36`) ✓
  - SYNC-070 = "Startup recovery …" (`sync-…:81`) ✓
  - NFR-033 = "flood waits never become tight retry loops" (`:38`) ✓
  - Applied at `pace.rs`, both module headers, `v1.sql:508`,
    `state/README.md`, `engine/README.md:73,84`, `results.md:48`.
  - Historical LOGBOOK entries (0900/0945) correctly keep POL-8 — a journal
    is not rewritten.

## Doc-only confirmed

`v1.sql` diff is comment + the (already-accepted) `backfill_control` table; all
source edits are comment-only. No logic, no signatures, no test assertions
changed.

## Independent verification

- `make check` **8/8 green** (reviewer re-run, provenance
  `.temp/acceptance/local-all`, exit 0): toolchain, format, lint `-D warnings`,
  workspace test, architecture, cargo-deny, traceability, scripts. The `test`
  step — which sometimes hits the flaky model proptest below — passed clean.

## Carry-over (NOT this task — needs its own owner)

`gramdrive-model` `naming_properties::sanitize_is_idempotent` is a genuine
pre-existing bug: `sanitize(sanitize(x)) != sanitize(x)` for a combining-mark
input (`/`→`_` leaves a trailing `\u{301}` re-processed differently). Proptest
is seed-random, so it surfaces intermittently. Correctly NOT fixed here
(doc-only scope; model is the lowest layer, cannot depend on this task's edits;
under concurrent editing). Reproducing seed preserved in `LOGBOOK.md` (1055) and
`.temp/acceptance/local-all/test.log`; the auto-generated proptest-regressions
byproduct was reverted to keep the diff doc-only. **Recommend a separate
model-crate bug/task for the sanitizer idempotency defect** — surfaced here so
it is not lost, but out of scope for this task's verdict.
