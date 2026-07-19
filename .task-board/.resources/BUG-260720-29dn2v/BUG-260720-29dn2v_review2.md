# BUG-260720-29dn2v — release-missing-devid-ca — REVIEWER VERDICT (rework cycle 2)

**Verdict:** `blocked` — the doc-only rework is CORRECT and the release.yml CA fix is
re-confirmed ACCEPTED. All autonomous work is complete and green. The ONLY remaining item is
AC part 2 — the live v0.1.0 signed/notarized public release on gramdrive-mac — which is a
genuine human-only gate (standing-order-forbidden git push + `environment: release` owner
approval + outward-facing credential-minting release). It cannot be closed autonomously.

---

## Part 1 — doc-only rework (the requested change): ACCEPTED, independently verified
Requested (BUG-260720-29dn2v_rework-scope.md): LOGBOOK.md only — restore the `### 0036`
entry intact, add the `### 0049` CA entry as its own block above it (newest-first), do NOT
touch release.yml, do NOT tag/release.

| Claim | How I checked | Result |
|---|---|---|
| Pure addition, no clobber | `git diff --numstat LOGBOOK.md` | **7 added / 0 deleted** ✅ |
| 0036 restored intact | `diff` of `0036…0014` range: `git show HEAD:LOGBOOK.md` vs working tree | **byte-for-byte IDENTICAL** ✅ |
| 0049 is its own block above 0036 | `grep -nE '^(## \|### )' LOGBOOK.md` | order `0049 → 0036 → 0014 → (07-19) 2320…` newest-first ✅ |
| 0049 content complete | read the block | 5 bullets ROOT CAUSE / FIX / DECISION pin-by-sha256 / GATE actionlint / LIMIT ✅ |
| release.yml untouched | `git log` (HEAD still 2b204e5), `git diff release.yml` | CA fix unchanged from accepted state, no rework edits ✅ |
| No tag/release | `git for-each-ref refs/tags/v0.1.0` | still points at `99ad6a9` (original), HEAD unmoved ✅ |

The regression that blocked cycle 1 (0036 heading overwritten, its 6 bullets orphaned under
the 0049 CA heading) is fully repaired. Two unrelated entries are separate blocks again.

## Part 2 — release.yml CA fix: ACCEPTED (re-confirmed, incl. fresh independent pin check)
The fix was accepted verbatim in cycle 1 and is unchanged. I re-verified the security-critical
item MYSELF this cycle, not on faith:
- Fetched `DeveloperIDG2CA.cer` from `apple.com` now → `shasum -a 256` = pinned
  `f16cd3c5…df3a` **EXACT match**.
- `openssl x509`: `CN=Developer ID Certification Authority, OU=G2`, issuer `Apple Root CA`,
  valid Sep-2021 → Sep-2031 — correct modern G2 intermediate.
- Placement after `.p12` import, before the kept find-identity check; `shasum --check --status`
  fails CLOSED under `set -euo pipefail`; `security import` with no `-T`, no trust override;
  `.cer` rm'd in-step + in the `always()` cleanup. No residual key/secret beyond the CA.

## Part 3 — why `blocked`, not `done`: AC part 2 is a human-only gate
AC requires the v0.1.0 run green end-to-end (sign → notarize → staple → gh release create) on
gramdrive-mac, and the DoD requires `find-identity -v -p codesigning >= 1` on the runner. Both
can be proven ONLY by the live run — the sim's dummy self-signed identity is not issued by G2
so it structurally cannot assert leaf validity (developer + cycle-1 review both flagged this).

The live run cannot be executed autonomously:
- re-pointing the tag needs `git push` — forbidden by standing orders (no auto commit/stage/push);
- the job carries `environment: release` — owner approval gate;
- it is an outward-facing, credential-minting, hard-to-reverse public GitHub Release.

This is a genuine stop-the-line human-only approval/external-action boundary, not rework and
not a recoverable runtime failure — so the correct routing is `blocked`, not `done`/`to-dev`.

### Failed assumptions / attempts (why autonomy can't clear it)
- Sim CANNOT assert `find-identity -v >= 1` (structural limit — no G2-issued leaf in sim).
- Safe-failure caveat stands: fix assumes the Relux Works leaf chains through G2 (not legacy
  G1). Unverifiable from here (leaf is inside the `MACOS_CERT_P12` secret). Fail-closed: if it
  chains through G1, the kept find-identity check reports 0 and the release fails LOUDLY at
  that step before any signing — no broken artifact ships.

### Viable options + tradeoffs
1. **Human runs the release now (recommended).** Closes AC part 2 with real proof. Cost: one
   owner approval + one green watch. On red at find-identity → leaf chains through a different
   intermediate (e.g. G1); add that CA and re-run — fully recoverable, nothing shipped.
2. Leave the task open indefinitely. Rejected: code + docs are done and verified; parking it
   hides that the only gate is a human approval, not more engineering.

### Exact human input needed
1. Commit the fix on `main` (release.yml + corrected LOGBOOK.md).
2. `git tag -d v0.1.0 && git push origin :refs/tags/v0.1.0 && git tag v0.1.0 <fix-sha> && git push origin v0.1.0` (prior runs published no release — safe to re-point).
3. Approve the `release` environment when prompted (owner sign-off).
4. `gh run watch <run-id>` to green: sign → notarize → staple → `gh release create` with dmg asset.
5. On green: flip DoD item 2 → task `done`. On red at find-identity: add the actual leaf CA, re-run.

**Recommendation:** option 1 — the engineering is complete and independently verified; hand to
the owner for the gated live release.
