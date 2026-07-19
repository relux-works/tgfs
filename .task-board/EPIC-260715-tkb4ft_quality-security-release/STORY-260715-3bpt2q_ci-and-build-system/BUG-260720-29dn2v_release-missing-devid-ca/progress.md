## Status
blocked

## Assigned To
[reviewer] reviewer (claude)

## Created
2026-07-19T20:39:38Z

## Last Update
2026-07-19T21:06:24Z

## Blocked By
- (none)

## Blocks
- (none)

## Checklist
- [x] DeveloperIDG2CA.cer downloaded with pinned sha256 and imported into the throwaway keychain in the same import step; find-identity check >=1
- [ ] Unpublished v0.1.0 tag re-pointed to the fix commit; release run on gramdrive-mac green end-to-end incl. notarization and GitHub Release with dmg asset
- [x] actionlint clean; keychain-sim on relux still clean after run
- [x] Code written per task description and AC
- [x] Relevant tests written for new or changed behavior and passing
- [x] Lint clean
- [x] Relevant build/validation commands run after changes and build not broken
- [x] New outcome artifact attached on the board with a task-scoped name when the work produces notes, logs, screenshots, or other deliverables
- [x] Important findings, decisions, anomalies, or regressions recorded in logbook when relevant
- [ ] Implementation matches AC
- [ ] Solution fits project architecture
- [ ] Tests green
- [x] If review does not accept the work — verdict evidence added and status routed by the explicit verdict branches

## Notes
spawn queued: [implementer] developer (claude) (run=RUN-260719-160178, max_parallel=1)
spawn run started: [implementer] developer (claude) (run=RUN-260719-160178)
FIX READY FOR REVIEW. release.yml: after the .p12 import and before the kept find-identity check, download DeveloperIDG2CA.cer (Developer ID CA G2 intermediate) from apple.com/certificateauthority pinned by sha256 f16cd3c5...df3a (fail closed on mismatch), security import into the SAME throwaway keychain (no -T, no trust override); .cer also rm-d in always() cleanup. Root cause reproduced from run 29702885260: leaf imports but find-identity -v lists 0 valid because relux lacks the G2 intermediate hosted runners preinstall. Validation ALL GREEN: actionlint 1.7.12 clean (all workflows); gitleaks --no-git on release.yml no leaks (the single LOGBOOK.md gitleaks hit is the pre-existing key-roundtrip prose already suppressed by .gitleaksignore fingerprint, not my change); local scratch keychain 1 certificate imported; keychain-sim.sh on relux extended with CA coverage -> SIM OK + G2 present + pin fails closed + zero residue. NOT executed (human/review gated + environment:release owner gate + no auto commit/push per standing orders): tag re-point (delete+retag+push v0.1.0 to fix commit) and live release run. Runbook + evidence in BUG-260720-29dn2v_results.md. Checklist item 2 (green live release) is the post-review human step.
agent completed: [implementer] developer (claude) (exit=0)
spawn run completed: claude (run=RUN-260719-160178, pid=77415, exit=0)
spawn queued: [reviewer] reviewer (claude) (run=RUN-260719-b5dbef, max_parallel=1)
spawn run started: [reviewer] reviewer (claude) (run=RUN-260719-b5dbef)
REVIEW VERDICT: blocked (code fix ACCEPTED, human-only release gate). Independently re-verified, not on faith: (1) fetched DeveloperIDG2CA.cer from apple.com myself -> sha256 EXACT match f16cd3c5...df3a; (2) openssl -> CN=Developer ID Certification Authority OU=G2, issuer Apple Root CA, valid 2021-2031 (correct intermediate); (3) placement correct (after .p12 import, before kept find-identity check); (4) fail-closed proven (shasum --check --status under set -euo pipefail aborts before security import); (5) no new key/secret/trust (import has no -P/-T/no trust override, anchor stays OS Apple Root CA); (6) no residue (.cer rm in-step + in always() cleanup); (7) actionlint 1.7.12 re-run clean exit 0; (8) keychain-sim relux SIM OK, G2 present, pin fails closed, keychain gone, lists restored. NO CODE REWORK REQUESTED. Why blocked not done: AC part 2 (v0.1.0 run green end-to-end sign/notarize/staple/gh release create on gramdrive-mac) is NOT executed and NOT autonomously executable -> requires git tag push (forbidden by standing orders) + environment:release owner approval (human-only) + is an outward-facing credential-minting public release. Decisive proof find-identity -v>=1 with the REAL leaf can only come from the live run; sim uses a dummy self-signed identity not issued by G2 so it cannot assert leaf validity. Safe-failure caveat: fix assumes leaf chains through G2 not G1 (cannot verify, leaf is in MACOS_CERT_P12 secret) BUT if wrong the kept find-identity check fails LOUDLY before signing -> never ships broken artifact. EXACT HUMAN ACTION NEEDED: review diff -> commit fix -> git tag -d v0.1.0 + push :refs/tags/v0.1.0 + retag v0.1.0 <fix-sha> + push -> approve release env -> gh run watch to green -> then flip checklist item 2 and set done. Runbook+evidence in BUG-260720-29dn2v_review.md.
CORRECTION - FINAL VERDICT: to-dev (SUPERSEDES the prior blocked note above). The release.yml CA fix itself stays reviewer-ACCEPTED and fully independently verified: sha256 pin fetched from apple.com = EXACT match; G2 identity correct (issuer Apple Root CA, valid 2021-2031); placement/fail-closed/no-residue correct; actionlint exit 0; keychain-sim clean. NO change requested to the code fix. The rework is a documentation regression the same change set introduced: the LOGBOOK.md edit OVERWROTE the heading of a prior unrelated entry. Confirmed vs git show HEAD:LOGBOOK.md - HEAD has the 0036 entry [REVIEW: self-hosted runner migration ACCEPTED, TASK-260719-1dwaj8 to done]; the working tree replaced that heading with the 0049 release.yml CA heading and orphaned the 0036 body bullets [accepted to done, GATE runner relux-gramdrive online re-verified, arm64 FINDING, no-residue FINDING, architecture-fit FINDING, rust-cache NOTE] under the 0049 CA heading. Two unrelated entries merged; the runner-migration review record lost as a discoverable heading. FIX: restore the 0036 entry intact and add 0049 [the 5 CA bullets] as its OWN block above it, newest-first per the file convention. Do NOT attempt the live release [git tag push forbidden by standing orders + environment:release owner gate + outward-facing credential-minting] - that is the downstream human-only step; after the logbook fix, resubmit to review to coordinate the human release handoff. Full evidence + runbook in BUG-260720-29dn2v_review.md.
agent completed: [reviewer] reviewer (claude) (exit=0)
spawn run completed: claude (run=RUN-260719-b5dbef, pid=85760, exit=0)
spawn queued: [implementer] developer (claude) (run=RUN-260719-63e1a1, max_parallel=1)
spawn run started: [implementer] developer (claude) (run=RUN-260719-63e1a1)
Doc-only rework per BUG-260720-29dn2v_review.md Part 2 complete. LOGBOOK.md ONLY. Split the fused entry: restored ### 0036 (REVIEW: self-hosted runner migration ACCEPTED, TASK-260719-1dwaj8 to done) intact with its original 6 bullets, and kept ### 0049 (release.yml CA) as its own block above it (5 CA bullets: ROOT CAUSE/FIX/DECISION pin-by-sha256/GATE actionlint/LIMIT), newest-first. git diff vs HEAD is a pure +7-line addition, zero deletions -> 0036 body verified byte-identical to HEAD (2b204e5). release.yml NOT touched (accepted verbatim, still at 99ad6a9). No tag/release performed (v0.1.0 pre-existing, left untouched). Part 3 (live v0.1.0 release: sign->notarize->staple->gh release create on gramdrive-mac) remains the documented HUMAN-ONLY gate. Ready for review.
agent completed: [implementer] developer (claude) (exit=0)
spawn run completed: claude (run=RUN-260719-63e1a1, pid=91108, exit=0)
spawn queued: [reviewer] reviewer (claude) (run=RUN-260719-da68b2, max_parallel=1)
spawn run started: [reviewer] reviewer (claude) (run=RUN-260719-da68b2)
REVIEW cycle 2 (BUG-260720-29dn2v_review2.md): doc-only rework ACCEPTED — LOGBOOK.md fix is a pure +7/-0 addition, 0036 restored byte-for-byte identical to HEAD 2b204e5, 0049 CA entry now its own block above it (newest-first order verified). Cycle-1 clobber regression fully repaired. release.yml re-confirmed accepted, untouched by rework; I independently re-fetched DeveloperIDG2CA.cer from apple.com this cycle → pinned sha256 f16cd3c5…df3a EXACT match, correct OU=G2 intermediate. No tag/release performed (v0.1.0 still at 99ad6a9, HEAD unmoved). VERDICT blocked, not done: AC part 2 (live v0.1.0 green end-to-end on gramdrive-mac) + DoD find-identity>=1 are provable ONLY by the live run, which is a human-only gate — git tag push (standing-orders forbidden) + environment:release owner approval + outward-facing credential-minting public GitHub Release. Not rework, not a recoverable failure. Exact human runbook in review2 Part 3; on green flip DoD item 2 → done.
agent completed: [reviewer] reviewer (claude) (exit=0)
spawn run completed: claude (run=RUN-260719-da68b2, pid=92748, exit=0)

## Precondition Resources
- [BUG-260720-29dn2v_rework-scope.md](file://BUG-260720-29dn2v/BUG-260720-29dn2v_rework-scope.md) — Doc-only rework: unmerge logbook entries

## Outcome Resources
- [BUG-260720-29dn2v_spawn-log_-implementer--developer--claude-.log](file://BUG-260720-29dn2v/BUG-260720-29dn2v_spawn-log_-implementer--developer--claude-.log) — System spawn log captured by task-board
- [BUG-260720-29dn2v_results.md](file://BUG-260720-29dn2v/BUG-260720-29dn2v_results.md) — Root cause, release.yml CA fix, validation matrix (actionlint/gitleaks/keychain-sim), and human-gated release runbook
- [BUG-260720-29dn2v_keychain-sim-relux.log](file://BUG-260720-29dn2v/BUG-260720-29dn2v_keychain-sim-relux.log) — keychain-sim on relux (CA-mechanic added): SIM OK, G2 present, pin fails closed, no residue
- [BUG-260720-29dn2v_keychain-sim.sh](file://BUG-260720-29dn2v/BUG-260720-29dn2v_keychain-sim.sh) — keychain-sim.sh extended with Developer ID G2 CA download+pin+import coverage (synced to relux)
- [BUG-260720-29dn2v_spawn-log_-reviewer--reviewer--claude-.log](file://BUG-260720-29dn2v/BUG-260720-29dn2v_spawn-log_-reviewer--reviewer--claude-.log) — System spawn log captured by task-board
- [BUG-260720-29dn2v_review.md](file://BUG-260720-29dn2v/BUG-260720-29dn2v_review.md) — Reviewer verdict: release.yml CA fix ACCEPTED & independently re-verified (sha256 pin matches Apple live cert, G2 identity, actionlint clean, sim clean); to-dev for a LOGBOOK.md regression (0049 edit clobbered the 0036 entry heading, merged two entries); after rework the live release is a human-only gate
- [BUG-260720-29dn2v_rework-outcome.md](file://BUG-260720-29dn2v/BUG-260720-29dn2v_rework-outcome.md) — Doc-only LOGBOOK rework outcome: 0036 restored, 0049 split out
- [BUG-260720-29dn2v_review2.md](file://BUG-260720-29dn2v/BUG-260720-29dn2v_review2.md) — Reviewer verdict cycle 2: doc rework ACCEPTED + release.yml re-confirmed (fresh pin check); blocked on human-only live release gate
