# TASK-260715-51n6jb — Review: authorization state machine

**Verdict: ACCEPTED → done.** Read-only review; all gates and tests
reproduced independently, not trusted from the implementer's results doc.

## What was reviewed

- `crates/gramdrive-source-tdjson/src/auth.rs` (new): `AuthMachine` +
  typed vocabulary (`AuthState`/`AuthInput`/`AuthRejection`/`RetryAdvice`/
  `AuthError`), 13 unit tests.
- `crates/gramdrive-source-tdjson/tests/auth_flow.rs` (new): 8 scripted
  integration flows over the real `TdRuntime` + `MockTdJson`.
- `src/lib.rs` (module registration, re-exports, docs), crate `README.md`
  (auth section + module table), `LOGBOOK.md` entry, board notes.

## Verification (reproduced, not trusted)

- `make check` suite `all`: **8/8 green** — toolchain, format, lint
  (clippy `-D warnings`), test (full workspace), architecture,
  supply-chain, traceability, scripts. Provenance `.temp/acceptance/local-all`;
  review logs `.temp/TASK-260715-51n6jb/review-make-check-01.log`,
  `review-cargo-test-01.log`.
- `cargo test -p gramdrive-source-tdjson`: 38 lib unit + 8 `auth_flow` +
  25 other integration tests, all green, deterministic (GUARD timeouts are
  upper bounds only, no timing assertions).

## AC coverage (verified against actual test bodies)

| AC scenario | Test |
|---|---|
| Success (phone→code→password) | `phone_code_password_success_path` — exact wire-conversation order asserted, `RuntimeStats::default()` (nothing absorbed) |
| Success (QR) | `qr_confirmation_path_reaches_ready_through_password` — link rotation re-entry + password gate |
| Retries | `wrong_code_classifies_and_the_retry_succeeds`, `invalid_password_classifies_and_the_retry_succeeds` — state stays where TDLib says, retry needs no special path |
| Expired code | `expired_code_recovers_through_resend` — classify → `RequestNewCode` → resend → fresh `CodeInfo` → success |
| Network loss mid-flow | `network_loss_mid_flow_is_transient_and_the_same_input_retries` — connection update ignored (no transition), 500 → `Network`/`RetrySameInput`, same input retried |
| Cancellation | `cancellation_mid_flow_closes_the_client_and_further_input_is_typed` — in-flight request abandoned, close → Closing → Closed, further input typed `InvalidInput`, runtime ends client + stream |
| Unknown/new states fail safe | `unknown_states_fail_safe_and_cancel_still_escapes` + unit `unknown_states_become_typed_unsupported_never_a_panic` — typed `Unsupported{td_type}`, typed `UnsupportedState` error, cancel escapes, no panic |

Unit side additionally pins: startup answer emitted on `waitTdlibParameters`
with no `@extra` minted; non-auth updates ignored; payload extraction with
degraded-report defaults; malformed updates → typed error, state unchanged;
resume mid-flow from any reported state; full input-validity state table;
full classification + advice tables; flood-second parsing (both message
shapes, overflow-safe); Debug redaction of code/password inputs.

## Architecture fit

- Sans-IO machine over the existing runtime seam; no new dependencies; no
  threads/timing in product code — matches the crate's deterministic-test
  discipline and the DEC-003 provider-neutral direction (typed vocabulary,
  no TDLib JSON outward; raw `@type` only as diagnostic detail).
- SEC-020 holds: `Secret::expose()` is `pub(crate)`; code/password
  plaintext leaves only via the crate-private `AuthInput::request` onto the
  wire; redaction asserted under `Debug`. Phone number deliberately
  unwrapped with recorded rationale.
- Ownership boundaries sound: Cancel = local `close` (logout/wipe stays in
  TASK-260715-wjaux5 / SEC-004); configuration answer machine-owned.
- Docs consistent: README auth section, `lib.rs` module list, logbook
  decisions all match the code as written.

## Non-blocking observations (no rework required)

1. **QR→phone fallback absent.** `WaitQrConfirmation` accepts only
   `Cancel`; real TDLib also permits `setAuthenticationPhoneNumber` there
   to fall back from QR to phone sign-in. V1 escape is cancel + restart —
   consistent with the documented v1 scope. First-class fallback = a small
   future extension of the input-validity table.
2. **Classification arm order.** Named contractual identifiers match
   before the 429/flood and 500 arms — correct priority; noted so a future
   arm addition preserves it.
3. **`trailing_integer` safety.** Byte-indexed scan; `str::get` guards
   char boundaries (returns `None`, never panics); huge-number overflow
   tested. No panic path.
4. **Implementer logbook timestamp** (0134) is a few minutes ahead of wall
   clock at review time — cosmetic only.

## Gate provenance

- `.temp/acceptance/local-all` (make check run)
- `.temp/TASK-260715-51n6jb/review-make-check-01.log`
- `.temp/TASK-260715-51n6jb/review-cargo-test-01.log`
