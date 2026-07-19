# TASK-260715-3bhbkv — Release signing & provenance: implementation results

**Status:** ready for review (board: `to-review`)
**Scope:** tag-triggered signed/notarized/attested macOS release + SBOM, changelog,
rollback metadata, version stamping, credential scrub. Mirrors the
relux-works/barycenter `release.yml` pattern.

## What was built

### 1. `.github/workflows/release.yml` (new)
Tag-triggered (`v*`), `macos-15`, `environment: release` (POL-8/DEC-020 human gate).
- Least privilege: top-level `contents: read`; the job elevates to
  `contents: write` + `id-token: write` + `attestations: write`.
- **Supply-chain gate first (POL-6):** `run_automated.py --suite supply-chain`
  fails *closed* before any credential is imported.
- **Throwaway keychain:** imports the Developer ID Application identity from
  `MACOS_CERT_P12`/`MACOS_CERT_PASSWORD` and stores the `gramdrive-notary` notary
  profile from `APPSTORE_KEY_ID`/`APPSTORE_ISSUER_ID`/`APPSTORE_PRIVATE_KEY` into a
  single `RUNNER_TEMP` keychain, deleted in an `always()` step. No secret echoed;
  nothing touches the login keychain or the workspace.
- Invokes `build_app_bundle.py --notarize --notary-keychain "$KEYCHAIN_PATH"`
  (signed, hardened runtime, timestamped, notarized+stapled) — never a copy of the
  codesign/notarytool commands.
- Invokes `build_release_provenance.py` for SBOM/changelog/rollback/manifest/scrub.
- `actions/attest-build-provenance` (pinned `e8998f9` = v2.4.0) attests the dmg;
  `gh release create` publishes dmg + checksums + SBOM + changelog + rollback +
  release manifest with the changelog as the release notes.
- All actions pinned by SHA, reusing ci.yml's vetted pins verbatim.

### 2. `.scripts/release/build_release_provenance.py` (new) + self-tests
Derived from the signed `.app` `manifest.json` + git history + `cargo metadata`:
- **`sbom.json`** — CycloneDX 1.5, one component per third-party crate with its
  SPDX license + purl (126 cargo, 0 swiftpm — the Apple package has no third-party
  SPM deps). POL-6 is *enforced* by `cargo deny` (core CI), **not** re-adjudicated
  here (no second, drifting policy engine); the SBOM records licenses and points at
  the gate. Deterministic serial (uuid5 of commit+tag).
- **`CHANGELOG.md`** — non-merge commits since the previous tag.
- **`rollback.json`** — this tag/commit/version + previous tag/commit + dmg sha256.
- **`release-manifest.json`** — ties tag/version/commit to every artifact by sha256;
  carries notarization + toolchain read from the app manifest.
- **`RELEASE-CHECKSUMS.sha256`** + a **credential scrub** that fails the release if
  any produced file carries secret-shaped content (high-confidence material over all
  files; leak-words over the structured JSON only, so a commit subject saying
  "password" in the changelog is not a false positive).

### 3. `.scripts/apple-app/build_app_bundle.py` (modified)
Closed the packaging-review 2115 finding flagged *for this task*:
- **Staples the `.app` AND the `.dmg`**, and staples the app *before* the dmg is
  built (app zipped → notarized → stapled → packed), so the app a user drags out of
  the dmg carries an offline ticket. Was: dmg-only (extracted app unverifiable
  offline).
- Added `--notary-keychain` so notarization can use a throwaway keychain (CI);
  defaults to the login keychain (local dev unchanged).

### 4. Version stamping (already automatic, confirmed)
`build_app_bundle.py` derives the marketing version from the tag's `git describe`
and `CFBundleVersion` from the git rev-count (Sparkle ordering). The workflow's
`fetch-depth: 0` gives it the tag + full history.

## Verification (this host)
- Script self-tests **204/204** (`unittest discover .scripts/tests`; +20: provenance
  suite +18, app-stapling +1, keychain +2, minus overlap).
- Repo gate `run_automated.py --suite repo` **2/2** (traceability + scripts).
- Supply-chain gate `run_automated.py --suite supply-chain` **1/1** (cargo-deny/POL-6
  green — all 126 deps permissive, matching the SBOM's 0 missing-license count).
- `actionlint -shellcheck` **exit 0** on all three workflows.
- **Real dry-run** of the non-signing half against this repo: 126 cargo + 0 swiftpm
  SBOM components, changelog of 62 commits, release manifest ties every artifact to a
  verified sha256, **scrub passed**.

## Board correction (coordinator-veto flagged)
Removed stale decomposition-era edge `152wjq (dependency-license-sbom) → 3bhbkv`
(reversed direction; POL-6 gate already in core-ci; SBOM is this task's own
deliverable). Precedent: 2130/2145 core-ci and 2151/2156 native-ci (both
review-upheld). Reverse: `link(TASK-260715-3bhbkv, blocked_by=TASK-260715-152wjq)`
(+ story edge). Details in LOGBOOK 2205 + board notes.

## What a real signed release still needs (documented, not a gap in this task)
The signing/notarization steps require Apple secrets + a macOS runner and cannot be
exercised in this session — they run on a real `v*` tag. The reusable script they
invoke is already proven end-to-end on this host by TASK-260715-1dk9ik (signed +
notarized + stapled, submission Accepted). Repo-admin follow-ups a workflow file
cannot self-grant: (1) set the `release` environment's required-reviewer rule (the
POL-8 owner gate); (2) add the five `MACOS_CERT_*` / `APPSTORE_*` secrets.
