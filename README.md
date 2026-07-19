# GramDrive

Concept: expose a user's Telegram account as a file tree — **folder per chat** (named after the chat/user, in Telegram dialog order), containing all downloaded media plus the chat history exported as text. The primary product is a native drive resembling Dropbox/Google Drive:

- desktop and mobile apps that integrate with Finder, Explorer, iOS Files, Android's system picker, and Linux mounts;
- the Telegram engine embedded per device for desktop/Android v1, with an interchangeable remote source retained for iOS cold hydration, self-hosting, or a later hosted tier;
- no web application or rich Telegram-like UI requirement for the initial product.

## Status

**Early implementation.** The technology survey, specification baseline, and complete service decomposition are committed; architecture decisions marked provisional still require their explicit decision tasks. Product code so far: the shared Rust core workspace skeleton (`crates/`, see below) — crate boundaries, dependency-direction rules, and quality gates; domain logic is still to come.

Start with the [specification index](.spec/README.md) and the [generated project plan](.planning/260715_045337_project.md). Product implementation is intentionally deferred until the plan is reviewed and approved.

Naming (DEC-019, POL-7): the public product name is **GramDrive**, and every shipped identifier is derived from the `com.reluxworks.gramdrive` namespace. `tgfs` remains the internal repository/codename only — it collides with [TheodoreKrypton/tgfs](https://github.com/TheodoreKrypton/tgfs) and must not appear in user-visible strings, marketing, or store listings. The repository is deliberately not renamed. Trademark/handle check happens before public release.

## Current direction (from research, 2026-07-15)

- **Shared client core:** Rust — virtual tree, local SQLite/cache, change cursors, hydration, range downloads, retry/recovery, offline state, naming and generated files. Swift/Kotlin bindings through UniFFI; direct use on Windows/Linux. Verified precedents: Element X (matrix-rust-sdk), Dropbox Nucleus, Firefox (`.research/260715-shared-core-feasibility.md`).
- **Telegram source — behind one provider-neutral Rust trait, two interchangeable implementations:**
  - *Local-first:* [TDLib](https://github.com/tdlib/td) (BSL-1.0) embedded per device via tdjson FFI — zero infrastructure, Dropbox UX on desktop/Android; on iOS TDLib fits only in the main app (FP extension ~20 MB cap), and TDLib has no takeout API (normal-API backfill with flood-wait pacing).
  - *Remote:* Go + [gotd/td](https://github.com/gotd/td) (MIT) — optional for takeout backfill, one canonical archive, and iOS cold hydration; requires an always-on instance (self-hosted or SaaS with auth-key custody). Patterns from [iyear/tdl](https://github.com/iyear/tdl) (study only — AGPL); Telethon (Codeberg, MIT) is the takeout-worker alternative.
- **OS integration:** thin native adapters: File Provider replicated extensions (iOS/macOS, Swift), Cloud Files API (Windows, Rust), DocumentsProvider/SAF (Android, Kotlin), FUSE (Linux, Rust). Read-only first.
- **UI:** native and minimal; web and rich chat UI deferred.
- **Storage:** SQLite/PostgreSQL canonical metadata + content-addressed blob store; text export as NDJSON (lossless) + Markdown (human-readable).
- Treat official Telegram clients as **reference implementations**. A commercial GPL fork is legally possible, but a proprietary/closed-source fork is generally incompatible with their copyleft obligations and needs a deliberate licensing review.

## Research artifacts

| Artifact | Contents |
|---|---|
| `.spec/architecture.md` | Native-drive architecture with the shared Rust core and interchangeable local/remote sources |
| `.spec/policies.md` | Accepted product policies POL-1…POL-8 (ordering, media/Archive Mode, retention, restricted content, support matrix, licensing, naming, approval gates) |
| `.research/260715-telegram-filesystem-landscape.md` | Synthesized library, API, platform, architecture, and prior-art report |
| `.research/260715-core-libraries.md` | MTProto/TDLib library landscape |
| `.research/260715-oss-clients.md` | Official/OSS Telegram clients, licenses, and API terms |
| `.research/260715-filesystem-integration.md` | File Provider / Cloud Files / SAF / FUSE survey |
| `.research/260715-prior-art.md` | Exporter, archive, Telegram-FUSE, and WebDAV prior art |
| `.research/260715-shared-core-feasibility.md` | Rust/UniFFI precedents, TDLib extension constraints, gomobile and grammers analysis |
| `docs/OPEN_QUESTIONS.md` | Open product and architecture decisions |
| `docs/TELEGRAM_API_COMPLIANCE.md` | Telegram API terms → verifiable controls, rule-to-task mapping (TGC-nn) |
| `docs/TRACEABILITY.md` | Requirement coverage matrix: every PRD/DOM/SYNC/PLAT/SEC/NFR/DEC/POL ID mapped to board elements |

## Delivery decomposition

The canonical local board is stored in `.task-board/` and must be changed through the `task-board` CLI. The current baseline contains 11 epics, 53 stories, and 142 atomic tasks; all remain unstarted.

Human-only work is isolated in the `manual-actions` epic (EPIC-260716-3vc5ay): product decisions and ADR ratification, external credentials (Telegram api_id/api_hash, Apple signing assets, Windows signing identity, test devices), and manual on-device release validation. Every other epic is designed to run autonomously in an agent loop once its `manual-actions` dependencies are done.

The project-level dependency plan has four phases:

1. manual decisions/credentials plus autonomous product-foundation analysis;
2. shared Rust core;
3. local TDLib source and the optional remote tier;
4. native drive integrations plus cross-platform quality, security, and release work.

Detailed generated plans for every epic are in [`.planning/`](.planning/). The remote tier is decomposed to preserve interface and sizing clarity, but remains optional and does not authorize hosted-service implementation.

## Tools

Conventions:

- `.spec/` — product and architecture source of truth.
- `crates/` — the shared Rust core workspace (`crates/README.md` documents layers, dependency direction, and feature policy).
- `apple/` — Apple-native packages; currently `apple/GramDriveSupport`, the provider-support Swift package every GramDrive process links (App Group container resolution, shared-state access per process role, the cross-process change doorbell), which also ships the `gramdrive-agent` companion agent and the `gramdrive-companion` menu-bar shell (authorization, status, cache/Archive settings, diagnostics, repair, removal — `apple/GramDriveSupport/README.md`).
- `.research/` — permanent research archive.
- `.task-board/` and `.planning/` — project decomposition and generated execution plans.
- `.scripts/` — reusable repo utilities.
- `.temp/` — ignored local agent/runtime artifacts only.
- Research documents cite primary-source URLs inline; verify against them before relying on a claim.

### Running the checks

Every automated gate runs through **one entrypoint**, and CI invokes the same
script with the same suite names — a gate that exists only inside a CI config
cannot be run before pushing, and drifts the first time either side is edited.

```sh
make check          # pre-push gate: the core suite plus the repo suite
make check-core     # Rust core only: toolchain, format, lint, test, architecture, supply chain
make check-security # gitleaks secret scan of committed history (needs gitleaks)
make gates          # print every suite and the exact command behind each step
make fmt            # apply rustfmt (the gate only checks formatting)
```

`make check` is shorthand for
`python3 .scripts/acceptance/run_automated.py --suite all --run-id local-all`.
Every step runs even after one fails, because the useful output of a gate run is
the full list of what is broken. Each run writes provenance to
`.temp/acceptance/<run-id>/` — `summary.json` (commit, worktree state, tool
versions, per-step exit codes and durations) plus one log per step. CI uploads
that directory as an artifact and passes `--require-clean`, which refuses to run
against a dirty worktree so the recorded commit describes what was actually
tested (NFR-052).

Prerequisites: `rustup` (it reads `rust-toolchain.toml` and installs the pinned
toolchain automatically), `cargo-deny` (`brew install cargo-deny`), and Python
3.11+. The `toolchain` step fails with an explicit message if any of them is
missing or is the wrong version, rather than letting an unpinned compiler
quietly produce a different verdict. The `security` suite additionally needs
`gitleaks` (`brew install gitleaks`); it is deliberately kept out of `make check`
so the everyday pre-push gate does not require it.

### Continuous integration

`.github/workflows/ci.yml` runs the same acceptance entrypoint on every pull
request (and on `main` after merge). It invents no step of its own — each job is
`run_automated.py --suite <x> --require-clean`, so a check that fails in CI fails
the same way locally:

| Job | Runner | Suite | Provenance artifact |
|-----|--------|-------|---------------------|
| `rust-core` | `macos-15` (arm64, POL-5 reference host) | `all` — toolchain, format, lint, test, architecture, cargo-deny (POL-6), traceability, script self-tests | `acceptance-ci-all` |
| `secret-scan` | `ubuntu-24.04` | `security` — gitleaks over committed history | `acceptance-ci-security` |

Design notes:

- **Least privilege.** The workflow grants `contents: read` and nothing else; no
  job writes to the repo or mints a token. Release signing/attestation is a
  separate tag-triggered workflow.
- **Pinned, so cache cannot change a verdict.** The Rust toolchain is pinned by
  `rust-toolchain.toml`, dependencies by `Cargo.lock`, `cargo-deny` and
  `gitleaks` by exact version (gitleaks additionally by sha256), and every action
  by commit SHA. The cargo cache is keyed on the toolchain and lockfile, so a hit
  can only ever hold artifacts built from identical inputs — it speeds up a run,
  it cannot alter its result.
- **No secrets in logs.** Neither job needs a repository secret, and the secret
  scan runs gitleaks with `--redact` so a matched value never reaches the
  uploaded log. Verified false positives are pinned by fingerprint in
  `.gitleaksignore`; `.gitleaks.toml` is the committed, shared rule config.
- **Provenance.** Each job uploads `.temp/acceptance/<run-id>/` (`if: always()`,
  `if-no-files-found: error`, 14-day retention) so a result — green or red — is
  attributable to a commit (NFR-052).

**Required checks (branch protection, one-time repo-admin setup).** A workflow
file cannot make itself blocking. Mark `rust-core` and `secret-scan` as required
status checks on `main` so a pull request cannot merge while either fails.

#### Native platform CI

`.github/workflows/native-ci.yml` extends the same pattern to the macOS native
drive (POL-5 / DEC-017 reference target). It is **not** the fast per-PR gate: its
jobs build TDLib from source and stage the core XCFramework, so it runs on a
schedule (nightly), on demand (`workflow_dispatch`), on `main`, and on
release-bound PRs (`release/**`) — which is what *"release branches require native
acceptance evidence"* needs without taxing every feature PR. Each job builds from
a clean checkout and, like core-ci, runs the pinned entrypoint (or a packaging
script that writes its own manifest), never an ad-hoc swift command list.

| Job | Runner | Suite / script | Provenance artifact |
|-----|--------|----------------|---------------------|
| `tdlib` | `macos-15` (arm64) | pinned TDLib built from source (cache-first, keyed on the pin) + Rust link smoke | `native-tdlib` |
| `apple-build-test` | `macos-15` (arm64) | `apple` — `swift build` + `swift test` of `apple/GramDriveSupport` over the staged core (`make check-apple`) | `acceptance-ci-apple` |
| `apple-package-unsigned` | `macos-15` (arm64) | `build_app_bundle.py --unsigned` — assembles `GramDrive.app` (nested appex, Info.plists, entitlement plists) with **no Developer ID** (`make package-app-unsigned`) | `native-app-package-unsigned` |

Design notes:

- **No secrets, ever.** Native-ci only assembles an **unsigned** bundle. Signing and
  notarization (the Developer ID identity in a keychain) live in the separate
  tag-triggered release workflow (`TASK-260715-3bhbkv`); this workflow keeps
  `permissions: contents: read` and consumes no repository secret.
- **TDLib cached on the pin.** The `tdlib` job keys its cache on `build_tdlib.py`
  (which holds the pinned commit), so a warm run restores the artifact and re-runs
  only the fast link smoke; the from-source C++ build happens on a cold cache.
- **Support matrix (POL-5), documented not silently missing.** macOS is the v1
  native leg. iOS (`EPIC-260715-3uynbw`), Windows (`EPIC-260715-1mlv5j`), Linux
  (`EPIC-260715-1hnglv`) and Android (`EPIC-260715-y0fshx`) legs are **deferred**
  with their backlog EPIC ids in the workflow header, and enter native-ci when the
  platform EPIC starts — not stubbed, because a build path nothing runs rots.

The bindings smoke (`make smoke-bindings`, not part of `make check`)
additionally needs `swiftc` (Xcode command line tools), `kotlinc`
(`brew install kotlin`), and Java 17+ (`brew install openjdk`); it downloads
its two JVM runtime jars (JNA, kotlinx-coroutines) from Maven Central once,
pinned by version and sha256 in the runner script.

The shared-state smoke (`make smoke-shared-state`, also not part of
`make check`) needs Xcode: it runs a Rust coordinator process, two
concurrent Swift provider processes, and a change-watcher process over one
substitute App Group container, through the packaged artifact (staging
`make package` first when none is present).

The agent-lifecycle smoke (`make smoke-agent-lifecycle`, also not part of
`make check`) needs Xcode: it runs the `gramdrive-agent` companion binary
as real processes over a substitute container and proves the lifecycle
contract — health over the bounded IPC channel, single-instance refusal of
a second agent, SIGTERM drain of a hosted transfer, and instant successor
startup after SIGKILL.

### Packaging the core for native consumers

```sh
make package               # build + verify the artifacts native hosts consume
make package-reproducible  # build the shipped library at two paths, compare bytes
```

`make package` produces a self-contained SwiftPM package in `.temp/packaging/`:
`GramDriveCore.xcframework` (macOS 14+ arm64 static library plus headers), the
generated Swift bindings, a manifest, and checksums — then proves it by
resolving and running a real minimal Swift package against the result. Needs
Xcode (`xcodebuild`, `swift`); like the smoke it is not part of `make check`,
because it needs a release build and produces artifacts rather than a verdict on
the source.

Per platform: **macOS** consumes the XCFramework; **Windows and Linux** consume
the `gramdrive-ffi` crate directly as a Rust dependency and need no artifact at
all (which is why the crate keeps its rlib crate-type); **Android** (`.so` +
Kotlin) and **iOS** slices are deferred until those platforms enter scope
(POL-5/DEC-017), not stubbed — a build path nothing runs is a build path that
rots. Full rationale, the measured reproducibility and size numbers, and the
crate-type and debug-info decisions: [`.scripts/packaging/README.md`](.scripts/packaging/README.md).

Available utilities:

| Tool | Purpose | Run | Output |
|---|---|---|---|
| `.scripts/acceptance/run_automated.py` | The single gate entrypoint: runs a named suite, records provenance. Used identically by `make` and by CI | `python3 .scripts/acceptance/run_automated.py --suite core --run-id local-core`; `--list` prints suites and steps | Exit 0 pass / 1 failed step / 2 could not start; `.temp/acceptance/<run-id>/` with `summary.json` + per-step logs |
| `make` | Shorthand for the entrypoint plus the non-gate inner loop (`fmt`, `build`, `test`) | `make check`, `make check-core`, `make check-repo`, `make gates` | Delegates to the entrypoint; never re-defines a gate command |
| `cargo` (pinned to Rust 1.91.0 by `rust-toolchain.toml`, edition 2024) | Build and test the shared core workspace | `cargo build --workspace` / `cargo test --workspace` (repo root); per-crate commands in each `crates/*/README.md` | Binaries/test results under `target/` (gitignored) |
| `rustfmt` + `clippy` (pinned components) | Formatting and lints. Config: `rustfmt.toml`, `clippy.toml`, and `[workspace.lints]` in `Cargo.toml` — levels live in the manifest so editors agree with the gate | `cargo fmt --all` to fix; the `format` and `lint` gate steps to check | Exit non-zero on a formatting diff or any warning (`-D warnings`) |
| `.scripts/check_toolchain.py` | Asserts the pinned toolchain is actually in effect — `rust-toolchain.toml` only binds when rustup drives cargo — and that `cargo-deny` meets the minimum version | `python3 .scripts/check_toolchain.py` (repo root; stdlib only) | Exit 0 + summary line, or exit 1 with itemized errors (CI-suitable) |
| `.scripts/check_crate_architecture.py` | Enforces `crates/README.md`: dependency direction, no cycles, no platform leakage in core crates, testkit dev-only, per-crate README sections, shared lint-set opt-in | `python3 .scripts/check_crate_architecture.py` (repo root; stdlib only, needs `cargo` on PATH) | Exit 0 + summary line, or exit 1 with itemized errors (CI-suitable) |
| `cargo-deny` (installed via `brew install cargo-deny`) | Supply-chain gate, config in `deny.toml`: POL-6 licenses (permissive-only), RustSec advisories, bans, and sources (crates.io only) | `cargo deny check` (repo root), or one check: `cargo deny check licenses` | `advisories ok, bans ok, licenses ok, sources ok`, or non-zero exit with the offending dependency tree |
| `.scripts/validate_traceability.py` | Validates `docs/TRACEABILITY.md` against `.spec/` and `.task-board/`: every requirement mapped exactly once, no orphan board elements, no stale requirement references on the board | `python3 .scripts/validate_traceability.py` (repo root; stdlib only) | Exit 0 + summary line, or exit 1 with itemized errors (CI-suitable) |
| `.scripts/tests/` | Self-tests for the gate scripts themselves — an untested runner is a gate with no gate | `python3 -m unittest discover -s .scripts/tests -t .scripts/tests`, or the `scripts` gate step | Standard unittest output |
| `uniffi-bindgen` (workspace-local, `crates/gramdrive-ffi/src/bin/`) | Generates Swift + Kotlin bindings from the compiled library, version-locked to the linked `uniffi` crate; pipeline documented in `crates/gramdrive-ffi/README.md` | `make bindings`, or `cargo run -p gramdrive-ffi --features bindgen --bin uniffi-bindgen -- generate --library target/debug/libgramdrive_ffi.dylib --language swift --language kotlin --out-dir .temp/bindings` | Generated sources in `.temp/bindings/` (build artifacts, never committed) |
| `.scripts/smoke/run_bindings_smoke.py` | End-to-end bindings smoke: builds the FFI library, generates bindings, compiles and runs the Swift and Kotlin smoke consumers (`.scripts/smoke/{swift,kotlin}/`) asserting async, progress, error, and cancellation round-trips | `make smoke-bindings`, or `python3 .scripts/smoke/run_bindings_smoke.py [--skip-swift] [--skip-kotlin]` (needs `swiftc`, `kotlinc`, `java`) | Exit 0 + `BINDINGS SMOKE PASSED`, or non-zero with the failing step's log; artifacts and per-step logs in `.temp/bindings-smoke/` |
| `.scripts/smoke/run_shared_state_smoke.py` | Multi-process shared-state smoke (TASK-260715-gnsa2s): a Rust coordinator process seeds a substitute App Group container, two concurrent Swift provider processes (`apple/GramDriveSupport` over the packaged artifact) must read byte-identical item metadata, and a watcher process must observe the Darwin change doorbell plus the `dataVersion` probe across a foreign commit | `make smoke-shared-state`, or `python3 .scripts/smoke/run_shared_state_smoke.py [--repackage]` (macOS; needs Xcode; stages `make package` when no artifact is present) | Exit 0 + `SHARED-STATE SMOKE PASSED`, or non-zero with the failing step's output; container and per-step logs in `.temp/shared-state-smoke/` |
| `.scripts/packaging/build_core_artifacts.py` | Builds what native consumers ship against: release staticlib (LTO restored via a crate-type override), Swift bindings generated from that exact binary, XCFramework, manifest (contract version read from the built artifact, `git describe`, toolchain), checksums, and a deterministic zip; verifies it all by resolving and running a real minimal SwiftPM package (`.scripts/packaging/swift-consumer/`). Owns the shipped-target list | `make package`, `make package-reproducible`, or `python3 .scripts/packaging/build_core_artifacts.py [--skip-verify] [--check-reproducible]` (macOS; needs `xcodebuild`, `swift`) | Exit 0 + `PACKAGING PASSED`, or non-zero with the failing step's log; artifacts, `manifest.json`, `CHECKSUMS.sha256` and per-step logs in `.temp/packaging/` |
| `.scripts/smoke/run_agent_lifecycle_smoke.py` | Multi-process agent-lifecycle smoke (TASK-260715-1yx9ly): the `gramdrive-agent` companion binary over a substitute container — startup with health served over the bounded UNIX-socket IPC, single-instance refusal (exit 2) of a second agent, SIGTERM drain cancelling a hosted transfer through its token (exit 0, endpoint removed), and a successor starting immediately after SIGKILL with healthy durable state | `make smoke-agent-lifecycle`, or `python3 .scripts/smoke/run_agent_lifecycle_smoke.py [--repackage]` (macOS; needs Xcode; stages `make package` when no artifact is present) | Exit 0 + `PASSED: agent lifecycle smoke`, or non-zero with the failing step's output; container and per-step logs in `.temp/agent-lifecycle-smoke/` |
| `.scripts/tdlib/build_tdlib.py` | Reproducible build of the pinned TDLib tdjson artifact the local Telegram source links against (BSL-1.0 recorded per POL-6): fetch at the pinned commit, CMake build, staged `libtdjson.dylib` + headers + license, manifest and checksums, proved by the `link-smoke/` Rust binary; pipeline documented in `.scripts/tdlib/README.md` | `make tdlib`, `make tdlib-smoke` (re-run only the link smoke), `make tdlib-verify` (same-path reproducibility), or `python3 .scripts/tdlib/build_tdlib.py` (macOS arm64; needs Xcode clang, `cmake`, `gperf`, Homebrew `openssl@3`) | Staged artifact, `manifest.json` and `CHECKSUMS.sha256` in `.temp/tdlib/out/`; smoke prints the running library's version |
| `make tdjson-smoke` | Real-linkage smoke of the `gramdrive-source-tdjson` runtime (crate docs: `crates/gramdrive-source-tdjson/README.md`): with `GRAMDRIVE_TDLIB_ARTIFACT_DIR` set, the crate's env-gated `build.rs` links the staged `libtdjson.dylib` and the otherwise-empty `real_tdjson_smoke` test drives correlation, client close, and shutdown against the real library. Every `make check` runs the same runtime mock-only, artifact-free | `make tdjson-smoke` (after `make tdlib` staged the artifact) | `cargo test` output: 1 test against the real library, exit non-zero on failure |
