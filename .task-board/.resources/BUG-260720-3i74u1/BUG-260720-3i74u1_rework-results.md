# BUG-260720-3i74u1 — Rework results (3 majors + should-fix + minors)

Rework pass over review verdict RUN-260719-5b6015. Control-channel design
accepted and not touched. Live Telegram E2E deliberately NOT attempted
(external blocker; owner closes it on the release build).

## Majors (all fixed)

1. **AuthSession Drop → close** (`crates/gramdrive-ffi/src/auth.rs`).
   Added `impl Drop for AuthSession { fn drop → self.close() }`. A host that
   drops the handle without `close()` previously stranded the pump thread on
   `recv_timeout` and held the sign-in slot `ScopeGuard` forever, blocking
   every future sign-in. `close()` is idempotent, closes the client, ends the
   update stream, and unwinds the pump — releasing the slot scope the pump
   owns. New test `dropping_a_session_without_close_frees_the_sign_in_slot`
   proves a fresh sign-in over the same root succeeds after a drop.

2. **persist_account acquires the real account's scope** (`auth.rs`
   `persist_account`). Finalization mutates the real account's storage, vault
   key, and durable row while the pump holds only the *slot* scope (account 0).
   Now it takes `ScopeGuard::acquire(data_dir, real_account)` before any
   mutation; contention (a concurrent probe/remove of the same id, e.g. Repair
   during finalize) fails the finalize fail-safe (`finalize-account-busy`)
   instead of racing the wipe/rename. New test
   `finalization_fails_safe_when_the_account_scope_is_held` proves Failed is
   emitted and nothing was moved/re-homed under contention.

3. **Removed the keychain `--exec` mode**
   (`.scripts/keychain/provision-telegram-credentials.swift`). The `--exec`
   read+exec path was a promptless secret-exfiltration primitive with zero
   callers. Removed the mode and the `readItem` helper entirely, and **dropped
   the tool's self-trust from the item ACL** (`trustedPaths` no longer includes
   `CommandLine.arguments[0]`) so the compiled tool in `.temp/` has no standing
   read authority. Only the product binaries + `/usr/bin/security` (dev
   inspection) remain trusted. Kept the write (provision) path intact.

## Should-fix (fixed)

- **Post-embed `otool -L` assertion** (`.scripts/apple-app/build_app_bundle.py`,
  new `assert_no_absolute_runtime_refs`, called at the end of
  `embed_runtime_libraries`). Reads the shipped Mach-Os back and fails the build
  if any executable/dylib still loads a runtime library by an absolute
  staged/Homebrew path (the silent-no-op failure mode of `install_name_tool
  -change`). Verified live: `make package-app` PASSED with the assertion active,
  and `otool -L` on all shipped Mach-Os shows only `@rpath`/system references.

## Minors (from the verdict)

- **AuthCommand redacting Debug** (`auth.rs`): dropped `#[derive(Debug)]`, added
  a manual impl that redacts `code`/`password` (phone stays visible, as TDLib
  echoes it).
- **Starting emitted on the pump thread** (`auth.rs`): moved
  `on_phase(Starting)` out of the constructor (often a main thread) into the
  pump's first action, honoring the listener contract.
- **Canonicalized scope key** (`auth.rs` `ScopeGuard::acquire`): the scope key
  now canonicalizes `data_dir` (falling back to the raw string when it does not
  resolve yet), so two spellings of the same root share one scope.
- **Bounded terminate_session inner waits** (`removal.rs`): the plumbing waits
  are now bounded by the outer deadline instead of a fresh full `TERMINATE_TIMEOUT`
  each, so the stage stays within one timeout instead of ~doubling it.
- **Smoke `failed`-event handling + masking** (`run_control_auth_smoke.py`): a
  `failed` event during submit-wait now returns a clean reason instead of
  blocking to a raw `socket.timeout`; added `mask_phone`/`mask_account` and
  applied them to every phone/account line logged to stdout.
- **Provision prove-runnable-before-delete** (`provision_telegram_credentials.py`
  + swift `--check`): the driver runs the signed tool's new side-effect-free
  `--check` after signing and *before* clearing the existing keychain items, so
  a tool that cannot load never destroys the only copies of the credentials.
- **Build-script + FFI test coverage**: new `RuntimeEmbeddingTest`
  (`core_tdjson_linked`, hermetic skip, embed→@rpath, the portability assertion
  clean+dirty, Frameworks signing order); new FFI tests
  `cancel_is_accepted_in_the_unsupported_state` and
  `finalization_reports_failed_when_the_identity_read_fails` (Failed code path).

## Minor NOT fixed — documented rationale

- **auth.rs `shared_runtime` OnceLock caches a `TdRuntime::start` failure**:
  left as-is. `RealTdJson::claim()` is one-shot (returns `None` after the first
  claim) and `TdRuntime::start` consumes the sender/receiver halves by value, so
  a transient start failure cannot be cleanly retried without holding and
  re-feeding the claimed halves — which requires an API change in
  `gramdrive-source-tdjson`. The rework scope forbids redesigning the accepted
  control-channel/runtime surface, and `TdRuntime::start` only fails on a thread
  spawn failure (process-level, effectively fatal). Caching is the honest
  behavior under the current one-shot API; flagged here for a future source-crate
  API pass.

## Verification (all re-run this pass)

- `make check` (suite all): **8/8** — toolchain, format (`cargo fmt --check`),
  lint (`cargo clippy -D warnings`), test (`cargo test --workspace` incl. 4 new
  FFI tests → 47 ffi tests), architecture, supply-chain, traceability, scripts
  (`.scripts/tests` incl. 7 new build-script tests). Log:
  (`make check` console, 8/8).
- Core restage `make package` (GRAMDRIVE_TDLIB_ARTIFACT_DIR set): **PASSED** —
  confirms the `real_tdjson` release build compiles with these changes; contract
  0.6.0 unchanged, verifier green. Log: `rework/package-core.log`.
- `swift test` (suite apple): **2/2** — swift-build + swift-test green against
  the restaged core. Log: `rework/check-apple.log`.
- `make package-app`: **APP PACKAGING PASSED** — assembles + signs (Developer ID
  262RZ595FP), the new otool assertion active and not tripped; shipped Mach-Os
  verified `@rpath`-only. `spctl: rejected` is expected for an un-notarized
  build (unchanged from the accepted run). Log: `rework/package-app.log`.

## Live Telegram E2E — NOT run (external blocker, owner-owned)

Per the rework scope and the owner decision on the board (2026-07-20): the last
AC hop (Telegram accepting a real code end-to-end) is closed by the owner
signing in on the released build, not by any agent — Telegram retired
shared-test-number auto-codes (tdlib/td#3361). Every other hop is proven live
from the shipped bundle. Checklist "Implementation matches AC" stays as the
reviewer/owner set it; not altered by this pass.
