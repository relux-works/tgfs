# GramDrive core workspace architecture

The shared Rust drive core (`.spec/architecture.md`) is a Cargo workspace of
seven crates. Crate names use the `gramdrive` product namespace (POL-7): the
`tgfs` codename never appears in a shipped identifier, and crate names leak
into shipped artifacts (`libgramdrive_ffi.dylib`, symbol names).

The crate set, dependency direction, and platform-neutrality rules below are
enforced by `python3 .scripts/check_crate_architecture.py` (CI-suitable,
stdlib-only); the license gate by `cargo deny check licenses`. Those scripts
are the executable form of these rules; changing an enforced rule means
changing this document and the script's policy table in the same commit.

Rules stated here that no check enforces yet, and are convention until they
have one: source implementations staying separate crates rather than features,
the "no cargo features" baseline, and layer numbering beyond what the
direction allow list implies. Platform neutrality is enforced by static scan
only — cross-target builds that would catch the rest are TASK-260715-2cn768.

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
- Workspace crates currently declare **no cargo features**. A crate that
  introduces one must document it in its README; features must never toggle
  provider or platform code into a core crate.

## License gate (POL-6)

The product is proprietary; dependencies must be permissive-licensed.
`deny.toml` allows exactly MIT, Apache-2.0, BSD-2/3-Clause, BSL-1.0, Zlib,
ISC. Anything else — even permissive licenses outside the POL-6 set —
requires an owner-approved decision row in `.spec/decisions.md` first.

```sh
cargo deny check licenses
```

## Commands

| Command | Purpose |
|---|---|
| `cargo build --workspace` | Build every crate (reference host: macOS 14+ arm64, POL-5) |
| `cargo test --workspace` | Run all crate tests |
| `cargo test -p <crate>` | Run one crate's tests (also in each crate README) |
| `python3 .scripts/check_crate_architecture.py` | Enforce the crate set, dependency direction, and platform neutrality |
| `cargo deny check licenses` | POL-6 license gate |

Toolchain pinning, rustfmt/clippy configuration, and advisory scanning are
owned by TASK-260715-2cn768; UniFFI generation by TASK-260715-265gqq;
artifact packaging by TASK-260715-3akqs8.
