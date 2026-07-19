# BUG-260720-116eli — release-attestation-plan-gate — implementation notes

## Problem

Release run 29703775352 (v0.1.0 @ 4ada843) signed, notarized and stapled fine on
`gramdrive-mac`, then died at **Attest build provenance**. The exact API error:

```
##[error]Error: Failed to persist attestation: Feature not available for the
relux-works organization. To enable this feature, please upgrade the billing
plan, or make this repository public. -
https://docs.github.com/rest/repos/attestations#create-an-attestation
```

GitHub artifact attestation is entitled per org plan: a public repo gets it free,
a **private** repo needs a paid plan. `relux-works/tgfs` is private without that
entitlement, so the attest step hard-failed and `gh release create` never ran.

## Fix (design)

The attest step is a `uses:` action; its error text can't be captured to
classify "feature unavailable" vs a real error. So instead of catching the
failure, the workflow **preflights the entitlement** with one controlled call
and gates the attest step on the result.

Key discovery (empirically verified against this repo, both with a `repo`-scoped
PAT and consistent with the org-plan gate): on the create-attestation endpoint
`POST /repos/{owner}/{repo}/attestations`, **the org-plan/entitlement check fires
BEFORE body validation**. So POSTing a deliberately invalid `bundle`:

- creates nothing (the body is not a valid sigstore bundle);
- when the feature is **unavailable** → returns the exact plan message
  ("Feature not available … upgrade the billing plan, or make this repository
  public");
- when the feature is **available** → returns a bundle-validation error (any
  other message).

Probe result on this repo:

```
$ gh api --method POST repos/relux-works/tgfs/attestations -f bundle=invalid
{"message":"Feature not available for the relux-works organization. To enable
this feature, please upgrade the billing plan, or make this repository public.",
 "documentation_url":".../attestations#create-an-attestation","status":"422"}
```

## Changes

### `.github/workflows/release.yml`
1. **New step `Preflight the GitHub attestation entitlement for this repo`**
   (`id: attest_preflight`, before the provenance step). POSTs the invalid-bundle
   probe with `GH_TOKEN`. Degrades **only** on the narrow signature — requires
   BOTH `Feature not available` AND `upgrade the billing plan` in the response.
   Any other outcome (validation error, transport failure, unexpected 5xx) sets
   `attestation_available=true`, so the real attest step still hard-fails on real
   errors. Sets step output `attestation_available` (true/false).
2. **Provenance step** now passes `--attestation-status available|unavailable`
   from the preflight output, so the release manifest records the entitlement.
3. **Attest step** (`id: attest`) gated with
   `if: steps.attest_preflight.outputs.attestation_available == 'true'`. Runs only
   when entitled; unchanged action → still hard-fails on real errors. Skipped
   (not failed) when unavailable, so the job continues.
4. **New step `Record the attestation status in the job summary`** writes the
   provenance decision to `$GITHUB_STEP_SUMMARY` both ways — the produced
   attestation (with its URL) when entitled, or the explicit
   `attestation: unavailable (private-repo plan)` gap when not. Default `if`
   (`success()`) so it runs after a skipped attest but not after a real attest
   failure (where nothing ships).

No other steps changed. "Both attest steps" in the task = the single attest step
covering **two subjects** (the dmg + `release-manifest.json`); gating that one
step covers both subjects.

### `.scripts/release/build_release_provenance.py`
- New `--attestation-status {available,unavailable,unknown}` flag (default
  `unknown`, matching a local `make release-provenance` dry run that never
  touches the API), threaded through `generate()` → `build_release_manifest()`.
- New `build_attestation_record(status)` → the manifest's `attestation` block:
  - `unavailable` → `{"available": false, "status": "unavailable (private-repo plan)", "note": …}`
  - `available`   → `{"available": true, "status": "attested", "note": …}`
  - `unknown`     → `{"available": null, "status": "unknown", "note": …}`
- Note wording deliberately avoids the credential-scrub leak-word/secret-material
  patterns (verified by a test).

### `.scripts/tests/test_build_release_provenance.py`
- New `AttestationStatusTest` (5 cases): unavailable gap, available attested,
  default unknown, unknown-status-value fails loudly, and the note survives the
  credential scrub. `run()` helper gained an `attestation_status` kwarg.

## Verification

- `python3 -m unittest discover -s .scripts/tests -t .scripts/tests -p test_build_release_provenance.py` → **24/24 OK** (19 original + 5 new).
- `make check`-equivalent repo gate: `run_automated.py --suite repo` →
  **2/2 passed** (traceability + scripts).
- `actionlint .github/workflows/release.yml` → **0 errors** (shellcheck 0.11.0 on
  PATH, so the `run:` scripts were linted).
- CLI smoke of the provenance script (real `cargo metadata` + `git`, staged
  minimal package) with `--attestation-status unavailable` and `available` →
  correct `attestation` block, credential scrub `passed` both ways.

## AC coverage / handoff boundary

- ✅ Degrades gracefully ONLY on the feature-unavailable/plan error (narrow
  two-substring match); any other attestation error still fails the release.
- ✅ Release manifest + GitHub job summary record
  `attestation: unavailable (private-repo plan)` when degraded.
- ✅ actionlint clean; no other steps changed; attestation resumes with zero
  config once the plan enables it or the repo is public.
- ⏭️ **Re-point v0.1.0 → fix commit + confirm the release run goes green** is a
  release operation, not developer-role code work: it needs the fix committed/
  merged first (this role does not auto-commit), it force-moves a tag to trigger
  a **real signed+notarized public release**, and it is behind the mandatory
  human approval gate (`environment: release`, POL-8/DEC-020). Handed to review
  to run through that gate after the code fix is accepted.

## Assumption / risk

The preflight relies on GitHub checking entitlement before body validation. If
GitHub ever reversed that ordering, the probe would classify unavailable-as-
available and the release would hard-fail at the real attest step — i.e. degrade
to today's behavior, caught by a red run, not silently ship a worse artifact.
Documented in the step comment.
