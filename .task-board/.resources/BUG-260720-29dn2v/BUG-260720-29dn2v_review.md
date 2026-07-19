# BUG-260720-29dn2v — release-missing-devid-ca — REVIEWER VERDICT

**Verdict:** `to-dev` — the release.yml CA fix itself is reviewer-ACCEPTED and independently
verified (no change requested to the fix). One required rework blocks acceptance of the
submission as a whole: the developer's LOGBOOK.md edit **clobbered a prior, unrelated
entry**. Fix that, then the only remaining step is the human-only live release (documented).

---

## Part 1 — release.yml CA fix: ACCEPTED (independently re-verified, not on faith)
| Claim | How I checked | Result |
|---|---|---|
| Pinned sha256 matches Apple's real cert | Fetched `DeveloperIDG2CA.cer` from apple.com myself, `shasum -a 256` | **EXACT match** `f16cd3c5…df3a` |
| Right intermediate | `openssl x509` subject/issuer/dates | `CN=Developer ID Certification Authority, OU=G2` · issuer `Apple Root CA` · valid Sep-2021 → Sep-2031 ✅ |
| Placement | release.yml:163-191 | After `.p12` import, before the kept find-identity check ✅ |
| Fail-closed | `shasum --check --status` under `set -euo pipefail`, before `security import` | Mismatch aborts the step — swapped/MITM'd cert never reaches keychain ✅ |
| No new key/secret/trust | `security import` has no `-P`, no `-T`, no trust override | Public cert only; anchor stays OS-trusted Apple Root CA ✅ |
| No residue | `.cer` rm'd in-step + added to `always()` cleanup `rm -f` | Throwaway keychain deleted in cleanup ✅ |
| actionlint | Re-ran `actionlint 1.7.12` on release.yml | **exit 0, clean** ✅ |
| keychain-sim on relux | `BUG-260720-29dn2v_keychain-sim-relux.log` + sim script | SIM OK · G2 present · pin fails closed · keychain GONE · default+search list RESTORED ✅ |

**Safe-failure caveat (not a defect):** the fix assumes the Relux Works Developer ID leaf
chains through **G2** (not legacy G1). I cannot verify this — the leaf is inside the
`MACOS_CERT_P12` secret. But it is fail-closed: if the leaf actually chains through G1, the
kept `find-identity` check reports 0 and the release fails LOUDLY at that step, before any
signing — it never ships a broken artifact. Decisive proof (`find-identity -v >= 1` with the
real leaf) can ONLY come from the live run; the sim uses a dummy self-signed identity not
issued by G2, so it structurally cannot assert leaf validity. Developer flagged this limit.

## Part 2 — REQUIRED REWORK: LOGBOOK.md regression (to-dev)
The new logbook entry was added by **overwriting the heading of the previous entry** instead
of inserting a new block above it. Confirmed against `git show HEAD:LOGBOOK.md`:

- At HEAD: `### 0036 — REVIEW: self-hosted runner migration ACCEPTED (TASK-260719-1dwaj8 → done)`
- Working tree: that `### 0036` heading is **gone**, replaced by `### 0049 — release.yml: missing Developer ID G2 CA …`, and the 0036 entry's body bullets (`DECISION: accepted → done`, `GATE (reviewer re-verified): runner relux-gramdrive online…`, the arm64 FINDING, the no-residue FINDING, the architecture-fit FINDING, the rust-cache NOTE) are now **orphaned under the 0049 CA heading** — two unrelated entries merged, and the runner-migration review record lost as a discoverable heading.

**Fix:** restore the `### 0036 …` entry intact and add `### 0049 …` (the CA-bug entry: its 5
bullets ROOT CAUSE / FIX / DECISION pin-by-sha256 / GATE actionlint / LIMIT) as its **own
separate block above 0036**. Newest-first ordering, one block per entry — the file's convention.

## Part 3 — after rework: human-only live release (do NOT attempt autonomously)
AC part 2 (v0.1.0 run green end-to-end: sign → notarize → staple → gh release create on
gramdrive-mac) is a human-only gate — the developer must **not** run it:
- re-pointing the tag requires `git push` — forbidden by standing orders (no auto commit/stage/push);
- the job carries `environment: release` — owner approval gate;
- it is an outward-facing, credential-minting, hard-to-reverse public GitHub Release.

Runbook for the human once the code + logbook land on `main`:
1. Commit the fix (release.yml + corrected LOGBOOK.md) on `main`.
2. `git tag -d v0.1.0 && git push origin :refs/tags/v0.1.0 && git tag v0.1.0 <fix-sha> && git push origin v0.1.0` (prior runs published no release — safe to re-point).
3. Approve the `release` environment when prompted (owner sign-off).
4. `gh run watch <run-id>` to green: sign → notarize → staple → `gh release create` with dmg asset.
5. On green: flip checklist item 2 → task `done`. On red at find-identity: leaf chains through a different intermediate (e.g. G1) — add that CA, re-run.
