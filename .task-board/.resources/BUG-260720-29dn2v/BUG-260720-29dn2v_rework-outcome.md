# BUG-260720-29dn2v — Doc-only rework outcome (LOGBOOK.md regression fix)

## Scope
Rework per BUG-260720-29dn2v_review.md Part 2. **LOGBOOK.md ONLY.** No code, no release.yml change, no tag, no release.

## Problem (reviewer Part 2)
The prior change set overwrote the heading of an unrelated entry: HEAD (2b204e5) carried
`### 0036 — REVIEW: self-hosted runner migration ACCEPTED (TASK-260719-1dwaj8 → done)`.
The working tree replaced that heading with `### 0049 — release.yml: missing Developer ID G2 CA …`
and orphaned the 0036 body (6 bullets) under the 0049 CA heading — two unrelated entries fused,
the runner-migration review record lost as a discoverable heading.

## Fix applied
Split the fused block into two, newest-first per the file convention:
- `### 0049 …` keeps ONLY its 5 CA bullets: ROOT CAUSE / FIX / DECISION pin-by-sha256 / GATE actionlint / LIMIT.
- `### 0036 …` heading re-inserted above 0014, with its original 6 bullets intact
  (DECISION accepted→done / GATE reviewer re-verified / arm64 FINDING / no-residue FINDING /
  architecture-fit FINDING / rust-cache NOTE).

## Verification
- `git diff LOGBOOK.md` vs HEAD = pure **+7-line addition, zero deletions** (the new 0049 block).
- 0036 body diffed against `git show 2b204e5:LOGBOOK.md` → **IDENTICAL** (byte-for-byte restored).
- Heading order verified newest-first: 0049 → 0036 → 0014 → (2026-07-19) 2320 …
- release.yml **NOT touched** (accepted verbatim; last commit 99ad6a9).
- No tag/release performed; pre-existing `v0.1.0` tag left untouched.

## Downstream (human-only, NOT done here)
AC part 2 — live v0.1.0 release green end-to-end (sign → notarize → staple → gh release create on
gramdrive-mac) — remains the documented human-only gate: git tag push (standing-orders forbidden) +
`environment: release` owner approval + outward-facing credential-minting public release. Runbook in
BUG-260720-29dn2v_review.md Part 3.
