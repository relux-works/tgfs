# Flight Logbook

> Institutional memory. Concise, factual, high-signal.
> Newest entries first. One block per insight.

## 2026-07-17

### 0342 — Review #2: arch-check rework accepted, task done (TASK-260715-3o8wpt)
- DECISION: rework accepted → done. All gates independently re-run on the real tree (build / test 14 suites 10 passed / fmt / clippy / deny / arch-check / `make check`), board harness re-run reproduces its log 1:1, and 10/10 reviewer probes beyond the harness behaved as claimed — incl. target-gated dev/build deps, plain-triple `[target.x86_64-pc-windows-msvc.dependencies]` (no cfg syntax at all), renamed target-gated dep, banned dep in dev section. Scope honored: only the check script + `crates/README.md` changed (mtime-verified), no probe leftovers in `crates/`.
- FINDING: `target_arch`/`target_env` predicates (e.g. `cfg(target_env = "msvc")`) are outside `PLATFORM_PREDICATE_RE` — consistent with the documented scope and review #1's required set, so not a defect; becomes worth adding only if arch/env-gating turns into a real leakage vector.
- NOTE: `TASK-260715-3o8wpt_negative-check-harness.py` resolves the repo root as `parents[2]` of its own path — it only runs from exactly two levels below the repo root (e.g. `.temp/<dir>/probe.py`); a copy run from its board-resource location fails on `fresh_copy()`.
- SCOPE: review-only; evidence `TASK-260715-3o8wpt_review2-verdict.md` + `TASK-260715-3o8wpt_review2-probes.log` (board), `.temp/TASK-260715-3o8wpt/review2/`.
- STATUS: TASK-260715-3o8wpt done; cross-target builds remain TASK-260715-2cn768.

### 0352 — Arch-check enforcement gaps closed; scan is now fail-closed (TASK-260715-3o8wpt)
- DECISION: cfg detection scans the **balanced-paren argument span** of every `cfg`/`cfg!`/`cfg_attr` invocation, not the line. Chosen over the reviewer's suggested line match (framed as "at minimum"): strictly stronger — also catches predicates rustfmt wraps across lines, which a line match misses — and *fewer* false positives, since the predicate must sit inside the cfg args. ~15 lines.
- DECISION: platform-neutral crates now error on **any non-null `target`** on a `cargo metadata` dep (any section, dev included), not on a dep-name list. A target-gated dep is leakage regardless of name, so this also covers external crates the ban list has never heard of.
- DECISION: added a `std::os::` source scan (reviewer's optional item). `std::os::unix::...` compiles per-platform with no cfg and no dep, so neither of the above sees it. Genuine cross-target builds remain the real gap → TASK-260715-2cn768, now named as a limitation in `crates/README.md` instead of implied away.
- FINDING: scans are deliberately fail-closed and this is now documented, not incidental — a predicate word in a block comment or string literal is flagged (false positive = one rename; miss = the platform-neutrality guarantee). Known residual miss: `//` inside a string literal truncates the line before its predicate.
- NOTE: `crates/README.md` "Everything in this document is enforced by ..." was false in *both* directions — it also implied the script owned the license gate. Now scoped, with the un-enforced conventions (sources-as-crates DEC-003/DEC-005, no-cargo-features baseline, layer numbering, cross-target) listed explicitly. Doc claims about enforcement are worth auditing against the checker, not just the checker against the docs.
- NOTE: negative evidence is now a reproducible harness (`TASK-260715-3o8wpt_negative-check-harness.py`), not hand-run injections: 12 violation cases + 2 controls, scratch-copy tree. Controls matter — they prove the fail-closed scan does not fire on the crates' own `//!` docs, which do name `cfg(target_os/windows/unix)` in prose.
- SCOPE: .scripts/check_crate_architecture.py, crates/README.md (no product code touched — workspace accepted as designed)
- STATUS: all gates green (build/test/fmt/clippy/deny/arch-check); to-review.

### 0328 — Review: arch-check script has 5 confirmed enforcement bypasses (TASK-260715-3o8wpt)
- FINDING: `.scripts/check_crate_architecture.py` cfg scan (check 7) misses 4 real predicate forms in platform-neutral crate sources: `#[cfg(all(unix, ...))]`, `#[cfg(not(windows))]`, `#[cfg_attr(windows, ...)]`, `cfg!(target_os = ...)` — regex only handles `cfg(` with optional `any(`. Verified: all 4 injected into gramdrive-model → check passes, exit 0.
- FINDING: manifest-level platform gating invisible — a `[target.'cfg(target_os = "macos")'.dependencies]` section in a neutral crate passes (cargo metadata dep `target` field ignored). Verified same way.
- NOTE: contradicts the script docstring and `crates/README.md` "Everything in this document is enforced by" claim; positive gates (build/test/fmt/clippy/deny/arch/direction/cycles/README sections) all independently re-verified green. Probes: `.temp/TASK-260715-3o8wpt/review-probe-01.log`, board artifact `TASK-260715-3o8wpt_review-probe.log`.
- STATUS: verdict to-dev — strengthen regex + flag non-null dep `target`, extend negative checks; workspace itself accepted as designed.

### 0323 — Shared-core Rust workspace skeleton stood up (TASK-260715-3o8wpt)
- MILESTONE: First product code. Cargo workspace (root `Cargo.toml`, edition 2024, Rust 1.91, resolver 3) with 7 crates under `crates/`: gramdrive-{model,source,state,render,engine,ffi,testkit}. Layering doc `crates/README.md`; per-crate READMEs with Ownership + Test command; build/test/clippy/fmt green on macOS arm64.
- DECISION: Source implementations are separate crates, not feature flags (DEC-003/DEC-005) — features unify transitively across a workspace, a feature-gated tdjson would leak TDLib linkage into builds that never asked for it. Reserved: `gramdrive-source-tdjson`, `gramdrive-source-remote`.
- DECISION: Crate names use `gramdrive-*` (POL-7), not the tgfs codename — crate names leak into shipped artifacts (`libgramdrive_ffi.dylib`, symbol names), where tgfs is forbidden.
- DECISION: `deny.toml` allow-list is exactly the POL-6 set (MIT/Apache-2.0/BSD-2/BSD-3/BSL-1.0/Zlib/ISC), fail-closed: even permissive licenses outside it (Unicode-3.0 arrives with syn/unicode-ident) fail `cargo deny check licenses` until an owner decision row exists. First serde/proc-macro dependency will trip this by design.
- NOTE: Architecture enforced by `.scripts/check_crate_architecture.py` (member set = policy table, direction allow-list, no cycles, testkit dev-only, platform-dep ban + cfg scan in core crates, README sections). Negative-tested: 4 injected violation classes all fail correctly (`TASK-260715-3o8wpt_negative-checks.log` on the board).
- NOTE: `cargo remove` gc's unused `[workspace.dependencies]` entries (render/ffi/testkit were dropped during a negative test and restored) — watch for silent manifest churn after cargo add/remove.
- SCOPE: Cargo.toml, deny.toml, Makefile, crates/*, .scripts/check_crate_architecture.py, README.md
- STATUS: to-review; toolchain pinning → TASK-260715-2cn768, UniFFI wiring → TASK-260715-265gqq.

### 0305 — Telegram API compliance checklist baselined (TASK-260715-pyqm1k)
- MILESTONE: `docs/TELEGRAM_API_COMPLIANCE.md` — 22 rules (TGC-01..22) extracted verbatim from core.telegram.org primary sources (terms, obtaining_api_id, content-protection, takeout + method page, errors, sponsored-messages; all fetched 2026-07-17), each mapped to owning board tasks or an explicit gap.
- FINDING: Most ToS obligations already have owners — branding POL-7, protected content POL-4/SEC-032, flood pacing SEC-031/NFR-033, api_id hygiene SEC-001/003/NFR-053, AI-training ban SEC-051, GPL avoidance POL-6. Checklist largely confirms coverage rather than discovering it.
- NOTE: 4 gaps need orchestrator action: G-1 ToS 2.2 "uses Telegram API" disclosure ACs (TASK-260715-13pxnu/32gjo8/1dk9ik); G-2 read-state-neutrality AC — crawl must never emit viewMessages/openChat (TASK-260715-26dnp6/10p5zp, verified via 3e8q4m); G-3 sponsored messages (below); G-4 breach-notice/ban-recovery ops runbook (TASK-260715-32gjo8).
- SCOPE: docs/TELEGRAM_API_COMPLIANCE.md, README.md, .spec/README.md (index rows)
- STATUS: validator OK 201/201; all 34 cited board IDs verified to exist; task to-review.

### 0304 — Protected-content "copying disabled" covers exported TEXT, not just media
- FINDING: api/content-protection requires "forwards, downloads, copying, screenshots must be disabled" for all messages in noforwards chats. Writing protected-chat message text into NDJSON/Markdown exports is copying — so text export must be excluded for protected chats, same as media. Official Telegram Desktop export behaves this way.
- NOTE: Does not contradict accepted POL-4 ("text is exported only where Telegram permits") — it resolves what "permits" means: nothing, for protected chats. Recorded as normative reading (TGC-15, fact-check F-2) unless owner overrides. Implementers: TASK-260715-23arcu, renderers STORY-260715-1oq9jg.
- STATUS: flagged for reviewer attention in the checklist.

### 0303 — Sponsored messages (ToS 3.3) is the one open compliance decision — owner call
- FINDING: ToS 3.3: apps that allow "accessing content from Telegram channels" must support official sponsored messages. GramDrive exposes channel media/history as files → clause arguably triggered, but documented mechanics (getSponsoredMessages "each time the user opens a channel", 5-min cache, view/click reporting) presuppose a chat-feed UI that a filesystem projection lacks.
- DECISION NEEDED: no board task owns this. Proposed: decision task under STORY-260715-1rmrtu producing a DEC row; POL-8 escalation (ToS risk beyond approved behaviors). Recommended position: reasoned non-applicability for v1 (no channel-feed surface), owner sign-off, re-evaluate if any feed-like UI appears; blocks the public-release gate (TASK-260715-1nxcst) until decided.
- STATUS: pending owner decision; analysis in docs/TELEGRAM_API_COMPLIANCE.md § Gaps G-3.

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
