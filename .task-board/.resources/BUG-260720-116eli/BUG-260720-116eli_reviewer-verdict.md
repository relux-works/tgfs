# BUG-260720-116eli — reviewer verdict — release-attestation-plan-gate

**Reviewer:** reviewer (read-only). **Date:** 2026-07-20.
**Verdict:** code **ACCEPTED as correct + complete**; task routed **`blocked`** on the
human-only gated release operation (final two AC clauses cannot be executed by any agent role).

---

## What was reviewed

Diff (working tree, uncommitted):
- `.github/workflows/release.yml` — attestation preflight + gating + job-summary
- `.scripts/release/build_release_provenance.py` — `--attestation-status` + manifest `attestation` block
- `.scripts/tests/test_build_release_provenance.py` — new `AttestationStatusTest` (5 cases)
- `LOGBOOK.md`, board `progress.md` bookkeeping

## Code review — ACCEPTED

**Design is sound.** A `uses:` action's error text can't be captured, so catch-and-degrade
is impossible; preflighting the entitlement with one controlled `POST .../attestations`
(invalid bundle → org-plan check fires before body validation → creates nothing) is the
right shape. Empirically verified against this repo by the implementer.

**AC #3 (hard-fail on real errors) — MET.** Degrades ONLY on the narrow two-substring
signature (`Feature not available` AND `upgrade the billing plan`). Every other outcome —
validation error, transport failure, 5xx — leaves `attestation_available=true`, so the real
attest step still hard-fails. Fail-closed: a transient probe failure degrades to today's
red-run behavior, never a silently-worse artifact.

**Gating correct.** Attest step `if: ...attestation_available == 'true'` → skipped (not
failed) when unavailable. Job-summary step default `success()` → runs after a skipped
attest, skipped after a *real* attest failure (nothing ships then). Publish also default
`success()`. Step order: preflight → provenance build → attest → summary → publish. Coherent.

**AC #2 (record the gap) — MET.** Manifest `attestation` block + `$GITHUB_STEP_SUMMARY`
both record `unavailable (private-repo plan)` both ways. Note wording avoids the
credential-scrub patterns (test-guarded).

**Fits architecture.** `--attestation-status` threaded generate() → build_release_manifest()
→ build_attestation_record(), consistent with the existing script. `unknown` default = local
`make release-provenance` dry run that never touches the API. Degrade-and-record philosophy
matches the prior release-bug fixes.

**Gates re-verified independently by reviewer:**
- `python3 -m unittest ... test_build_release_provenance.py` → **24/24 OK**
- `actionlint .github/workflows/release.yml` → **clean** (shellcheck 0.11.0 on PATH)
- Job permissions block has `attestations: write` + `id-token: write` → preflight probe can
  actually reach the endpoint and read the plan/validation error.
- No unrelated steps changed (signing, notarization, publish, cleanup untouched).

**Residual risks (documented, acceptable):** (1) probe assumes entitlement-check-before-
validation ordering — if GitHub reversed it, mis-classify → hard-fail at real attest (=red
run), never silent; (2) invalid `bundle` string can't create an attestation on an entitled
repo (GitHub validates it's a sigstore bundle). Both documented in-code.

## Why `blocked` and not `done`

Task AC has four clauses. Two are code-verified above. The other two require **running the
release**:
- "Release run on gramdrive-mac completes green end-to-end..."
- "v0.1.0 GitHub Release exists with the dmg asset."

Neither is true, and neither can be executed by the developer OR reviewer role, because it
requires:
1. The fix committed/merged — it is currently **uncommitted working-tree changes**; the role
   does not auto-commit.
2. Force-moving the unpublished `v0.1.0` tag onto the fix commit — an outward-facing,
   hard-to-reverse action that triggers a **real signed + notarized public release**.
3. Passing the **mandatory `environment: release` human approval gate** (POL-8 / DEC-020,
   release.yml:16–21, 72) — owner sign-off, a repo-admin protection a workflow cannot grant
   itself.

This is a genuine human-only approval + external-action boundary — exactly the stop-the-line
case. Marking `done` would falsely assert the v0.1.0 Release exists.

## Exact human input needed to close

1. Review + commit + merge the fix to `main`.
2. Force-move `v0.1.0` → the fix commit (`git tag -f v0.1.0 <sha> && git push -f origin v0.1.0`).
3. Approve the `environment: release` gate when the run pauses for owner sign-off.
4. Confirm the run goes green end-to-end through `gh release create`, the job summary shows
   `attestation: unavailable (private-repo plan)`, and the v0.1.0 Release carries the dmg asset.

After that green run, this task is `done`. No code rework is required.
