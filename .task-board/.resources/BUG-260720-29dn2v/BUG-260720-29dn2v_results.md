# BUG-260720-29dn2v — release-missing-devid-ca — implementation results

## Root cause (confirmed)
First live release on the self-hosted runner `relux` (run 29702885260, tag v0.1.0 @ 99ad6a9)
failed at the "Import the Developer ID identity" step:

```
1 identity imported.
##[error]Process completed with exit code 1.
```

The `.p12` leaf imports fine, but `security find-identity -v -p codesigning` lists **0 valid**
Developer ID Application identities, so `grep -c "Developer ID Application"` prints `0` and exits 1
(fatal under `set -euo pipefail`). Cause: `relux` lacks the Apple **"Developer ID Certification
Authority" (G2)** intermediate that hosted GitHub runners preinstall. Without that intermediate the
leaf never builds a chain to the (system-trusted) Apple Root CA, so `-v` treats it as invalid.

## Fix (`.github/workflows/release.yml`)
Inside the same throwaway-keychain import step, **after** the `.p12` import and **before** the
`find-identity` check, download the public G2 CA from Apple pinned by sha256 and import it into the
SAME throwaway keychain:

- URL: `https://www.apple.com/certificateauthority/DeveloperIDG2CA.cer`
- Pinned sha256: `f16cd3c54c7f83cea4bf1a3e6a0819c8aaa8e4a1528fd144715f350643d2df3a`
- `curl --fail` + `shasum -a 256 --check --status` → **fails CLOSED** on any mismatch (a swapped /
  MITM'd cert never reaches the keychain).
- `security import` with **no `-T`** (public cert, no private key) and **no trust override** (the
  trust anchor stays the OS-trusted Apple Root CA, never this run).
- The `.cer` is removed in-step; also added to the `always()` cleanup `rm -f` (belt-and-suspenders).
  It is public, not a secret, and the throwaway keychain that holds it is deleted in cleanup — so no
  residual certs/secrets outside the run lifetime (AC satisfied).

The existing `find-identity -v -p codesigning | grep -c "Developer ID Application"` check is kept
verbatim.

Cert identity (verified locally):
```
subject=CN=Developer ID Certification Authority, OU=G2, O=Apple Inc., C=US
issuer =C=US, O=Apple Inc., OU=Apple Certification Authority, CN=Apple Root CA
notBefore=Sep 22 2021 GMT   notAfter=Sep 17 2031 GMT
```

## Validation run (all green)
| Check | Command | Result |
|---|---|---|
| Workflow lint | `actionlint` (1.7.12) | CLEAN (all workflows) incl. internal shellcheck of `run:` |
| Secret-scan | `gitleaks detect --no-git --config .gitleaks.toml --source .github/workflows/release.yml` | no leaks found (pinned sha256 does not trip generic-api-key; ci.yml `GITLEAKS_SHA256` is precedent) |
| CA mechanic (local scratch keychain) | curl + sha-pin + `security import` | `1 certificate imported`, cert present |
| Keychain lifecycle on `relux` | `ssh relux 'bash ~/gramdrive-ci/keychain-sim.sh'` | **SIM OK** — keychain GONE, default+search list RESTORED, no residue |
| CA mechanic on `relux` (added to sim) | same sim, new section | `CA import: G2 intermediate present`, `CA pin: fails closed on mismatch`, `CA mechanic: OK` |

`keychain-sim.sh` was extended to cover the CA download+pin+import (positive: G2 present; negative:
wrong pin fails closed). It cannot assert `find-identity -v` validity because it uses a dummy
self-signed identity that is not issued by G2 — only the live run with the real Relux Works `.p12`
proves `>=1` valid identity. Updated sim synced to `relux:~/gramdrive-ci/keychain-sim.sh`.

## NOT executed — needs human go-ahead (review-gated)
The tag re-point + live public signed release was **not** run autonomously. It is an outward-facing,
credential-minting, hard-to-reverse action; standing orders forbid auto commit/stage/push, and the
workflow itself has `environment: release` (owner approval gate). Correct order = review this diff
FIRST, then a human executes:

```bash
# 1. Commit the fix on main (or a branch), then re-point the unpublished v0.1.0 tag:
git tag -d v0.1.0
git push origin :refs/tags/v0.1.0        # delete remote tag (no release was ever published)
git tag v0.1.0 <fix-commit-sha>
git push origin v0.1.0                    # triggers release.yml on gramdrive-mac
# 2. Approve the `release` environment when GitHub prompts (owner sign-off).
# 3. Watch to green: gh run watch <run-id>   (sign → notarize → staple → gh release create)
```

## Files changed
- `.github/workflows/release.yml` — CA download+pin+import before find-identity; CA file added to cleanup rm.
- `.temp/TASK-260719-1dwaj8/keychain-sim.sh` (+ synced to `relux`) — CA-mechanic coverage (untracked runner infra).
