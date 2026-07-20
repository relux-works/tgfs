# BUG-260720-3i74u1 — Rework review verdict: ACCEPTED (→ done)

Reviewer, 2026-07-20. Read-only review; no code modified. Reviews the rework of
the 0338 verdict (`BUG-260720-3i74u1_review-verdict.md`): 3 confirmed majors +
1 should-fix + minors. The control-channel design was accepted in the prior
cycle and was not re-litigated here.

## Verdict

ACCEPTED. Every item the rework scope required is correctly resolved, both heavy
test suites are green on an independent re-run, and no new defect was introduced
by the two Rust lifecycle/concurrency fixes. The live-Telegram final hop is
owner-owned by the 2026-07-20 decision (not an agent gap), so it does not block
acceptance.

## Majors — all fixed and independently verified

1. **AuthSession `Drop → close` (`crates/gramdrive-ffi/src/auth.rs`)** — `impl
   Drop for AuthSession` calls the idempotent `close()`. `close()` only flips an
   atomic and sends `client.close()` (non-blocking) — safe in `Drop`, and it can
   never run on the pump thread because the pump holds `Arc<SessionShared>`, not
   the `AuthSession` handle. The client close ends the update stream, the pump
   returns, and the slot `ScopeGuard` (moved into the pump thread) drops.
   Adversarial check: no deadlock, no double-close hazard (swap-guarded), and a
   `Drop` mid-finalize degrades to a graceful `finalize-*` failure, not
   corruption. Regression test `dropping_a_session_without_close_frees_the_sign_in_slot`
   passes.

2. **`persist_account` acquires the real account's scope (auth.rs)** — the real
   account's `ScopeGuard::acquire` is the FIRST statement of `persist_account`,
   before the vault store, the wipe, the rename, and the row write. Contention
   (a concurrent probe/remove of the same id, e.g. Repair during finalize)
   returns `finalize-account-busy` with zero mutation. Scope acquisition is a
   non-blocking global `HashSet` insert (fails fast, never blocks), so holding
   the slot key (`root#0`) and the account key (`root#<id>`) simultaneously
   cannot deadlock. The happy path still reaches `Complete` (no spurious busy) —
   confirmed by `phone_code_password_flow_completes_and_persists_the_account`.
   Regression test `finalization_fails_safe_when_the_account_scope_is_held`
   passes and asserts nothing moved into the contended account.

3. **Keychain `--exec` removed + tool self-trust dropped
   (`.scripts/keychain/provision-telegram-credentials.swift`)** — verified the
   tool now has NO read/consume path at all: only `SecItemDelete`+`SecItemAdd`
   (write) and a side-effect-free `--check`. `readItem` and `--exec` are gone.
   `trustedPaths` starts `["/usr/bin/security"]` and appends only the
   `--agent`/`--app` product binaries — `CommandLine.arguments[0]` is never
   added, so the compiled tool in `.temp/` holds no standing read authority over
   the items. The promptless team-signed exfiltration primitive is eliminated,
   not merely narrowed. Write/provision path intact.

## Should-fix — fixed

- **`assert_no_absolute_runtime_refs` (`build_app_bundle.py`)** reads the shipped
  Mach-Os back after embed and fails on any absolute staged/Homebrew load — the
  exact silent `install_name_tool -change` no-op the 0338 verdict flagged.
  Exercised directly by new `RuntimeEmbeddingTest` cases: clean (passes),
  surviving-staged-ref (fails, names `gramdrive-agent`), Homebrew ref (fails),
  and Frameworks-signed-before-binaries order.

## Minors — addressed

`AuthCommand` manual redacting `Debug` (code/password hidden, phone visible);
`Starting` emitted on the pump thread not the constructor; `ScopeGuard::acquire`
canonicalizes `data_dir`; `removal.rs` terminate plumbing waits bounded by the
outer deadline; smoke handles a `failed` event during submit-wait and masks
every phone/account line (`mask_phone`/`mask_account`); provisioning driver runs
the signed tool's `--check` before deleting the existing keychain items. New FFI
tests `cancel_is_accepted_in_the_unsupported_state` and
`finalization_reports_failed_when_the_identity_read_fails`.

**OnceLock deferral (documented, not fixed):** `shared_runtime` caching a
transient `TdRuntime::start` failure is a sound judgment — `RealTdJson::claim()`
is one-shot and `TdRuntime::start` consumes the sender/receiver halves, so a
clean retry needs a `gramdrive-source-tdjson` API change the rework scope
forbids, and `start` only fails on thread-spawn (process-fatal). Accepted.

## Independently re-run this review

- `cargo test -p gramdrive-ffi`: **47 passed / 0 failed**, including all four new
  tests (`dropping_a_session_without_close_frees_the_sign_in_slot`,
  `finalization_fails_safe_when_the_account_scope_is_held`,
  `cancel_is_accepted_in_the_unsupported_state`,
  `finalization_reports_failed_when_the_identity_read_fails`).
- `make check`: **8/8** exit 0 (toolchain, format, lint/clippy -D warnings, test
  `cargo test --workspace --all-features` 52.8s, architecture, supply-chain,
  traceability, scripts incl. the new build-script tests).
- `make check-apple`: **2/2** exit 0 (swift build + swift test against the staged
  core).

Logs: `.temp/BUG-260720-3i74u1/review2/{make-check,check-apple}.log`.

## Live Telegram E2E — correctly not attempted

The AC's first sentence (Start Sign In drives the real TDLib flow over the live
control channel; every hop up to code acceptance proven vs test infra; `notWired`
unreachable in the shipped bundle; agent auto-start honoring the login-item
preference; swift test + make check green; packaging assembles+signs) is fully
satisfied and was proven live in the prior cycle. The AC's second sentence
(real code accepted, session persists) is the OWNER's step on the released build
by the 2026-07-20 decision — Telegram retired shared-test-number auto-codes
(tdlib/td#3361, LOGBOOK 0400). This is owner-owned, not an agent gap, so it does
not hold acceptance.

## Checklist disposition set by this review

- "Implementation matches AC" — checked (every agent-ownable AC hop proven; the
  final live hop is owner-owned by decision).
- Item 3 persistence sub-clause remains owner-verified on the release build.
- Verdict-evidence — this artifact + notes + LOGBOOK 1600.
