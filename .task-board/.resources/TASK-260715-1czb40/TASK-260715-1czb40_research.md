# Requirement Coverage Matrix — Research Findings

Date: 2026-07-17
Task: TASK-260715-1czb40 (requirement-coverage-matrix), STORY-260715-24cqzd / EPIC-260715-1hm0ka
Deliverables: `docs/TRACEABILITY.md` (matrix), `.scripts/validate_traceability.py` (validation)

## Key takeaways

1. **Full coverage achieved.** All **201** requirement identifiers committed in `.spec/` are mapped to board elements exactly once: PRD 30, DOM 13, SYNC 41, PLAT 32, SEC 28, NFR 29, DEC 20 (including DEC-013..DEC-020), POL 8 (POL-1..POL-8). No requirement lacked a plausible implementing/validating board element; no board element references a nonexistent requirement.
2. **Disposition split:** 166 active (macOS-first committed path), 24 deferred-platform (Windows/Android/Linux/iOS epics per DEC-017/POL-5), 10 deferred-optional (remote tier per DEC-005), 1 future (SYNC-063, write support — intentionally unmapped).
3. **Validation is automated and fails closed.** `python3 .scripts/validate_traceability.py` (stdlib-only) exits non-zero on: missing matrix rows, duplicate rows, requirement IDs absent from `.spec/`, orphan board-element references, unmapped non-future rows, unjustified multiple mappings, active rows mapped only into deferred epics, and stale requirement IDs anywhere in board READMEs. Current run is clean; negative fixtures confirmed each failure mode fires.
4. **Spec tension found (needs owner attention):** `product.md` "Product success gates" requires **macOS and Windows** for V1 product-completeness, while accepted DEC-017/POL-5 commit **macOS 14+ arm64 only** and defer Windows. Recorded as open question #9 in `docs/OPEN_QUESTIONS.md`. Changing either text touches an Accepted decision row, which escalates to the owner per POL-8.
5. **SEC-051 (never train AI/ML on Telegram content) had no implementing element.** It is a standing negative constraint; it is now mapped to the release-readiness review gate (TASK-260715-1nxcst) rather than an implementation task. Any future analytics/telemetry work must re-verify it.

## Method

1. Enumerated definitions from all nine `.spec/*.md` files: bullet definitions (`- **PRD-001 (V1):**` style) for PRD/DOM/SYNC/PLAT/SEC/NFR, decision-table rows for DEC, `## POL-n.` headings for POL. Counts cross-checked by hand against each file section (30+13+41+32+28+29+20+8 = 201).
2. Read all 207 board element READMEs (11 epics, 53 stories, 143 tasks; concatenated dump under `.temp/TASK-260715-1czb40/`) and mapped each requirement to the element(s) whose description/scope/AC implement or validate it.
3. Wrote the matrix to `docs/TRACEABILITY.md` with per-row disposition and per-row justification for every multiple mapping (typical split: engine implementation vs. source adapter vs. platform surface vs. validation harness, or decision record vs. implementing task).
4. Implemented and ran the validation script; iterated to a clean pass; verified failure modes with synthetic negative fixtures (missing row, duplicate row, orphan element, stale board reference, unmapped active row, active row confined to deferred epics).

## Mapping conventions (fact-checked against board content)

- **Range-style scope hints on the board are real but sparse.** Only ~12 board READMEs cite requirement IDs, usually as ranges ("SYNC-040 through SYNC-054" in STORY-260715-2hs8cf, "DOM-001 through DOM-024, PRD-010 through PRD-014" in STORY-260715-3qxar5, "SEC-001 through SEC-034 and SEC-050 through SEC-053" in STORY-260715-mcvwdo, "SEC-040 through SEC-044" in STORY-260715-3bs3wv, "SYNC-001 through SYNC-005" in STORY-260715-255sa3, "PRD-020 through PRD-024 and SYNC-030 through SYNC-034" in STORY-260715-1oq9jg, PRD-022 in TASK-260715-1ynmct, DEC-012/PLAT-IOS-004 in TASK-260715-180uh6, DEC-019 in TASK-260717-3dvved, POL-5/POL-6 in CI tasks). All cited IDs exist in `.spec/` — zero stale references. The matrix keeps per-ID granularity that these ranges lack.
- **POL-n rows and DEC-(n+12) rows intentionally share elements.** POL-1..8 are the detailed form of DEC-013..DEC-020 (stated in `policies.md` and each decision row); both map to the decision-record task plus the implementing task. This is a documented justified duplication, not an accident.
- **Deferred-platform requirements remain fully decomposed.** Windows (5), Android (5), Linux (4), iOS (6) PLAT requirements plus NFR-023, PRD-063, DEC-006, DEC-012 all have live board elements inside their deferred epics; nothing was dropped, only sequenced behind DEC-017/POL-5 scope entry.

## Fact-check notes

- Board element existence for all 125 referenced IDs is machine-verified against `.task-board/` directory names on every script run (this is how two early transcription errors would have been caught; the clean run proves none remain).
- Decision statuses verified against `decisions.md` as of 2026-07-17: DEC-001..004, 006..010, 013..020 Accepted; DEC-005, DEC-011 Provisional; DEC-012 Open release gate. Decision-record tasks TASK-260715-240bpy, 287x8t, 2cl112, 3prhsi, 2weglw, 3ox001, 3rhlh6, 7pdgft are `done` on the board, matching the Accepted rows for DEC-013..DEC-020.
- The summary table in `docs/TRACEABILITY.md` (166/24/10/1) matches the script's independently computed output.
- Board totals cited here (11/53/143) come from `task-board q 'summary()'` on 2026-07-17; README's "142 atomic tasks" predates TASK-260717-3dvved and is one behind — not corrected here because the board is the live source.

## Sources

- `.spec/product.md`, `.spec/domain-model.md`, `.spec/sync-and-filesystem-semantics.md`, `.spec/platform-requirements.md`, `.spec/security-and-privacy.md`, `.spec/quality-and-release.md`, `.spec/decisions.md`, `.spec/policies.md`, `.spec/README.md` (requirement conventions, change control)
- `.task-board/**/README.md` (207 elements) via CLI queries and read-only dump
- `task-board q 'summary()'`, `task-board q 'list(...)'` outputs, 2026-07-17
