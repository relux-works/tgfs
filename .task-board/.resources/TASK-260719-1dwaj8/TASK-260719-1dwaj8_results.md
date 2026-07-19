# TASK-260719-1dwaj8 — Self-hosted runner migration: results

Provisioned a self-hosted GitHub Actions runner on the spare Intel Mac (`ssh relux`)
and migrated ci / native-ci / release workflows to it, with arm64 artifacts produced
by cross-compilation and arch-proven via lipo/file(1).

## Runner

| | |
|---|---|
| Name | `relux-gramdrive` |
| Repo | relux-works/tgfs (repo-level runner) |
| Labels | `self-hosted`, `macOS`, `X64`, `gramdrive-mac` |
| Version | actions-runner 2.335.1 (osx-x64); tarball sha256 `b2fe57b2ae5b0bc1605f9fc0723c07eedf06167321d3478ce0440f15e5b0a010` |
| Service | LaunchAgent `actions.runner.relux-works-tgfs.relux-gramdrive.plist` via `./svc.sh install` |
| Registration | `gh api -X POST repos/relux-works/tgfs/actions/runners/registration-token` (short-lived token, consumed at config time) |
| Restart proof | `./svc.sh stop && ./svc.sh start` → GitHub API reports `online` (AC) |
| Reboot note | LaunchAgent loads at login; relux runs other user LaunchAgents (auto-login session present). A physical reboot was NOT exercised: the box hosts unrelated live services (coolify, tundra-relay, market-impulse) and rebooting it is not this task's call. |

Machine config lives in the runner dir, not in workflows:

- `~/actions-runner/.env`: `DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer`,
  `CARGO_TARGET_DIR=/Users/administrator/gramdrive-ci/cargo-target/tgfs`
- `~/actions-runner/.path`: `~/gramdrive-ci/bin`, `~/.cargo/bin`, `~/.local/bin` + system dirs

## Toolchain (all no-sudo, pinned + checksum-verified; `~/gramdrive-ci/provision.sh`, log `logs/provision-02.log`)

| Tool | Version | Delivery |
|---|---|---|
| Xcode | 26.2 (17C52), universal x86_64+arm64, minOS 15.6 | **already on relux** at `/Applications/Xcode.app`; selected via `DEVELOPER_DIR` (no sudo `xcode-select`). `xcodebuild -showsdks` OK, `-checkFirstLaunchStatus` OK, SDK MacOSX26.2 |
| Command Line Tools | swift 6.2.3, notarytool 1.1.0, stapler present | pre-existing |
| Rust | rustup, toolchain 1.91.0 (rustfmt, clippy) + **aarch64-apple-darwin target** | rustup-init, `--default-toolchain none` then pinned install |
| Python | 3.12.13 | python-build-standalone 20260718 x86_64 tarball, sha256 `10b47148…d81f` → `~/gramdrive-ci/bin/python3` (CLT python is 3.9 — too old for the gate entrypoint: `datetime.UTC` needs 3.11+) |
| cmake | 4.3.3 (macos-universal) | Kitware release tarball, checksum from `cmake-4.3.3-SHA-256.txt` |
| gitleaks | 8.30.1 darwin_x64 | release tarball, sha256 `dfe101a4…0709` (pre-provisioned in `~/gramdrive-ci/bin`; the workflow also self-installs its own pinned copy per run — same version, same checksum discipline) |
| gperf | 3.0.3 | ships with macOS (`/usr/bin/gperf`) |
| cargo-deny | 0.20.2 | taiki-e/install-action in-workflow (persists in `~/.cargo/bin`) |

### Xcode fallback disposition (task: "verify Xcode_26_5.app launches on Intel — if incompatible, document")

`/Applications/Xcode_26_5.app` on the arm64 host is **arm64-only** (`file` on its main
binary and xcodebuild: Mach-O arm64, no x86_64 slice) with `LSMinimumSystemVersion 26.2`.
It cannot launch on macOS 15.7 Intel — the rsync fallback is dead on arrival.
No stop-the-line needed: relux already carries universal **Xcode 26.2**, which provides
`xcodebuild -create-xcframework` and everything the packaging pipeline needs.

## TDLib artifact: cache-seeded (task: "record which path was taken")

Cross-building the pinned **arm64** TDLib on the Intel host is blocked exactly as the
task predicted: the C++ build links OpenSSL and brew on Intel has only x86_64 OpenSSL.
Path taken: **seeded the runner-local cache from this arm64 host** —

```
rsync .temp/tdlib/out/  relux:~/gramdrive-ci/cache/tdlib-28775d200a3a0386/out/
```

Key = `tdlib-$(shasum -a 256 .scripts/tdlib/build_tdlib.py | cut -c1-16)` — same recipe-pinning
discipline the old actions/cache key used (actions/cache itself is behind the blocked billing).
native-ci's tdlib job restores it, proves `libtdjson.dylib` is arm64 via file(1), cross-links it
(`make tdlib-smoke-link`, `cargo build --target aarch64-apple-darwin`), and **fails actionably on a
cold miss** with the exact seeding instructions. The runtime probe (C JSON call, version read) ran
on the arm64 host that built the artifact; the artifact's `manifest.json` records that build.

## Workflow migration (commit d46b203 + keychain fix commit)

- **ci.yml** — `rust-core` and `secret-scan` on `[self-hosted, gramdrive-mac]`.
  - secret-scan DECISION: moved (not kept on ubuntu) because the billing block covers hosted
    Linux too — this exact job failed at start with the billing error (run 29700024728).
    gitleaks pin unchanged (8.30.1), darwin_x64 asset, `shasum -a 256 -c`.
  - Hosted cache steps (Swatinem/rust-cache) dropped: cache service behind the same billing
    block; persistent runner keeps incremental state in the machine-local `CARGO_TARGET_DIR`.
  - x86_64 deviation documented in the job comment (platform-neutral gates run natively;
    shipped arm64 proven in native-ci/release).
- **native-ci.yml** —
  - `tdlib`: runner-local seeded cache (above).
  - `apple-build-test`: stages the core with `make package-host-test` (new) — an x86_64 twin
    of the same source lipo'd into the staged slice so `swift build` + `swift test` **really
    execute** on the Intel host; staging manifest/README record it; never a release input.
  - `apple-package-unsigned`: stages the SHIPPED shape (`make package`, arm64-only) + assembles
    with arm64 cross-built executables; a dedicated step re-reads every staged Mach-O with
    file(1) and fails on any non-arm64 (AC evidence in the job log).
- **release.yml** — `[self-hosted, gramdrive-mac]`; temp-keychain isolation KEPT and extended
  for the persistent runner:
  - naming step records default keychain + full search list **verbatim** before any change;
  - always()-cleanup deletes the throwaway, restores both, wipes `.temp` after artifact upload.
  - Measured on relux (dummy-p12 lifecycle sim, `~/gramdrive-ci/keychain-sim.sh`):
    `security delete-keychain` leaves a **dangling search-list entry**, `list-keychains -s`
    with zero args is a silent no-op, and `default-keychain -s` on a never-explicitly-set list
    makes the list follow the default — so grep-filter cleanup can silently leave residue.
    The verbatim capture/restore closes all three; sim verdict: keychain GONE, default RESTORED,
    search list RESTORED, zero residue.

## Script changes (all with self-tests; suite `scripts` green)

- `.scripts/packaging/build_core_artifacts.py`:
  - verifier is arch-aware: on a host that cannot execute the shipped slice it **cross-links**
    the consumer (`swift build --arch arm64` — real resolve+link proof) instead of running it;
    `verify_mode` (`native-run` / `cross-link-only` / `skipped`) recorded in manifest + README;
    contract version honestly stays `unverified` in cross-link mode (the runtime probe for the
    same commit lives in native-ci's apple-build-test).
  - `--host-test-slice`: builds a host-arch twin from the same clean target dir, lipo's it into
    the staged slice; manifest gains a `host_test_slice` record and the README a loud
    "CI test staging — not the shipped shape" banner. `SLICES` (the shipped list) unchanged.
- `.scripts/apple-app/build_app_bundle.py`: every `swift build` carries `--arch arm64`
  (constant `BUILD_ARCH`), and every built product is arch-gated with `lipo -archs` —
  anything but exactly `arm64` fails the build; manifest records the enforcement.
- `Makefile`: `package-host-test`, `tdlib-smoke-link` targets.
- Tests: `.scripts/tests/test_build_core_artifacts.py` (+6: cross-link mode, host-test twin,
  noop-on-arm64, swift_arch, manifest modes; fixture now writes per-triple and pins
  `host_machine` so verdicts don't depend on which CI host runs the suite),
  `.scripts/tests/test_build_app_bundle.py` (+3: --arch on every build, wrong-arch fails,
  manifest records enforcement).

## Verification

- Local: `make check` → suite all 8/8 (fmt, clippy, test, architecture, supply-chain,
  traceability, scripts — 242 script tests); actionlint clean (with `.github/actionlint.yaml`
  declaring the runner label). Re-run at handoff: `actionlint` on ci/native-ci/release → exit 0.
- Keychain lifecycle sim on relux re-run at handoff → `SIM OK` (keychain GONE, default
  RESTORED, search list RESTORED, no `gramdrive-signing` residue).

### CI on the runner (green)

| Run | Workflow | Commit | Conclusion |
|---|---|---|---|
| 29702010556 | CI (secret-scan, rust-core) | d46b203 | **success** |
| 29702010606 | native-ci (tdlib, apple-build-test, apple-package-unsigned) | d46b203 | **success** |
| 29702440710 | CI (secret-scan, rust-core) | 99ad6a9 (keychain fix) | **success** |
| 29702440760 | native-ci | 99ad6a9 (keychain fix) | success (confirmed at handoff) |

**file(1) arm64 evidence** (native-ci run 29702010606 job logs, on the x86_64 runner):

```
# tdlib job — seeded arm64 TDLib artifact
.temp/tdlib/out/lib/libtdjson.dylib: Mach-O 64-bit dynamically linked shared library arm64

# apple-package-unsigned job — cross-built, assembled bundle
.temp/app-packaging/GramDrive.app/Contents/MacOS/gramdrive-agent: Mach-O 64-bit executable arm64
.temp/app-packaging/GramDrive.app/Contents/MacOS/GramDrive: Mach-O 64-bit executable arm64
.temp/app-packaging/GramDrive.app/Contents/PlugIns/GramDriveFileProvider.appex/Contents/MacOS/GramDriveFileProvider: Mach-O 64-bit executable arm64
```

The keychain-fix commit 99ad6a9 touches **only** release.yml; ci.yml and native-ci.yml
are byte-identical to the green d46b203 run, so the HEAD re-runs exercise the same code.

## Release: what an actual v0.1.0 run still needs (owner action, not a forced fit)

The failed release run 29699941040 was triggered by tag `v0.1.0`, which points at a
pre-migration commit — a tag-triggered workflow runs the workflow file AT the tag's commit,
so re-running it can never use the migrated release.yml. To release v0.1.0 through the
self-hosted runner the tag must be recreated on a post-migration commit (owner decision —
POL-8 makes public release the mandatory human gate anyway). Everything the run needs on
the runner is proven: toolchain, arm64 staging, keychain lifecycle, cleanup.
