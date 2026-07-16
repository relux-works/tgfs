# Flight Logbook

> Institutional memory. Concise, factual, high-signal.
> Newest entries first. One block per insight.

## 2026-07-17

### 0255 — Prefix twin closed: architecture.md and README defer to the owning section
- FIX: `.spec/architecture.md:76` no longer enumerates App Group among identifiers taking the `com.reluxworks.gramdrive.*` prefix. It now states identifiers are "derived from the `com.reluxworks.gramdrive` namespace" and defers to `platform-requirements.md` § Identifier and naming convention "for the exact per-platform forms, including the Apple-mandated App Group prefixes". Closes the 0248 regression.
- FIX: `README.md:15` — same absolute claim relaxed to "derived from the `com.reluxworks.gramdrive` namespace". Summary register, so no enumeration replaces it.
- NOTE: Both fixes remove a normative detail rather than restate it. Two files asserting the same per-platform rule is what produced the 0242/0248 split-fix in the first place; only `platform-requirements.md:15` owns the detail now, so the next correction there has no twin to leave behind. Deferral beats duplication for any claim with platform-mandated exceptions.
- VERIFIED non-defects, left untouched per review-2: `.spec/policies.md:64` (says Bundle/package only — accurate, and pre-existing), `docs/GLOSSARY.md:10` (generic gloss). Same benign-gloss register also confirmed at `decisions.md:28`, `RISK_REGISTER.md:21`, `OPEN_QUESTIONS.md:36` — none enumerate App Group.
- OPEN: `.spec/platform-requirements.md:26` imprecision from 0248 (macOS 15 vs 15+ deployment target) deliberately NOT fixed — the round-2 scope says do not touch that file, and the reviewer marked it non-blocking with no failure path. Left for whoever next edits the section.
- SCOPE: .spec/architecture.md:76, README.md:15
- STATUS: validator OK 201/201 exit 0; stale-name greps clean; no `com.reluxworks.gramdrive.*` enumeration claim survives outside accurate/gloss uses. TASK-260717-3dvved to-review.

### 0248 — Prefix defect fixed in one spec file, survives verbatim in its twin
- REGRESSION: The 0242 fix closed the unsatisfiable App Group prefix rule in `.spec/platform-requirements.md:15` but left the identical claim standing in `.spec/architecture.md:76` — "All shipped bundle, package, **App Group**, and sync-root identifiers use the `com.reluxworks.gramdrive.*` prefix". Both lines were authored by the same task in the same edit (the 0233 entry's own SCOPE names both files).
- ROOT CAUSE of review-2 reject: architecture.md:76 enumerates App Group explicitly, is contradicted by `262RZ595FP.com.reluxworks.gramdrive` in the very section it cross-references, and the paragraph directly below it is § Native layers → macOS instructing the implementer to use an App Group container. Same failure path the 0239 review blocked on, relocated one file over.
- NOTE: General constraint — a normative claim written once tends to be written several times in the same pass. Fixing the instance a review cites does not fix the claim; grep the assertion across `.spec/`, `docs/`, and `README.md` before calling it closed. Here `grep -rniE 'prefix'` surfaced 4 copies, of which 2 were defects, 1 accurate (`policies.md:64` — "Bundle/package" only), 1 a benign gloss (`GLOSSARY.md:10`).
- FINDING: The 0242 "macOS 14+" correction was right and the 0239 reviewer was wrong — DEC-017 (`.spec/decisions.md:26`) and POL-5 (`.spec/policies.md:47`) both say macOS **14+**, not 14. Implementer correctly declined the reviewer's literal wording.
- FINDING: `.spec/platform-requirements.md:26` says the `group.` form applies "once iOS or macOS 15+ enters scope", but macOS 15 is already in scope (matrix is 14+). What is future is a 15+ *deployment target*, not the OS version. Non-blocking — row :23 already says "macOS 14 deployment target".
- SCOPE: .spec/architecture.md:76, README.md:15
- STATUS: validator OK 201/201 exit 0; all stale-name greps clean; F1 rework itself verified good. TASK-260717-3dvved routed to-dev for a 2-line fix. Evidence: TASK-260717-3dvved_review-2.md.

### 0242 — Identifier section fixed: v1 App Group recorded, prefix rule reworded
- FIX: `.spec/platform-requirements.md:15` prefix rule reworded — identifiers are now "derived from the `com.reluxworks.gramdrive` namespace", with App Groups explicitly carrying Apple's mandatory `group.` or team-ID prefix ahead of it. Resolves the unsatisfiable-rule contradiction from the 0239 entry.
- FIX: `262RZ595FP.com.reluxworks.gramdrive` added to the identifier table as the entitlement form v1 ships; `group.com.reluxworks.gramdrive` marked iOS + macOS 15+ / future, not used by v1. Values sourced from TASK-260716-1jswke progress:29, not invented.
- FINDING: The table heading read "**Registered** Apple identifiers", but the team-prefixed group is deliberately NOT portal-registered — adding it under that heading would have created a fresh contradiction. Heading widened to "Apple identifiers" + per-row `Registration` column, so no row inherits a blanket registration claim from a heading. Not flagged by review; surfaced while applying the fix.
- DECISION: Row reads "the entitlement form v1 ships (macOS 14 deployment target)", not the reviewer's literal "macOS 14 v1 entitlement form". The matrix is macOS **14+**, so one build must run on 14 and uses the team-prefixed form throughout; "on macOS 14" could be misread as per-OS-version builds.
- SCOPE: .spec/platform-requirements.md:13-27 (single file; naming rollout untouched — already accepted)
- STATUS: validator OK 201/201 exit 0; stale-name greps match reviewer baseline (8 `tgfs` codename, 1 `tgfiles` in DEC-019 rationale, no wrong-cased gramdrive); repo not renamed. Handed to review. Evidence: TASK-260717-3dvved_rework-f1.md.

### 0239 — macOS 14 v1 App Group must be team-prefixed, not `group.`-prefixed
- FINDING: The two App Group forms are not interchangeable. `group.com.reluxworks.gramdrive` needs portal registration + a provisioning profile, which does NOT work with Developer ID signing; macOS 14 builds must use the team-prefixed `262RZ595FP.com.reluxworks.gramdrive`, which needs no portal registration. Established by TASK-260716-1jswke (human portal round-trip).
- ROOT CAUSE of review reject: `.spec/platform-requirements.md:23` listed only `group.com.reluxworks.gramdrive` as "the" App Group. DEC-017/POL-5 make macOS 14 arm64 the ONLY v1 platform — so the spec omitted the identifier v1 actually ships, and an implementer of PLAT-MAC-003 (V1) would hit the signing wall 1jswke already solved.
- FINDING: `.spec/platform-requirements.md:15` ("every App Group identifier uses the `com.reluxworks.gramdrive.*` prefix") is unsatisfiable by any real App Group — Apple mandates a `group.` or team-ID prefix BEFORE the namespace. The rule contradicts its own table 8 lines later.
- NOTE: General constraint — facts established by human-gated credential/portal tasks decay unless pulled into `.spec/`. Progress notes are not the source of truth; the spec is.
- SCOPE: .spec/platform-requirements.md:15,23
- STATUS: TASK-260717-3dvved routed to-dev for a ~3-line fix; evidence in TASK-260717-3dvved_review.md.

### 0235 — Telegram app title still legacy `memori`, inconsistent with GramDrive
- FINDING: Registered app title on my.telegram.org is `memori` (per TASK-260716-1iypv4 progress), now inconsistent with the accepted public name GramDrive (DEC-019).
- NOTE: Human-only surface (my.telegram.org login); not reachable from the agent loop. Not blocking — the title is not user-facing in v1, but it is visible in Telegram session/authorization lists.
- STATUS: pending owner action; recorded as follow-up in TASK-260717-3dvved_docs.md.

### 0234 — R-015 (name collision) was stale after DEC-019
- FINDING: `docs/RISK_REGISTER.md` R-015 still read "Decide public product/repository naming before external launch; current private repo name is provisional" after DEC-019 had already decided it.
- FIX: R-015 mitigation now cites DEC-019/POL-7 (GramDrive public, `tgfs` private codename) with the residual trademark/handle check called out. docs/RISK_REGISTER.md:21
- NOTE: Risk register has no status column — resolved risks can only be signalled inside the mitigation cell. Worth a `Status` column if more rows get retired.

### 0233 — Naming conventions recorded as prose to avoid traceability ID debt
- DECISION: The `com.reluxworks.gramdrive.*` identifier convention went into `.spec/platform-requirements.md` as a `###` prose section, NOT as a new `PLAT-005` bullet.
- ROOT CAUSE: `.scripts/validate_traceability.py:52` registers a requirement ID from the bullet form `- **PLAT-00N (V1):**` and then fails unless that ID has exactly one row in docs/TRACEABILITY.md mapped to a real board element. Minting an ID would break the validator or force an invented board mapping.
- NOTE: General constraint for future spec edits — adding a requirement-shaped bullet to `.spec/` is never a free doc change; it obligates a matrix row + board element. Prose carries normative content with no ID debt.
- SCOPE: .spec/platform-requirements.md:13, .spec/architecture.md:76

### 0224 — Spec tension: product.md success gate vs POL-5 support matrix
- FINDING: `.spec/product.md` "Product success gates" requires macOS AND Windows for V1 product-completeness; accepted DEC-017/POL-5 commit macOS 14+ arm64 only, Windows deferred.
- SCOPE: .spec/product.md, .spec/policies.md (POL-5), .spec/decisions.md (DEC-017)
- NOTE: Recorded as open question #9 in docs/OPEN_QUESTIONS.md. Reconciling touches an Accepted decision row → owner escalation per POL-8.
- STATUS: pending owner decision.

### 0223 — SEC-051 has no implementing task; mapped to release gate
- FINDING: SEC-051 (never train AI/ML on Telegram content) is a standing negative constraint with no board implementation task.
- DECISION: Mapped to release-readiness review TASK-260715-1nxcst as an enforcement gate; any future analytics/telemetry work must re-verify it.
- SCOPE: docs/TRACEABILITY.md (SEC section)

### 0222 — Requirement coverage matrix baseline (TASK-260715-1czb40)
- MILESTONE: All 201 `.spec/` requirement IDs (PRD 30, DOM 13, SYNC 41, PLAT 32, SEC 28, NFR 29, DEC 20, POL 8) mapped exactly once to board elements in docs/TRACEABILITY.md; 125 board elements referenced.
- FINDING: Dispositions: 166 active (macOS-first), 24 deferred-platform, 10 deferred-optional (remote tier), 1 future (SYNC-063). Zero stale requirement references on the board.
- FIX: `.scripts/validate_traceability.py` (stdlib-only) fails on missing/duplicate rows, orphan board elements, unmapped non-future rows, unjustified multi-mappings, active rows confined to deferred epics, stale board refs. Clean run verified; all failure modes exercised on negative fixtures.
- NOTE: Board READMEs cite requirement ranges ("SYNC-040 through SYNC-054") in only ~12 elements; docs/TRACEABILITY.md is the per-ID authority.
