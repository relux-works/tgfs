# TASK-260715-3o8wpt — Rust workspace and crate boundaries: implementation notes

Task: Define Rust workspace and crate boundaries (STORY-260715-2p879f / EPIC-260715-1poogc)
Date: 2026-07-17. Reference host: macOS 15 (Darwin 25.5.0), arm64, Rust 1.91.0.

## What was built

Cargo workspace at the repo root (`Cargo.toml`, `resolver = "3"`, edition 2024,
`rust-version = 1.91`, all crates `publish = false`) with seven members under
`crates/`:

| Crate | Responsibility | Layer |
|---|---|---|
| `gramdrive-model` | Domain vocabulary (identity, tree, naming, versions, cursors, byte ranges) | 0 |
| `gramdrive-source` | Provider-neutral `DriveSource` contract (DEC-003) | 1 |
| `gramdrive-state` | SQLite state store + migrations | 1 |
| `gramdrive-render` | Deterministic NDJSON/Markdown renderers | 1 |
| `gramdrive-engine` | Transfer/cache engine (hydration, quota, eviction) | 2 |
| `gramdrive-ffi` | UniFFI boundary; only crate native consumers link; `rlib`+`staticlib`+`cdylib` | 3 |
| `gramdrive-testkit` | Fake source, fixtures, conformance helpers; dev-dependency only | — |

Architecture documentation: `crates/README.md` (layer table, dependency allow
list, feature/platform policy, commands). Each crate has a README with
`## Ownership` (board story/task mapping) and `## Test command` sections plus
crate-level rustdoc stating its boundary rules. Crates carry
`#![forbid(unsafe_code)]` (model/source/render/testkit) or `#![deny(unsafe_code)]`
(state/engine/ffi — layers that may later need audited exceptions).

Seed code: `gramdrive-model::ByteRange` (half-open, never-empty, checked
constructor + error type, 4 unit tests). Dependent crates re-export their
lower layers (`pub use gramdrive_model as model;`) so declared edges are real
library-level dependencies, each with a smoke test through the re-export.

## Enforcement (architecture check)

`python3 .scripts/check_crate_architecture.py` (stdlib-only, CI-suitable,
mirrors `crates/README.md`; uses `cargo metadata`). Fatal checks:

1. workspace member set must equal the policy table (new crate ⇒ explicit rule);
2. internal `[dependencies]`/`[build-dependencies]` ⊆ per-crate allow list;
3. `gramdrive-testkit` never a normal/build dep of a product crate;
4. nothing depends on `gramdrive-ffi`;
5. cycle detection on the actual graph (independent of the allow list);
6. platform-banned direct deps (windows/windows-sys/fuser/jni/objc2/…) in
   platform-neutral crates, all dependency sections including dev;
7. no `cfg(target_os/windows/unix/target_family/target_vendor)` in
   platform-neutral crate sources (comments stripped);
8. per-crate README with required sections.

## License gate (POL-6)

`deny.toml` + `cargo deny check licenses`. Allow list is exactly the POL-6
set: MIT, Apache-2.0, BSD-2-Clause, BSD-3-Clause, BSL-1.0, Zlib, ISC.
Deliberately fail-closed: permissive licenses outside the set (e.g.
Unicode-3.0, which arrives with syn/unicode-ident) fail the gate until an
owner-approved decision row exists — expect this the first time a
serde/proc-macro dependency is added. `all-features = true` so feature-gated
deps can't evade the gate; workspace (proprietary, unpublished) crates are
skipped via `private.ignore`.

## Key decisions

- **Source implementations are separate crates, not feature flags**
  (DEC-003/DEC-005). Reserved names: `gramdrive-source-tdjson`,
  `gramdrive-source-remote`. Rationale: features unify transitively across a
  workspace build graph, so a feature-gated tdjson would leak TDLib linkage
  into builds that never asked for it; separate crates keep DEC-003 honest.
- **Crate names use the `gramdrive` namespace (POL-7)**, not the `tgfs`
  codename: crate names leak into shipped artifacts (`libgramdrive_ffi.dylib`,
  symbols), which POL-7 forbids for `tgfs`.
- **`engine → render` is allowed but unused**; the actual edge is added only
  when the render planner integration demands it.
- **`state` depends only on `model`** — change-cursor persistence types are
  expected in the model vocabulary. If TASK-260715-1opnb2 needs the source
  trait itself, the allow list must be amended consciously.
- **Workspace crates declare no cargo features yet**; policy documented in
  `crates/README.md`.

## Verification evidence

Positive (`.temp/TASK-260715-3o8wpt/final-verification-01.log`):
- `cargo build --workspace` — green;
- `cargo test --workspace` — 10 unit tests, 0 failures (14 ok-suites incl. doc-test runs);
- `python3 .scripts/check_crate_architecture.py` — "OK: 7 crates conform";
- `cargo deny check licenses` — "licenses ok";
- `cargo fmt --all -- --check` — clean; `cargo clippy --workspace --all-targets` — 0 warnings;
- `python3 .scripts/validate_traceability.py` — still OK 201/201.

Negative (`.temp/TASK-260715-3o8wpt/negative-checks-01.log` — each violation
introduced temporarily, then reverted; final tree re-verified clean):
- NEG-1 direction violation (`render → state`) + testkit as normal dep of
  `state` → 3 errors, exit 1;
- NEG-2 `#[cfg(target_os = "macos")]` in `gramdrive-model` → error with
  file:line, exit 1;
- NEG-3 removed crate README → error, exit 1;
- NEG-4 added `option-ext@0.2.0` (MPL-2.0) to model → `licenses FAILED`,
  exit 4, with full inverse dependency tree in output.

## Tooling installed (reproducible)

- `cargo-deny` via `brew install cargo-deny` — required for the POL-6 gate.
- `rustup component add rustfmt clippy` on toolchain 1.91.0-aarch64-apple-darwin.

Also added a root `Makefile` (`make check` = arch + licenses + traceability +
build + test) and updated `README.md` (status + Tools table).

## Left to sibling tasks

- Toolchain pinning (`rust-toolchain.toml`), rustfmt/clippy config, advisory
  scanning — TASK-260715-2cn768 (this task set only `rust-version` in the
  workspace manifest and used default lint settings, which run clean).
- UniFFI dependency, UDL/proc-macro choice, generation pipeline —
  TASK-260715-265gqq (`gramdrive-ffi` deliberately has no uniffi dep yet).
- XCFramework/Android/Win/Linux artifact packaging — TASK-260715-3akqs8.
- Contract/domain types beyond the `ByteRange` seed — the owning story tasks
  listed in each crate README.

Nothing is committed to git (per workflow: review first).
