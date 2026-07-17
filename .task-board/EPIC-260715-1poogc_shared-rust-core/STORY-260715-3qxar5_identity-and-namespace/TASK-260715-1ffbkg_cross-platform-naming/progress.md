## Status
done

## Assigned To
[reviewer] reviewer (claude)

## Created
2026-07-15T00:52:44Z

## Last Update
2026-07-17T04:22:35Z

## Blocked By
- TASK-260715-1qz1g5

## Blocks
- (none)

## Checklist
- [x] Sanitizer covers Unicode normalization (NFC), Windows reserved names (CON, NUL, COM1...), path separators, control chars, trailing dots/spaces, emoji-safe truncation to path budgets, case-insensitive collision handling
- [x] Collision suffixes derive from stable identity (never discovery order); no path traversal possible from crafted chat names — negative tests
- [x] Shared fixture corpus with expected outputs for Apple/Windows/Android/Linux passes; POL-1 name format applied
- [x] All quality gates green (make check)
- [x] Code written per task description and AC
- [x] Relevant tests written for new or changed behavior and passing
- [x] Lint clean
- [x] Relevant build/validation commands run after changes and build not broken
- [x] New outcome artifact attached on the board with a task-scoped name when the work produces notes, logs, screenshots, or other deliverables
- [x] Important findings, decisions, anomalies, or regressions recorded in logbook when relevant
- [ ] Implementation matches AC
- [ ] Solution fits project architecture
- [ ] Tests green
- [ ] If review does not accept the work — verdict evidence added and status routed by the explicit verdict branches

## Notes
spawn queued: [implementer] developer (claude) (run=RUN-260717-3085bf, max_parallel=1)
spawn run started: [implementer] developer (claude) (run=RUN-260717-3085bf)
Implemented gramdrive_model::naming (crates/gramdrive-model/src/naming.rs). sanitize() is total; resolve_siblings() derives collision suffixes from ItemId digest, never discovery order (SYNC-012). One name for the strictest target with Platform::check modeling all four faithfully (SYNC-013, PLAT-021). Traversal impossible by construction, proven by property test. Whole-path/MAX_PATH budget delegated to the Windows CfAPI host per PLAT-WIN-004/PLAT-022 (core cannot know the sync-root prefix) - flagged for reviewer. POL-1 formatting moved from tree.rs to naming::chat_folder_name (single source of truth); tree still emits raw names by design, so provider-layer integration is a follow-up. Proptest found a real bug on first run: is_nfc_quick returns Maybe for a leading combining mark, so !=Yes rejected valid NFC; fixed to full is_nfc. New deps unicode-normalization + unicode-segmentation are MIT OR Apache-2.0, inside the POL-6 allow list - no DEC-021 exception needed, cargo deny green. make check 8/8; 83/83 tests in gramdrive-model (37 new). Ready for review.
agent completed: [implementer] developer (claude) (exit=0)
spawn run completed: claude (run=RUN-260717-3085bf, pid=74227, exit=0)
spawn queued: [reviewer] reviewer (claude) (run=RUN-260717-3e979a, max_parallel=1)
spawn run started: [reviewer] reviewer (claude) (run=RUN-260717-3e979a)
REVIEW: changes requested -> to-dev. Independently re-ran make check (8/8) and cargo test -p gramdrive-model (83/83) - all green. Implementation quality is high and docs are excellent; one confirmed correctness defect blocks acceptance. FINDING 1 (blocking, naming.rs:630): fold_key uses str::to_lowercase, and the doc comment claims it is at least as aggressive as the platforms own tables - that claim is FALSE. to_lowercase applies the contextual final-sigma rule (emits ς at word end) while NTFS \$UpCase folds via uppercase (ς->Σ, σ->Σ) and APFS folds via true Unicode case folding (ς->σ); both collapse what to_lowercase keeps apart. Reproduced through the public API: siblings titled ΟΔΟΣ and οδοσ (ordinary Greek word, caps vs lowercase) get NO suffix and resolve to ONE directory on Windows/APFS - one chat silently shadows the other. Same for οδος/οδοσ and dotless-ı/i. This is precisely the failure SYNC-012/SYNC-013 and the DoD item case-insensitive collision handling exist to prevent. Root cause is structural: Platform models character rules and budgets and exposes case_insensitive()->bool, but never models HOW each platform folds, so the corpus structurally cannot catch a fold mismatch. FIX: neither single-direction mapping suffices (to_lowercase misses sigma/dotless-i/sharp-s; to_uppercase misses Kelvin/Angstrom); the round-trip to_uppercase().to_lowercase() catches all with no new dep and over-collides only on ß/ss - the safe direction per the implementers own principle (a spurious collision costs a suffix, a missed one ships a broken tree). A true case fold (caseless) is the more principled option. Keep the .nfc() re-normalization. FINDING 2 (blocking, same root, naming_properties.rs:169): resolved_siblings_never_collide asserts collision-freedom by folding with to_lowercase() - the SAME function the impl uses - so it is tautological and cannot fail for this bug class; it restates the implementation instead of checking it. Should assert against a platform-modeled fold (e.g. a new Platform::fold) shared by corpus and property suite, which also closes Finding 1s structural gap. NON-BLOCKING: (a) resolve_siblings loop bound min(len+2,64) is adequate - escalation chains run in parallel rounds and each extra round needs a distinct crafted sibling, so rounds are bounded by longest chain (<=len), not 3*len; backstop framing is accurate. (b) Lazy paging vs set-relative naming: resolve_siblings needs the whole sibling set while the tree pages lazily, so a consumer naming page 1 must materialize every sibling - inherent to SYNC-012 and correctly flagged as provider-layer, but worth an architecture note before the adapter task since it partly defeats TASK-260715-3tjduq paging. VERIFIED: 53-case corpus; 37 new tests; deps MIT OR Apache-2.0 inside POL-6 allow list, no DEC-021 exception needed, cargo deny green; traceability maps SYNC-012/013+PLAT-021; tree.rs refactor behaviour-preserving. Everything except the fold is right - strip-before-NFC ordering, grapheme truncation, held-back escape byte, identity-digest-over-prefix, suffixing every set member, and the is_nfc_quick bug found by proptest are all correct. Evidence + reproduction: TASK-260715-1ffbkg_review-verdict.md; probe at .temp/TASK-260715-1ffbkg/sigmaprobe/. Rework scope: fix fold_key, re-point the property/corpus assertions at a platform-modeled fold, pin Greek-sigma + dotless-i pairs in the corpus.
agent completed: [reviewer] reviewer (claude) (exit=0)
spawn run completed: claude (run=RUN-260717-3e979a, pid=85144, exit=0)
spawn queued: [implementer] developer (claude) (run=RUN-260717-399a46, max_parallel=1)
spawn run started: [implementer] developer (claude) (run=RUN-260717-399a46)
Rework done, ready for review. Findings 1+2 addressed — but NOT as the verdict recommended, and the reviewer should check this first. MEASURED (coarseprobe over whole char space, caseless as ground truth): the recommended to_uppercase().to_lowercase() round trip has one hole — ẞ (U+1E9E) vs ß/ss. APFS folds all three to ss; the round trip sends ß->ss but ẞ->ß. It would have shipped the same class of defect one codepoint over. True case folding alone also fails: it misses ı/i, which NTFS $UpCase merges. Neither platform fold contains the other, so no single mapping works. SHIPPED: Platform::fold models each platform (Windows=to_uppercase/$UpCase, Apple=full case fold via caseless, Android/Linux=identity/bytes); fold_key composes all four, derived from Platform::ALL like ComponentBudget::strictest. 0 holes vs both folds across the char space, either order, idempotent. Property suite + corpus now fold by Platform::fold. Corpus gained a PAIR-shaped fold table with a hand-written merged_by column — a one-output-per-input table structurally cannot catch a wrong fold. Sigma/dotless-i/ẞ goldens pinned. ALSO MEASURED: re-pointing the broad property at Platform::fold does NOT catch the bug (arb_raw never lands two siblings on one folded name — stays green against the to_lowercase key); the targeted case_variant_siblings_never_collide is what bites. Mutation-verified: both the old key and the recommended round trip FAIL the new suite. New dep caseless 0.2 (MIT, in POL-6 allow list, no DEC-021 exception, cargo deny green). make check 8/8. Artifact: TASK-260715-1ffbkg_rework-results.md
agent completed: [implementer] developer (claude) (exit=0)
spawn run completed: claude (run=RUN-260717-399a46, pid=94317, exit=0)
spawn queued: [reviewer] reviewer (claude) (run=RUN-260717-ced80b, max_parallel=1)
spawn run started: [reviewer] reviewer (claude) (run=RUN-260717-ced80b)
Review round 2: ACCEPTED -> done. Re-verified independently, not read: make check 8/8, 88/88 tests. Finding 1 CLOSED - the fold safety claim (any platform merges => fold_key merges) verified EXHAUSTIVELY over all 1,112,064 codepoints, 0 holes on every platform in BOTH composition orders (probe: .temp/TASK-260715-1ffbkg/foldprobe/). Finding 2 CLOSED - fold corpus is pair-shaped with hand-written merged_by filesystem truth; property suite and corpus fold by Platform::fold; sigma + dotless-i suffix goldens pinned. Mutation-verified myself in a throwaway worktree: the old to_lowercase key FAILS (unit + corpus + goldens), and the previous review OWN recommended to_uppercase().to_lowercase() ALSO FAILS on the Apple-merged pair capital-sharp-s/sharp-s. The implementer was right to reject the reviewer recommendation with measurement; adopting it would have shipped a second shadowed folder. Targeted property measured at 9/10 catches by pure random search, deterministic via the checked-in regression seed. caseless 0.2.2 MIT, in POL-6 allow list, cargo deny green. NON-BLOCKING NIT for the next task touching naming.rs: naming.rs:707 and naming.rs:1006 say the composition gives distinctness for the LAST fold applied - inverted, it is the FIRST (Apple) that is structurally free; the sentence contradicts itself by then correctly naming Windows (the last effective fold) as the non-free half. Comment-only fix, nothing mis-tested. Full verdict: TASK-260715-1ffbkg_review-verdict-2.md
agent completed: [reviewer] reviewer (claude) (exit=0)
spawn run completed: claude (run=RUN-260717-ced80b, pid=3870, exit=0)

## Precondition Resources
- [TASK-260715-1ffbkg_rework-scope.md](file://TASK-260715-1ffbkg/TASK-260715-1ffbkg_rework-scope.md) — Rework: platform-true case folding + non-tautological tests

## Outcome Resources
- [TASK-260715-1ffbkg_spawn-log_-implementer--developer--claude-.log](file://TASK-260715-1ffbkg/TASK-260715-1ffbkg_spawn-log_-implementer--developer--claude-.log) — System spawn log captured by task-board
- [TASK-260715-1ffbkg_results.md](file://TASK-260715-1ffbkg/TASK-260715-1ffbkg_results.md) — Implementation notes: naming policy design decisions, is_nfc_quick bug found by proptest, POL-6 dependency rationale, verification results
- [TASK-260715-1ffbkg_spawn-log_-reviewer--reviewer--claude-.log](file://TASK-260715-1ffbkg/TASK-260715-1ffbkg_spawn-log_-reviewer--reviewer--claude-.log) — System spawn log captured by task-board
- [TASK-260715-1ffbkg_review-verdict.md](file://TASK-260715-1ffbkg/TASK-260715-1ffbkg_review-verdict.md) — Reviewer verdict: changes requested. Confirmed case-fold defect (to_lowercase misses Greek sigma / dotless-i collisions that NTFS+APFS make) + tautological property test. Includes reproduction and recommended fix.
- [TASK-260715-1ffbkg_rework-results.md](file://TASK-260715-1ffbkg/TASK-260715-1ffbkg_rework-results.md) — Rework results: platform-modeled Platform::fold + composed collision key; review's recommended round-trip fix shown insufficient (ẞ/ß hole); mutation-verified; make check 8/8
- [TASK-260715-1ffbkg_make-check-rework.log](file://TASK-260715-1ffbkg/TASK-260715-1ffbkg_make-check-rework.log) — make check 8/8 after rework
- [TASK-260715-1ffbkg_review-verdict-2.md](file://TASK-260715-1ffbkg/TASK-260715-1ffbkg_review-verdict-2.md) — Round-2 review verdict: ACCEPTED. Fold safety claim independently verified 0 holes across all 1.1M codepoints in both composition orders; both mutants caught; one non-blocking doc inversion recorded.
- [TASK-260715-1ffbkg_review2-make-check.log](file://TASK-260715-1ffbkg/TASK-260715-1ffbkg_review2-make-check.log) — Reviewer-run make check: 8/8 green
