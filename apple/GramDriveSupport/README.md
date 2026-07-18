# GramDriveSupport

The Apple provider-support Swift package (`.spec/architecture.md`): the
app, the companion agent, and the File Provider extension all link this
package so every GramDrive process resolves the same shared container,
derives the same file layout, and follows the same multi-process rules.
Owned by TASK-260715-gnsa2s (STORY-260715-33oacu, macos-engine-host).

Policy lives in the Rust core (`crates/gramdrive-ffi/src/shared_state.rs`
— WAL-only open, role rights, snapshot reads, migration-on-open,
coordinator-only corruption recovery); this package binds it to the Apple
host: App Group resolution, `URL`-shaped entry points, and the Darwin
change doorbell.

## Surface

| Type | Owns |
|---|---|
| `AppGroup` | The container identity (`262RZ595FP.com.reluxworks.gramdrive`, the team-prefixed entitlement form v1 ships — DEC-019/POL-7) and the data-root rule: `Library/Application Support/GramDrive` inside the container. Everything below the data root comes from the core's `sharedStateLayout`, so Swift and Rust can never disagree about paths |
| `SharedState` | Role-based open: `openInAppGroupContainer(role:)` for product processes, `open(dataRoot:role:)` for tests and tools. The engine host opens as `.coordinator`, the File Provider extension and UI surfaces as `.provider` |
| `ChangeSignal` | The cross-process change doorbell: a payload-free Darwin notification (App-Group-prefixed name, which sandboxed processes may post and observe). Writers `post()` after commit; observers treat a ring as *check now* — compare `SharedStateStore.dataVersion()` and re-read only on change. Advisory, never authoritative: the database is the truth. Finder signaling (`signalEnumerator`) is a separate channel owned by the File Provider domain work |

The `gramdrive-shared-state-smoke` executable is the harness process for
`.scripts/smoke/run_shared_state_smoke.py` (reader / watcher / doorbell
modes); it is not a product target.

## The core dependency is a built artifact

`Package.swift` resolves `GramDriveCore` (XCFramework + generated
bindings) by path from `.temp/packaging/GramDriveCore`, which `make
package` stages — built artifacts are never committed
(`.scripts/packaging/README.md`). Building here without the artifact fails
at dependency resolution:

```sh
make package                                  # stage the core artifact (repo root)
cd apple/GramDriveSupport
swift build                                   # macOS 14+ arm64 (POL-5)
swift test                                    # Swift Testing suite
```

`GRAMDRIVE_CORE_PACKAGE=<path>` overrides the artifact location when
consuming a staged or released artifact elsewhere.

## Verification

- `swift test` — Swift Testing suites: App Group identity and layout
  derivation, role-based open against a substitute container, provider
  quarantine refusal, coordinator corruption recovery through the
  bindings, and doorbell post/observe/cancel round-trips.
- `make smoke-shared-state` (repo root) — the real multi-process proof:
  a Rust coordinator process seeds, two concurrent Swift provider
  processes must read byte-identical item metadata through the packaged
  artifact, and a watcher process must observe the doorbell plus the
  data-version probe across a foreign commit. The Rust-side stress and
  SIGKILL crash tests live in `crates/gramdrive-state/tests/multiprocess.rs`.

## Substitute containers

Product processes resolve the real App Group container (which requires
GramDrive signing and entitlements). Tests and the smoke pass a substitute
container directory through the same `AppGroup.dataRootURL(containerURL:)`
rule — the layout code path is identical; only the root differs.
