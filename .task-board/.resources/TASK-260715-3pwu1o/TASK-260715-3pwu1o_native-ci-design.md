# TASK-260715-3pwu1o — native platform CI: design, validation, and dependency block

**Role:** developer (implementer). **Status handed to:** `blocked` (one orchestrator decision needed — see §7).
**Date:** 2026-07-19. **Host:** macOS arm64, Xcode 26.5 (17F42), Swift 6.3.2, Rust 1.91.0.

---

## 1. TL;DR

The v1 native-ci deliverable is **fully designed and proven feasible today** — the
macOS build+test leg is green on this host right now (`swift build` ✓, `swift test`
= **252 tests / 47 suites passed**), and every supporting script the workflow needs
already exists.

I did **not** write the workflow/product-code, because the board **mechanically blocks**
this task: `TASK-260715-3pwu1o` is `blockedBy` `TASK-260715-11qg88`
(*native-provider-harnesses*, status `backlog`). `task-board m 'set_status(...development)'`
is refused: *"blocked by unfinished dependencies."*

My analysis (evidence below) is that **this dependency edge is over-scoped for the v1
native-ci checklist**: native-ci v1 is a *compile / unit-test / unsigned-package* gate,
which does **not** require the provider **integration** harnesses that 11qg88 builds.
The two are related (native-ci will eventually *run* those harnesses as a native
acceptance suite), but the v1 legs specified in this task's own checklist stand on their own.

Re-scoping or removing a planning dependency edge is an **orchestrator ownership-boundary
decision**, not a developer one — so I stop here and ask for exactly that decision (§7),
with a turnkey design attached so clearing the edge is a minutes-long handoff.

---

## 2. The block

### Dependency chain (verified via `task-board q`)

| Task | Name | Status |
|------|------|--------|
| `TASK-260715-3e8q4m` | source-conformance-suite | **done** |
| `TASK-260715-11qg88` | native-provider-harnesses | **backlog** (ready — its only blocker is done) |
| `TASK-260715-3pwu1o` | **native-ci (this task)** | backlog, `isBlocked: true` (blocked by 11qg88) |

`11qg88` lives in `STORY-260715-2ufyq8` (cross-platform-test-infrastructure, backlog),
a *different story* from this task's `STORY-260715-3bpt2q` (ci-and-build-system).

### Why the edge is over-scoped for v1

`11qg88` scope: *"Automate File Provider, CfAPI, DocumentsProvider, and FUSE scenarios
using the common fixture tree,"* AC: *"Harnesses validate identity/enumeration/content/
cancel/restart/read-only behavior and produce CI-reviewable evidence."* — i.e. **cross-platform
provider integration harnesses** (a heavy, four-platform deliverable).

This task's **own checklist** scopes native-ci v1 to:
1. *"swift build+test of apple/ packages on macos-15 arm64, unsigned app-bundle assembly
   gate (packaging script without Developer ID), TDLib artifact build cached; provenance
   artifacts per job"*
2. *"Blind gates where runner lacks capability documented; Windows/Android/Linux legs
   explicitly deferred per POL-5 with a scope note, not silently missing"*
3. *"Workflow validated; local simulation of each leg entrypoint runs clean"*

None of these reference the provider integration harnesses. "swift build+test of apple/
packages" = the apple package's **own unit tests** (already present under
`apple/GramDriveSupport/Tests/`), not the integration scenarios 11qg88 automates.

**Conclusion:** native-ci v1 (this checklist) can and should land before the full
provider integration harnesses. The provider-integration **native acceptance suite** is a
follow-up that native-ci wires in once 11qg88 lands.

---

## 3. Validation evidence (this host, today)

| Leg | Command | Result |
|-----|---------|--------|
| apple build | `swift build --package-path apple/GramDriveSupport` | **exit 0** (`.temp/3pwu1o-swift-build.log`) |
| apple test | `swift test --package-path apple/GramDriveSupport` | **exit 0 — 252 tests / 47 suites passed** (`.temp/3pwu1o-swift-test.log`) |
| staged core | `.temp/packaging/GramDriveCore` | present, contract `0.5.0` (from `make package`) |
| staged TDLib | `.temp/tdlib/out` | present, `libtdjson.dylib` + headers + manifest |

The apple package resolves `GramDriveCore` by path from the staged artifact
(`Package.swift`: `../../.temp/packaging/GramDriveCore`), so the CI order is fixed:
**TDLib → `make package` (stage core) → swift build+test / unsigned assembly.**

Building blocks that already exist (nothing new needed except §4.2):
- `.scripts/acceptance/run_automated.py` — the one pinned gate entrypoint (suites: core, repo, security, all).
- `.scripts/tdlib/build_tdlib.py` (`make tdlib`) — reproducible pinned TDLib artifact, own manifest provenance.
- `.scripts/packaging/build_core_artifacts.py` (`make package`) — stages `GramDriveCore` XCFramework + bindings.
- `.scripts/apple-app/build_app_bundle.py` (`make package-app`) — assembles `GramDrive.app` (**currently signing-only**).
- `.github/workflows/ci.yml` — the core-ci this extends (rust-core on macos-15, secret-scan on ubuntu-24.04).

---

## 4. Turnkey v1 design (barycenter-faithful)

Design rule honored throughout: **CI invents no step of its own** — it calls the pinned
entrypoint (`run_automated.py --suite <x>`). The two sanctioned exceptions the repo
already documents (packaging/tdlib run their scripts directly because each writes a
*stronger, purpose-built* `manifest.json` — routing them through the gate would add a
second, weaker provenance record) are reused as-is.

### 4.1 New `apple` gate suite in `run_automated.py`

Add two steps and one suite. Fast (debug) build+unit-test, matching the `core` suite's
`cargo test` posture. Requires the staged core to be present (workflow stages it first).

```python
# in build_steps(), add:
Step(
    name="swift-build",
    argv=("swift", "build", "--package-path", "apple/GramDriveSupport"),
    purpose="apple/GramDriveSupport compiles against the staged GramDriveCore",
),
Step(
    name="swift-test",
    argv=("swift", "test", "--package-path", "apple/GramDriveSupport"),
    purpose="apple/GramDriveSupport unit tests (File Provider, agent, companion, shared state)",
),

# in SUITES, add:
"apple": ("swift-build", "swift-test"),
```

Notes:
- **Not** folded into `all` — `all` runs on any host without Xcode or the staged core;
  `apple` needs both. It is its own CI job, per one-job-per-component.
- Prerequisite (staged core) mirrors the smokes' "stage `make package` if none present."
  Optionally add an `apple-prereq` step asserting `.temp/packaging/GramDriveCore/Package.swift`
  exists, for a cleaner failure than a SwiftPM resolution error. (Recommended but optional.)
- `--require-clean` stays compatible: `make package`/`make tdlib` write only to `.temp/` (gitignored).

Tests to add in `.scripts/tests/test_run_automated.py` (fake-runner style already used there):
- `apple` suite resolves to `[swift-build, swift-test]` in order;
- `apple` excluded from `all`;
- existing invariants (`test_every_suite_references_only_real_steps`,
  `test_every_step_is_reachable_from_a_suite`) keep passing with the new step/suite.

### 4.2 New `--unsigned` assembly mode in `build_app_bundle.py` (the ONE real code gap)

The packaging gate must run on a runner **without a Developer ID identity**. Today
`build_app_bundle.py` always signs. Add an assembly-only path that stops before codesign.

The pipeline is `build_products → assemble_bundle → write_entitlement_files → sign →
verify → assess → build_dmg → notarize`. The first three steps are **signing-independent**
(they stage the three executables into `GramDrive.app`, generate Info.plists / entitlement
plists, the launchd plist). Unsigned mode runs those, then records provenance:

```python
# AppPackager: add
def record_unsigned(self) -> list[SignedBinary]:
    """The bundle's binaries recorded without signing — the assembly-gate provenance.
    No codesign runs, so cdhash is None and the identity is 'unsigned'."""
    return [SignedBinary(key=s.key, bundle_id=s.bundle_id, entitlements=ENTITLEMENTS[s.key]())
            for s in BINARIES]

# package(): thread an `unsigned: bool = False` param; branch after write_entitlement_files:
if unsigned:
    signed = packager.record_unsigned()
    spctl_verdict, dmg, notarization = "not-assessed", None, {"submitted": False}
else:
    signed = packager.sign(app, entitlement_files, timestamp=timestamp)
    packager.verify(app, signed)
    spctl_verdict = packager.assess(app)
    dmg = packager.build_dmg(app, short_version, timestamp=timestamp)
    notarization = packager.notarize(dmg, notary_profile) if notarize else {"submitted": False}
```

- `build_manifest`: when unsigned, `identity="unsigned (assembly gate)"`, `cdhash=None`
  per binary; add `"signed": False` and a note. Skip the dmg checksum/size when `dmg is None`;
  still checksum the assembled `.app` tree (real provenance of the assembly).
- `main()`: add `--unsigned` (mutually exclusive with `--notarize`); when set,
  `identity="unsigned"`, skip `resolve_identity`.
- What the gate proves: the three SwiftPM executables assemble into one `GramDrive.app`
  with the correct layout (`Contents/MacOS`, nested `PlugIns/*.appex`, `Library/LaunchAgents`),
  Info.plists (app `LSUIElement`, appex `NSExtension` file-provider point + principal class),
  and entitlement plists — **the assembly contract**, minus the signature. That is exactly
  "unsigned app-bundle assembly gate (packaging script without Developer ID)."

Tests to add in `.scripts/tests/test_build_app_bundle.py` (41 faked-subprocess tests exist):
- unsigned `package(...)` runs `swift build` + assembly, runs **no** `codesign`/`spctl`/
  `hdiutil`/`notarytool`, and writes a manifest with `signed: False` and no cdhashes;
- the assembled tree contains the appex at `Contents/PlugIns/GramDriveFileProvider.appex`
  with an `NSExtension` Info.plist;
- `--unsigned` + `--notarize` is rejected.

### 4.3 `.github/workflows/native-ci.yml` (complete, copy-paste ready)

Scheduling reflects the task scope (*"Signing separated from ordinary PR CI; native
harness scheduling and result retention"*) and AC (*"release branches require native
acceptance evidence"*): native-ci is **not** the fast per-feature-PR gate — it runs on a
schedule, on-demand, on `main` merges, and on release-bound PRs. Signing/attestation stay
in the separate tag-triggered release workflow (`TASK-260715-3bhbkv`).

> Action SHAs below are reused verbatim from the vetted `ci.yml`. `actions/cache` is the
> one action not yet used in this repo — pin it to a **verified** SHA before committing
> (marked `<PIN>`); do not ship the placeholder.

```yaml
# Native platform CI — extends the barycenter core-ci (ci.yml) to the macOS
# native drive (POL-5 / DEC-017 reference target). Each job runs the ONE pinned
# acceptance entrypoint or a purpose-built packaging script (never an ad-hoc
# swift command list), and uploads its run's provenance.
#
# Not the fast per-PR gate: the native chain builds TDLib from source, stages
# the core XCFramework, and builds+tests the Swift package, so it runs on a
# schedule, on demand, on main, and on release-bound PRs — satisfying "release
# branches require native acceptance evidence" without taxing every feature PR.
#
# Blind gates / deferred targets (POL-5), explicitly NOT silently missing:
#   - Windows  (Cloud Files API, Rust)      — deferred: EPIC-260715-1mlv5j backlog
#   - Linux    (FUSE, Rust)                 — deferred: EPIC-260715-1hnglv backlog
#   - Android  (DocumentsProvider, Kotlin)  — deferred: EPIC-260715-y0fshx backlog
#   - iOS      (File Provider, Swift)        — deferred: EPIC-260715-3uynbw backlog
# These enter this workflow as their own macOS/Windows/Linux legs when the
# platform EPIC starts. The shared Rust core's portability is already gated by
# ci.yml's rust-core suite; a blind cross-compile leg for Windows/Linux core
# consumers is a follow-up (needs the C/TDLib link story per platform).
name: native-ci

on:
  workflow_dispatch:
  schedule:
    - cron: "0 6 * * *"          # nightly native acceptance evidence
  push:
    branches: [main, "release/**"]
  pull_request:
    branches: ["release/**"]      # release-bound PRs pay the native cost; feature PRs do not

permissions:
  contents: read

concurrency:
  group: native-ci-${{ github.workflow }}-${{ github.ref }}
  cancel-in-progress: ${{ github.event_name == 'pull_request' }}

jobs:
  apple-build-test:
    name: apple-build-test
    runs-on: macos-15                 # Apple Silicon, POL-5 reference host
    steps:
      - name: Checkout
        uses: actions/checkout@11bd71901bbe5b1630ceea73d27597364c9af683 # v4.2.2
        with:
          fetch-depth: 0              # git describe / rev-count for version stamping
      - name: Install the pinned Rust toolchain
        run: rustup show && rustc --version && cargo --version
      - name: Cache cargo registry and build
        uses: Swatinem/rust-cache@98c8021b550208e191a6a3145459bfc9fb29c4c0 # v2.8.0
      - name: Cache the pinned TDLib artifact
        id: tdlib-cache
        uses: actions/cache@<PIN>     # v4.x — pin to a verified SHA before commit
        with:
          path: .temp/tdlib/out
          key: tdlib-${{ runner.os }}-${{ hashFiles('.scripts/tdlib/build_tdlib.py') }}
      - name: Build TDLib (only on cache miss)
        if: steps.tdlib-cache.outputs.cache-hit != 'true'
        run: make tdlib
      - name: Stage the GramDriveCore artifact
        run: make package
      - name: Run acceptance suite (apple)
        run: python3 .scripts/acceptance/run_automated.py --suite apple --require-clean --run-id ci-apple
      - name: Upload acceptance provenance
        if: always()
        uses: actions/upload-artifact@ea165f8d65b6e75b540449e92b4886f43607fa02 # v4.6.2
        with:
          name: acceptance-ci-apple
          path: .temp/acceptance/ci-apple
          if-no-files-found: error
          retention-days: 14

  apple-package-unsigned:
    name: apple-package-unsigned
    runs-on: macos-15
    steps:
      - name: Checkout
        uses: actions/checkout@11bd71901bbe5b1630ceea73d27597364c9af683 # v4.2.2
        with:
          fetch-depth: 0
      - name: Install the pinned Rust toolchain
        run: rustup show
      - name: Cache cargo registry and build
        uses: Swatinem/rust-cache@98c8021b550208e191a6a3145459bfc9fb29c4c0 # v2.8.0
      - name: Cache the pinned TDLib artifact
        id: tdlib-cache
        uses: actions/cache@<PIN>     # same key as apple-build-test → shared hit, no rebuild
        with:
          path: .temp/tdlib/out
          key: tdlib-${{ runner.os }}-${{ hashFiles('.scripts/tdlib/build_tdlib.py') }}
      - name: Build TDLib (only on cache miss)
        if: steps.tdlib-cache.outputs.cache-hit != 'true'
        run: make tdlib
      - name: Stage the GramDriveCore artifact
        run: make package
      - name: Assemble the unsigned app bundle (no Developer ID)
        run: python3 .scripts/apple-app/build_app_bundle.py --unsigned --out-dir .temp/app-packaging
      - name: Upload unsigned-package provenance
        if: always()
        uses: actions/upload-artifact@ea165f8d65b6e75b540449e92b4886f43607fa02 # v4.6.2
        with:
          name: native-app-package-unsigned
          path: .temp/app-packaging
          if-no-files-found: error
          retention-days: 14
```

Design notes carried from ci.yml: least privilege (`contents: read`); pinned toolchain +
lockfile + action SHAs so cache cannot change a verdict; provenance uploaded `if: always()`
with `if-no-files-found: error` (a silently-skipped gate becomes a failure, not a green
empty artifact). Each job builds from a **clean checkout** (AC).

### 4.4 README "Continuous integration" additions

Extend the CI job table:

| Job | Runner | Suite / script | Provenance artifact |
|-----|--------|----------------|---------------------|
| `apple-build-test` | `macos-15` (arm64) | `apple` — swift build + swift test of `apple/GramDriveSupport` over the staged core | `acceptance-ci-apple` |
| `apple-package-unsigned` | `macos-15` | `build_app_bundle.py --unsigned` — assembly contract, no Developer ID | `native-app-package-unsigned` |

Add a **support matrix / deferred targets** note (POL-5): macOS is the v1 native leg;
Windows/Linux/Android/iOS legs are **deferred with their backlog EPIC ids** and enter
native-ci when the platform EPIC starts — documented, not silently missing.

---

## 5. Blind gates & deferrals (checklist item #2) — documented

- **macOS** — implemented (the two jobs above). Reference host runs the real build/test/package.
- **Windows** (Cloud Files API, Rust) — **deferred**, `EPIC-260715-1mlv5j` (backlog).
- **Linux** (FUSE, Rust) — **deferred**, `EPIC-260715-1hnglv` (backlog).
- **Android** (DocumentsProvider/SAF, Kotlin) — **deferred**, `EPIC-260715-y0fshx` (backlog).
- **iOS** (File Provider, Swift) — **deferred**, `EPIC-260715-3uynbw` (backlog).

Each is named in the workflow header comment and the README matrix, so a reader sees a
deliberate deferral with a traceable owner, not an omission. The shared Rust core's
platform-neutrality is already gated by `ci.yml` rust-core (`architecture` step). A blind
cross-compile leg (Windows/Linux core consumers) is a reasonable follow-up but was **not**
forced in here: it needs a per-platform C/TDLib link story, and stubbing one now would be a
build path nothing runs (the repo's stated anti-pattern).

---

## 6. What I did NOT do, and why

- **No product-code edits** to `build_app_bundle.py`, `run_automated.py`, or `ci.yml`, and
  **no new `native-ci.yml`** committed — the task is blocked on an ownership-boundary
  decision (§7); the stop-the-line rule is to stop product-code changes and ask for that
  decision, not to implement around the block.
- **Did not delete/alter the dependency edge** — the board dependency graph is the
  orchestrator's to change, not the developer's.
- Working tree is clean of product changes (only this task's `.temp/` artifact + board files).

---

## 7. The one decision needed from the orchestrator

**Should native-ci v1 (this checklist: apple build+test, unsigned packaging, cached TDLib,
provenance, documented deferrals) land now, decoupled from `TASK-260715-11qg88`
(provider integration harnesses)?**

### Option A — re-scope the edge (recommended)

Drop or soften `11qg88 → 3pwu1o` so native-ci v1 can proceed; track the
**provider-integration native acceptance suite** as a follow-up dependency (native-ci
*consumes* 11qg88's harnesses when they exist). Rationale: v1 legs are proven green today,
all building blocks exist, and the checklist never asked for the integration harnesses.
On approval I implement §4 (est. small: one suite + two steps + tests; one `--unsigned`
mode + tests; one workflow file; README) and hand off to review.

### Option B — keep native-ci waiting for 11qg88

If the intent is that native-ci must ship *with* harness-backed native acceptance from day
one, this stays blocked until `11qg88` (and its story `2ufyq8`) are done, then native-ci
adds an `apple-acceptance` suite that runs the harnesses. The §4 build/test/package design
is still ~90% of the work and drops in unchanged.

**Recommendation: Option A.** It ships real native gating (compile + 252-test unit suite +
unsigned packaging contract + provenance) on the reference platform now, and layers the
heavier integration acceptance on top when 11qg88 lands — instead of blocking all native CI
evidence on a four-platform harness deliverable that hasn't started.

---

## Appendix — commands run for validation

```
swift build --package-path apple/GramDriveSupport            # exit 0
swift test  --package-path apple/GramDriveSupport            # 252 tests / 47 suites, exit 0
task-board q 'get(TASK-260715-11qg88){id name status blockedBy}'  # backlog, blockedBy 3e8q4m(done)
```
Logs: `.temp/3pwu1o-swift-build.log`, `.temp/3pwu1o-swift-test.log`.
