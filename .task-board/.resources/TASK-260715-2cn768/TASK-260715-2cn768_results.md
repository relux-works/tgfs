# TASK-260715-2cn768 — Pin Rust toolchain and quality configuration

Status: ready for review. Config-only; no product Rust source changed.

## What landed

| File | Role |
|---|---|
| `rust-toolchain.toml` (new) | Pins Rust **1.91.0** exactly + `rustfmt`, `clippy`, `profile = "minimal"` |
| `rustfmt.toml` (new) | `style_edition = "2024"`, `newline_style = "Unix"`. Stable options only |
| `clippy.toml` (new) | Test-only exemptions for `unwrap`/`expect`/`panic` |
| `Cargo.toml` `[workspace.lints]` | Lint **levels** (see below) |
| `Cargo.toml` `[profile.*]` | dev / test / release / bench |
| `deny.toml` | Added `[advisories]`, `[bans]`, `[bans.build]`, `[sources]` alongside the existing POL-6 licenses |
| `.scripts/check_toolchain.py` (new) | Asserts the pin is actually in effect |
| `.scripts/acceptance/run_automated.py` (new) | **The single gate entrypoint** |
| `.scripts/tests/` (new) | 47 self-tests for both scripts |
| `.scripts/check_crate_architecture.py` | New check #11: shared lint-set opt-in |
| `Makefile` | Now delegates every gate to the entrypoint |
| `README.md`, `crates/README.md` | Documented |

## Acceptance criteria

> Commands are documented, deterministic, and fail on formatting, denied lints,
> forbidden licenses, or known critical vulnerabilities according to policy.

- **Documented** — `README.md` (running the checks + tools table), `crates/README.md`
  (toolchain/quality config, supply-chain gate, commands). `make gates` prints every
  suite and the exact command behind each step.
- **Deterministic** — exact toolchain pin, and `check_toolchain.py` asserts it took
  effect rather than assuming rustup is driving. Caveat stated below.
- **Fails on formatting** — `cargo fmt --all --check`.
- **Fails on denied lints** — `[workspace.lints]` + `-D warnings`. Verified by probe.
- **Fails on forbidden licenses** — `cargo deny check licenses`, POL-6 allow-list unchanged.
- **Fails on known vulnerabilities** — `cargo deny check advisories`. Vulnerabilities are
  an unconditional error in cargo-deny 0.18+ (no severity threshold to relax), matching
  the release gate's "no unresolved critical/high security findings".

## The entrypoint (barycenter pattern)

```sh
python3 .scripts/acceptance/run_automated.py --suite core --run-id local-core
make check          # --suite all --run-id local-all
make gates          # --list
```

| Suite | Steps |
|---|---|
| `core` | `toolchain`, `format`, `lint`, `test`, `architecture`, `supply-chain` |
| `repo` | `traceability`, `scripts` |
| `all` | `core` + `repo` |

Any step name also works as `--suite` (`--suite supply-chain --run-id local-sc`), so
re-running one gate does not send anyone back to a hand-typed cargo command.

- Provenance per run → `.temp/acceptance/<run-id>/`: `summary.json` (commit, worktree
  state, tool versions, per-step command/exit/duration) + one log per step. CI uploads
  this directory; matches barycenter's `.temp/acceptance/<run-id>` contract.
- `--require-clean` refuses a dirty worktree (exit 2) — a result stamped with a commit
  that does not describe the tested tree is a false provenance record (NFR-052).
- Exit codes: `0` pass / `1` a step failed / `2` could not start.
- **Every step runs even after one fails.** The useful output of a gate run is the full
  list of what is broken, not the first thing it tripped over.
- `--run-id` is validated against `^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$` — it names a
  directory, so `../escape` must not become a write path. Tested.

CI reuse (TASK-260715-3faqmr): one job per suite, `--suite core --run-id ci-core
--require-clean`, upload `.temp/acceptance/ci-core` with `if: always()`.

## Decisions worth reviewing

- **cargo-deny only, no cargo-audit.** The task text says "deny/audit"; both read the same
  RustSec DB, so `cargo deny check advisories` covers what `cargo audit` reports. One tool
  = one config file, one version to pin, one place a rule can hide.
- **Lint levels in the manifest, not a CI flag.** `[workspace.lints]` is compiled into
  every invocation, so an editor running clippy on save shows the same verdict as the
  gate. `-D warnings` covers the warn-by-default remainder.
- **Denied:** `unwrap_used`, `expect_used`, `panic` — a panic crosses the UniFFI boundary
  as a crashed File Provider extension or a lost error category (NFR-030). `print_stdout`,
  `print_stderr`, `dbg_macro` — a library in someone else's process has no console
  (NFR-032). Tests exempted in `clippy.toml`.
- **Not adopted: `clippy::pedantic`.** It would fire on existing `gramdrive-model` code
  (`missing_errors_doc`, `must_use_candidate`), forcing edits to another task's crate to
  satisfy lint noise. Deliberate omission, not an oversight.
- **`overflow-checks = true` in release.** A wrapped offset in hydration/eviction is a
  silent data error, not a crash (NFR-012). Costs a few percent; the perf budgets
  (NFR-020..022) are provisional and I/O-bound. Revisit only with measured evidence.
- **`panic = "unwind"` stated explicitly.** UniFFI converts panics to FFI errors via
  `catch_unwind`; `abort` would crash the host app. It is the default — stated as a guard
  against a future "optimization" (TASK-260715-265gqq depends on it).
- **No `targets` in `rust-toolchain.toml`.** POL-5 gives v1 exactly one target; listing it
  would only slow toolchain install on a Linux CI runner. Shipped-target list is
  TASK-260715-3akqs8's.

## Findings (also in LOGBOOK.md)

1. **`lto = "thin"` is silently inert for the artifact that ships.** Cargo omits `-C lto`
   from any rustc invocation that also emits an rlib. `gramdrive-ffi` is
   `crate-type = ["lib", "staticlib", "cdylib"]`, so its dylib/`.a` link **without** LTO,
   with no warning. Verified: `-C lto=thin` appears with `crate-type = ["cdylib"]` alone
   and is absent with the real three-type set. Kept the setting (correct for every
   crate-type that links, live once the shipped crate-type is settled) with the caveat
   written into `Cargo.toml` and `crates/README.md`. **Fix belongs to TASK-260715-3akqs8**
   or an architecture change splitting the rlib out. Other profile flags do apply —
   `-C codegen-units=1`, `-C overflow-checks=on`, `-C debuginfo=line-tables-only` confirmed
   on the rustc command line.
2. **Cross-target builds were mis-assigned to this task.** `crates/README.md:18` said this
   task would add them. POL-5 fixes v1 at one target, so there is no second target to build
   against; a probe target like `x86_64-unknown-linux-gnu` would gate on an out-of-scope
   platform and is trivially green against stub crates. Corrected the doc: the gap closes
   via a platform host crate or TASK-260715-3faqmr's blind cross-build job, and is now
   stated as a live limitation rather than implied away.
3. **`[lints] workspace = true` is per-crate opt-in and fails open.** A crate that omits it
   is silently exempt from the entire lint set and the gate still passes green. Added as
   check #11 in `check_crate_architecture.py`; verified it fires.
4. **`group_imports` / `imports_granularity` are nightly-only.** They warn and do nothing on
   the pinned stable toolchain. Dropped — reaching them needs `cargo +nightly fmt`, which
   defeats the pin. Import grouping stays unenforced convention.
5. **`rust-toolchain.toml` only binds when rustup drives cargo.** A distro rustc or a
   container with a baked-in toolchain ignores it and still prints "Finished" — hence
   `check_toolchain.py`.
6. `[bans] allow-build-scripts` is deprecated in cargo-deny 0.20 → moved to `[bans.build]`.
   `[bans] wildcards = "deny"` flags our own `{ workspace = true }` path deps as wildcards;
   `allow-wildcard-paths = true` scopes the exemption to private crates only.

## Verification

Final `--suite all` run: **8/8 passed** (provenance attached as
`TASK-260715-2cn768_gate-run.json`).

| Check | Result |
|---|---|
| `make check` (all 8 gates) | pass, exit 0 |
| `cargo build --workspace` / `--release` / `--profile bench` | pass; `libgramdrive_ffi.dylib` + `.a` produced |
| `cargo test --workspace` | pass |
| `cargo fmt --all --check` | pass |
| `cargo clippy ... -D warnings` | pass |
| `cargo deny check` | `advisories ok, bans ok, licenses ok, sources ok`, no deprecation warnings |
| `.scripts/tests/` | 47 tests pass |
| traceability validator | pass |

**Gates verified to actually fire**, not just to pass:
- Injected `println!` + `.unwrap()` + an undocumented `pub fn` into `gramdrive-model` →
  clippy errored on all three (`missing_docs` warned → error under `-D warnings`).
  Reverted.
- Removed `[lints] workspace = true` from `gramdrive-render` → architecture check #11
  errored. Reverted.
- `--require-clean` against the dirty tree → exit 2, no steps run.
- Missing tool → step fails with exit 127 and a readable message, no traceback.

## Not done here / handoffs

- **TASK-260715-3akqs8 (packaging):** the LTO caveat above; shipped-target list;
  strip/dSYM (`debug = "line-tables-only"` is a placeholder that keeps symbolication
  possible, not a packaging decision).
- **TASK-260715-3faqmr (core CI):** wire jobs to `--suite core` / `--suite repo`; the
  blind cross-build gate; secret scanning (NFR-050) and SBOM emission (NFR-052) are named
  in that task's README and are not part of this config.
- **TASK-260715-265gqq (UniFFI):** will be the first legitimate `[bans.build]
  allow-build-scripts` entry; depends on `panic = "unwind"`.
