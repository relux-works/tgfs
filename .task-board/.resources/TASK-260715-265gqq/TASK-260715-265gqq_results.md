# TASK-260715-265gqq — UniFFI API and generation pipeline: results

Date: 2026-07-17. Role: developer. Code-complete, all gates green, **ready for
review**. The licensing blocker that parked this task is resolved (DEC-021,
owner-accepted 2026-07-17); enforcement landed in `deny.toml`.

## What landed

### Contract (`crates/gramdrive-ffi/src/api.rs`)

Provider-neutral surface, declared with UniFFI **proc-macros** (decision:
proc-macros over UDL — single compiler-checked source of truth; rationale in
crate README). Namespace `gramdrive`; Swift module `GramDriveCore`, Kotlin
package `com.reluxworks.gramdrive.core` (POL-7, pinned in `uniffi.toml`).

- `ContractVersion` record + `contract_version()` — interface version,
  independent of crate versions (0.1.0).
- `DriveError` — 9 stable error categories (NFR-030): InvalidArgument,
  NotFound, AuthRequired, RateLimited(+retry_after_ms), SourceUnavailable,
  Storage, Integrity, Cancelled, Internal. Category = contract; `detail`
  string = diagnostics only.
- `TransferProgress` record + `ProgressListener` foreign-implemented
  callback trait (`with_foreign`).
- `CancellationToken` exported object (tokio watch-backed): explicit,
  in-band cancellation; operations fail with `DriveError::Cancelled` at the
  next cancellation point (NFR-025).
- `DriveCore` object: validated constructor (`CoreConfig`), `data_dir()`,
  and `probe_transfer(...)` — the boundary conformance probe exercising
  async + progress + errors + cancellation without a Telegram account.

Zero Telegram/TDLib/gotd and zero OS-native types in the surface (DEC-003);
paths are strings, times are integer milliseconds. Architecture gate green.

### Generation pipeline

- Workspace-local `uniffi-bindgen` bin inside `gramdrive-ffi`, gated behind
  the tooling-only `bindgen` feature (`required-features`), version-locked
  to the linked `uniffi` crate. Library-mode generation from the compiled
  dylib. `make bindings` → `.temp/bindings/`.
- Generated bindings are build artifacts (never committed); UniFFI API
  checksums bind them to the exact library build.

### Smoke consumers (AC)

`.scripts/smoke/run_bindings_smoke.py` (`make smoke-bindings`): builds the
library, generates Swift+Kotlin bindings, compiles a real Swift consumer
(links the staticlib) and a real Kotlin/JVM consumer (loads the cdylib via
JNA; jars sha256-pinned), runs both. Both assert: contract version,
constructor error round-trip, async success + per-chunk progress, async
error round-trip, token cancellation round-trip (+ no callbacks after
cancel), and (Kotlin bonus) coroutine-cancellation dropping the future.
Result: `SWIFT SMOKE PASSED`, `KOTLIN SMOKE PASSED`, `BINDINGS SMOKE
PASSED`.

### Threading/async model + versioning policy (DoD)

Documented in `crates/gramdrive-ffi/README.md`: tokio drives the core
(`async_runtime = "tokio"`); exported futures are polled by the foreign
binding and must never block; callback dispatch rules (background thread,
non-blocking, non-throwing); panic→unwind→binding error; contract semver
policy + checksum-enforced build pairing.

### Licensing enforcement (DEC-021 — the former blocker)

`deny.toml` grants the two out-of-POL-6 licenses as **per-crate
`[licenses.exceptions]` entries, not blanket `allow` entries**:

- `MPL-2.0` → the 8 `uniffi*` crates actually in the tree (uniffi,
  uniffi_bindgen, uniffi_core, uniffi_internal_macros, uniffi_macros,
  uniffi_meta, uniffi_pipeline, uniffi_udl).
- `Unicode-3.0` → `unicode-ident` only. Its expression is
  `(MIT OR Apache-2.0) AND Unicode-3.0`, so the MIT half resolves against the
  existing allow list and only the Unicode-3.0 term needs the exception.

**Deviation from the unblock instruction, called out for review:** the
instruction said to add both licenses to `[licenses] allow`. I used
`exceptions` instead. DEC-021 grants these licenses to *named* crates
("named POL-6 exceptions"); a blanket `allow` would let any future
dependency carry MPL-2.0 in silently, which is the outcome the decision row
exists to prevent, and it contradicts deny.toml's own stated philosophy
("the gate is what makes it a fact rather than an intention"). `exceptions`
is the mechanism that makes "named" enforceable and is strictly narrower.
Names are unversioned, matching the existing `[bans.build]` convention: a
patch bump must not flip the gate, but a *new* `uniffi_*` crate fails until
added on purpose. `.spec/policies.md` POL-6 was corrected accordingly (it
described "allow entries"); the DEC-021 decision row itself is untouched.

## Key findings (also in LOGBOOK.md)

1. **uniffi 0.32 Swift does NOT propagate Task cancellation** — generated
   poll loop is not cancellation-aware; `CALL_CANCELLED` handler is
   `fatalError("Cancellation not supported yet")`. Kotlin frees the future
   on coroutine cancel but never calls `rust_future_cancel`. → cancellation
   made an explicit contract token; re-evaluate per uniffi upgrade.
2. **Error fields must never be named `message`** — collides with
   `kotlin.Exception.message`; generated Kotlin does not compile. Field is
   `detail`; rule recorded in api.rs + README.
3. **Licensing**: resolved via DEC-021 (see above). All `uniffi*` crates are
   MPL-2.0; `unicode-ident` is `(MIT OR Apache-2.0) AND Unicode-3.0`.
4. **The DEC-021 commit (fc3b594) left the traceability gate red** — it added
   the decision row to `.spec/decisions.md` but no matching row in
   `docs/TRACEABILITY.md`, and `validate_traceability.py` requires every
   spec-defined ID to have one ("missing matrix row for DEC-021"). Fixed
   here: DEC-021 row added (elements: TASK-260715-265gqq,
   TASK-260715-152wjq), POL-6 row note updated to cite DEC-021. Worth
   knowing: adding a decision row is a two-file change, and the gate that
   catches the omission is in the `repo` suite, not `core`.

## Verification

Full suite re-run after the deny.toml change — `make check` → **8/8 passed**:

| Check | Result |
|---|---|
| toolchain | ok |
| format (`cargo fmt --all --check`) | ok |
| lint (`cargo clippy --workspace --all-targets --all-features -D warnings`) | ok |
| test (`cargo test --workspace --all-features`) | ok |
| architecture (`check_crate_architecture.py`) | ok |
| supply-chain (`cargo deny check` — licenses/advisories/bans/sources) | **ok** (was RED) |
| traceability | ok (was RED — see finding 4) |
| scripts (unittest) | ok |
| `make smoke-bindings` (Swift + Kotlin, out-of-gate) | BINDINGS SMOKE PASSED |

Gate provenance: `.temp/acceptance/local-all/`.
Smoke logs: `.temp/bindings-smoke/*.log`.

Provider-neutrality re-checked by grep over the FFI surface: no
Telegram/TDLib/MTProto/OS-native types; the only "Telegram" occurrence is a
doc comment stating the probe runs *without* a Telegram account.

## Environment / tooling notes

- Installed `kotlin` (kotlinc 2.x) via `brew install kotlin` for the Kotlin
  smoke — documented in root README prerequisites.
- JVM jars pinned by version + sha256 in the smoke runner: jna-5.17.0,
  kotlinx-coroutines-core-jvm-1.10.2 (Maven Central).
- `deny.toml [bans.build]` allow list populated (13 build-script crates from
  the uniffi/tokio trees, all version/feature probes) — the edit the config
  comment explicitly reserved for this task.
- cargo-deny 0.20.2 locally; config schema floor is 0.18
  (`.scripts/check_toolchain.py`).

## Files changed

`Cargo.toml` (workspace deps uniffi/tokio), `Cargo.lock`,
`crates/gramdrive-ffi/{Cargo.toml, uniffi.toml, README.md, src/lib.rs,
src/api.rs, src/bin/uniffi_bindgen.rs}`, `crates/README.md`, `deny.toml`,
`Makefile`, `README.md`, `docs/OPEN_QUESTIONS.md`, `docs/TRACEABILITY.md`,
`.spec/policies.md`, `LOGBOOK.md`, `.scripts/smoke/{run_bindings_smoke.py,
swift/main.swift, kotlin/Main.kt}`. Nothing committed (per workflow: commits
happen after human review).
