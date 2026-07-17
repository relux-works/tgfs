# GramDrive core workspace architecture

The shared Rust drive core (`.spec/architecture.md`) is a Cargo workspace of
seven crates. Crate names use the `gramdrive` product namespace (POL-7): the
`tgfs` codename never appears in a shipped identifier, and crate names leak
into shipped artifacts (`libgramdrive_ffi.dylib`, symbol names).

The crate set, dependency direction, and platform-neutrality rules below are
enforced by `python3 .scripts/check_crate_architecture.py` (CI-suitable,
stdlib-only); the supply-chain gate by `cargo deny check`. Those scripts are
the executable form of these rules; changing an enforced rule means changing
this document and the script's policy table in the same commit.

Every gate runs through one entrypoint, `.scripts/acceptance/run_automated.py`
— see [Commands](#commands).

Rules stated here that no check enforces yet, and are convention until they
have one: source implementations staying separate crates rather than features,
the "no cargo features" baseline, and layer numbering beyond what the
direction allow list implies.

Platform neutrality is enforced by static scan only. A cross-target build would
catch what the scan cannot (a `std::os` call reached through a dependency, for
instance), but v1 has exactly one supported target — macOS 14+ arm64, POL-5 —
so there is no second target to build against, and a scan is the whole of what
is available. The gap closes from two directions: a platform host crate
(Windows CfAPI, Linux FUSE) brings its target with it, and TASK-260715-3faqmr
owns the barycenter blind cross-build job. Until one of those lands, a
neutrality violation invisible to the scan ships undetected.

## Crates and layers

| Layer | Crate | Responsibility | Owning board story |
|---|---|---|---|
| 0 | `gramdrive-model` | Domain vocabulary: identity, virtual tree, naming, versions, cursors, byte ranges | STORY-260715-3qxar5 (identity-and-namespace) |
| 1 | `gramdrive-source` | Provider-neutral `DriveSource` contract (DEC-003) | STORY-260715-255sa3 (drive-source-contract) |
| 1 | `gramdrive-state` | SQLite state store, schema migrations, reconciliation | STORY-260715-16ik2x (metadata-state-store) |
| 1 | `gramdrive-render` | Deterministic NDJSON/Markdown renderers and render planning | STORY-260715-1oq9jg (deterministic-rendering) |
| 2 | `gramdrive-engine` | Transfer/cache engine: hydration, quota, eviction, resumable downloads | STORY-260715-2hs8cf (transfer-and-cache-engine) |
| 3 | `gramdrive-ffi` | UniFFI boundary; the only crate native consumers link | STORY-260715-2p879f (workspace-and-bindings) |
| — | `gramdrive-testkit` | Deterministic fake source, fixtures, conformance helpers | STORY-260715-255sa3 (drive-source-contract) |

```text
                 gramdrive-ffi            (layer 3 — FFI boundary)
                       |
                 gramdrive-engine         (layer 2 — orchestration)
                /      |       \
  gramdrive-source gramdrive-state gramdrive-render   (layer 1)
                \      |       /
                 gramdrive-model          (layer 0 — vocabulary)

  gramdrive-testkit -> {model, source}    (dev-dependency only for product crates)
```

## Dependency direction rules

Dependencies point strictly downward. The authoritative allow list
(mirrored in the check script):

| Crate | May depend on (internal, `[dependencies]`/`[build-dependencies]`) |
|---|---|
| `gramdrive-model` | — |
| `gramdrive-source` | `gramdrive-model` |
| `gramdrive-state` | `gramdrive-model` |
| `gramdrive-render` | `gramdrive-model` |
| `gramdrive-engine` | `gramdrive-model`, `gramdrive-source`, `gramdrive-state`, `gramdrive-render` |
| `gramdrive-ffi` | every core crate |
| `gramdrive-testkit` | `gramdrive-model`, `gramdrive-source`, `gramdrive-render` |

Additional rules:

- **No cycles.** Checked on the actual graph, independently of the allow list.
- **`gramdrive-testkit` is never a `[dependencies]` entry of a product
  crate.** Any crate may use it as a `dev-dependency`. Test support must not
  ship in product artifacts.
- **Nothing depends on `gramdrive-ffi`.** It is the top of the graph.
- **New crates fail the check until this table and the script's policy table
  list them.** That is deliberate: adding a crate is an architecture change.

## Feature and platform policy

- **Core crates contain no platform-specific code.** Specifically, none of:
  - a `target_os`/`target_family`/`target_vendor`/`windows`/`unix` predicate
    in any cfg form — `cfg(...)`, `cfg!(...)`, `cfg_attr(...)`, including
    nested `all()`/`not()`/`any()` and arguments wrapped across lines;
  - a dependency from the platform ban list (`windows`, `windows-sys`,
    `fuser`, `jni`, `objc2`, `core-foundation`, …; full list in the check
    script), in any section including dev;
  - a target-gated dependency (`[target.'cfg(...)'.dependencies]`), in any
    section including dev — platform-conditional deps are leakage whatever
    the dep is named;
  - a `std::os::` path, which compiles per-platform with no cfg and no
    dependency to give it away.

  The source scans are deliberately fail-closed: a predicate word inside a
  block comment or string literal is flagged, because a false positive costs
  a rename and a miss costs the guarantee. OS placeholder objects, extension
  lifecycle, secure-store APIs, and UI types live in the native adapter
  layers (`.spec/architecture.md`).
- **Source implementations are separate crates, not feature flags**
  (DEC-003/DEC-005). Reserved names: `gramdrive-source-tdjson` (local TDLib
  via tdjson, EPIC-260715-2ptb18) and `gramdrive-source-remote` (gotd/td
  service client). Features leak transitively across a workspace; a separate
  crate keeps TDLib linkage out of every build that does not ask for it, and
  keeps the core honest about DEC-003 (no provider types in core).
- **Platform host crates (future Windows CfAPI / Linux FUSE hosts) join the
  workspace as new members** with their own policy rows; the platform ban
  list applies per-crate, not globally.
- Workspace crates declare **no cargo features** except documented
  tooling-only ones: a crate that introduces a feature must document it in
  its README, and features must never toggle provider or platform code into
  a core crate. The single current feature is `gramdrive-ffi/bindgen`,
  which gates the workspace-local `uniffi-bindgen` binary and is never
  enabled by a product build (`crates/gramdrive-ffi/README.md`).

## Toolchain and quality configuration

Every gate compiles with one pinned compiler, and every config file states why
it deviates from a default:

| File | Owns |
|---|---|
| `rust-toolchain.toml` | The exact toolchain (1.91.0) and its components. An exact version, not `stable`: a floating channel changes the lint set and rustfmt output between runs |
| `rustfmt.toml` | Formatting. Stable options only — a nightly option would need `cargo +nightly fmt`, defeating the pin |
| `clippy.toml` | Clippy knobs with no manifest equivalent (test-only exemptions for `unwrap`/`expect`/`panic`) |
| `Cargo.toml` `[workspace.lints]` | Lint **levels**. In the manifest rather than a CI flag, so an editor running clippy on save shows the same verdict as the gate. Each crate opts in with `[lints] workspace = true`; the architecture check fails a crate that forgets, because the failure is otherwise silent |
| `Cargo.toml` `[profile.*]` | Build profiles. `overflow-checks` stays on in release (offset and quota arithmetic that wraps is a silent data error, NFR-012); `panic = "unwind"` is required — UniFFI turns panics into FFI errors via `catch_unwind`, and `abort` would crash the host app. `lto = "thin"` reaches the shipped `gramdrive-ffi` artifact only via packaging's crate-type override — see below |
| `deny.toml` | Supply chain: POL-6 licenses, RustSec advisories, bans, sources |

`python3 .scripts/check_toolchain.py` asserts the pin is actually in effect.
rust-toolchain.toml only binds when rustup drives cargo; a distro rustc or a
container with a baked-in toolchain ignores it and still prints "Finished".

**Release LTO reaches the shipped artifact only through packaging.** Cargo omits
`-C lto` from any rustc invocation that also produces an rlib, since rustc
cannot LTO an rlib output. `gramdrive-ffi` is `crate-type = ["lib", "staticlib",
"cdylib"]` — the rlib serves the Windows CfAPI and Linux FUSE hosts — so a plain
`cargo build --release` of it links **without** LTO, silently.

Resolved by TASK-260715-3akqs8 without an architecture change: the packaging
pipeline builds the shipped library with `cargo rustc --crate-type staticlib`,
which emits only the type that ships and restores `-C lto=thin`
(`.scripts/packaging/README.md`). Splitting the rlib into its own crate is
therefore unnecessary. The consequence to keep in mind: `lto = "thin"` is a
statement about `make package` output, not about an ad-hoc
`cargo build --release`.

Bumping Rust means changing `channel` in `rust-toolchain.toml` and
`workspace.package.rust-version` in `Cargo.toml` together — the check fails if
the declared MSRV outruns the pinned compiler.

## Supply-chain gate (POL-6, NFR-050)

`cargo deny check` runs four checks: **licenses** (POL-6: the permissive
allow-list — anything else, even a permissive license outside the set, needs an
owner-approved decision row in `.spec/decisions.md` first), **advisories**
(RustSec vulnerabilities and unmaintained crates), **bans** (wildcard
dependencies, build scripts), and **sources** (crates.io only).

cargo-deny is the only tool here rather than cargo-audit alongside it: both read
the same RustSec database, so one tool means one config and one place a rule can
hide. Licenses, bans and sources are hermetic; advisories are evaluated against
a database that changes, so a commit that passes today can fail tomorrow when a
CVE lands. That is the check working, not flaking.

## Commands

One entrypoint runs every gate, and CI runs the same one — a check that lives
only in a YAML file cannot be run before pushing:

```sh
make check                                    # everything CI runs
make check-core                               # Rust core gates only
make gates                                    # list suites and their commands
python3 .scripts/acceptance/run_automated.py --suite core --run-id local-core
```

| Suite | Steps |
|---|---|
| `core` | `toolchain`, `format`, `lint`, `test`, `architecture`, `supply-chain` |
| `repo` | `traceability`, `scripts` |
| `all` | `core` + `repo` |

Any step name also works as `--suite` for a one-step run
(`--suite supply-chain --run-id local-sc`). Each run writes provenance to
`.temp/acceptance/<run-id>/`: `summary.json` (commit, worktree state, tool
versions, per-step exit codes and durations) plus a log per step. CI uploads
that directory as an artifact; `--require-clean` refuses to run against a dirty
worktree, which is what makes the recorded commit mean anything (NFR-052).

Inner-loop commands, which are not gates:

| Command | Purpose |
|---|---|
| `make fmt` | Apply rustfmt (the gate's `format` step only checks) |
| `make build` | Build every crate (reference host: macOS 14+ arm64, POL-5) |
| `make test` | Run all crate tests without provenance |
| `cargo test -p <crate>` | Run one crate's tests (also in each crate README) |
| `make bindings` | Generate Swift + Kotlin bindings into `.temp/bindings` (library mode; `crates/gramdrive-ffi/README.md`) |
| `make smoke-bindings` | Compile and run the Swift and Kotlin smoke consumers against freshly generated bindings (`.scripts/smoke/`) |

The UniFFI contract, generation pipeline, threading/async model, and
interface versioning policy live in `crates/gramdrive-ffi/README.md`
(TASK-260715-265gqq). Artifact packaging, including the shipped-target list
and dSYM/stripping decisions, is TASK-260715-3akqs8; CI jobs and the blind
cross-build gate are TASK-260715-3faqmr.

**Licensing — two named POL-6 exceptions, gate green.** The uniffi crates are
MPL-2.0 and `unicode-ident` is `(MIT OR Apache-2.0) AND Unicode-3.0`; both
licenses sit outside the POL-6 allow list and are owner-accepted named
exceptions per **DEC-021** (2026-07-17). `deny.toml` enforces them as
per-crate `[licenses.exceptions]` rather than blanket `allow` entries, so the
grant reaches only the crates named there — any further license, or any
further crate carrying these two, fails the gate until a new decision row
covers it. All four `cargo deny` checks (licenses, advisories, bans, sources)
pass.
