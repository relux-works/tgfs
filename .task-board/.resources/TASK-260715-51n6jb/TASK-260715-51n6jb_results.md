# TASK-260715-51n6jb — Authorization state machine: implementation notes

## What landed

New module `crates/gramdrive-source-tdjson/src/auth.rs` (~700 lines with unit
tests) plus scripted integration suite
`crates/gramdrive-source-tdjson/tests/auth_flow.rs` (8 end-to-end scenarios
over the real `TdRuntime` + `MockTdJson`). Re-exported from the crate root;
crate README and `lib.rs` docs updated.

## Design

**Sans-IO deterministic machine.** `AuthMachine` holds the typed `AuthState`
and the account's `TdlibConfig`; it performs no I/O, owns no threads, no
timing. The caller (the coming `DriveSource` adapter or a native shell via
FFI) pumps the client's `UpdateStream` into `on_update`, submits the requests
each `AuthStep` returns, and turns user actions into requests via `on_input`.
This is what makes every acceptance scenario a deterministic scripted test.

**TDLib's reported state is the single source of truth.** Inputs never move
the typed state — only `updateAuthorizationState` events do. A rejected
code/password leaves the flow exactly where TDLib says it is, so retries need
no special path, and an interrupted sign-in resumes from whatever state TDLib
reports first (proved by tests).

**Core-facing typed vocabulary (DEC-003 direction), FFI-ready:**
- `AuthState`: Starting / Configuring / WaitPhoneNumber / WaitCode(CodeInfo)
  / WaitQrConfirmation{link} / WaitPassword(PasswordInfo) / Ready /
  LoggingOut / Closing / Closed / Unsupported{td_type}
- `AuthInput`: SubmitPhoneNumber / RequestQrCode / SubmitCode / ResendCode /
  SubmitPassword / Cancel
- `AuthRejection` (classified from `TdError`): InvalidPhoneNumber /
  PhoneNumberBanned / InvalidCode / ExpiredCode / InvalidPassword /
  RateLimited{retry_after_secs} / Network / SessionEnded / Other
- `RetryAdvice`: RetrySameInput / ReviseInput / RequestNewCode /
  WaitThenRetry / Abort — the typed form of the story's "explicit UX/error
  mapping"
- `AuthError` (caller-side): InvalidInput / UnsupportedState /
  MalformedUpdate

No TDLib JSON crosses outward; the raw `@type` appears only as diagnostic
detail in `Unsupported`/errors (same rule the runtime's `TdError` follows).

## Decisions and boundaries

1. **Unsupported-state policy (product scope).** V1 signs in an existing
   personal account: phone → code → optional 2FA password, and QR → optional
   password. Email-gated sign-in (`authorizationStateWaitEmailAddress` /
   `…WaitEmailCode`), registration (`…WaitRegistration`), and any future
   TDLib state become the typed `AuthState::Unsupported`: every input except
   `Cancel` fails with typed `AuthError::UnsupportedState`, nothing panics,
   cancel still closes the client. If email-gated accounts need first-class
   support later, that is a new task extending the state/input enums.
2. **Cancel is local close, not logout.** `Cancel` → `{"@type":"close"}`;
   the runtime already treats `authorizationStateClosed` as end-of-client.
   Server-side logout/revocation and the storage wipe belong to account
   removal (TASK-260715-wjaux5, SEC-004).
3. **Configuration answer is machine-owned.** On `waitTdlibParameters` the
   machine emits `TdlibConfig::startup_requests()` itself — plumbing the
   user never sees, and the reason the machine holds the config.
4. **Rejection classification matches Telegram's contractual identifiers**
   (`PHONE_CODE_INVALID`, `PHONE_CODE_EXPIRED`, `PASSWORD_HASH_INVALID`,
   …). TDLib code 500 is read as transient network for this flow (advice:
   retry same input) — a misread internal error costs one failed retry, not
   a wrong transition. Flood-wait seconds parsed from both message shapes
   ("Too Many Requests: retry after N", "FLOOD_WAIT_N").
5. **Secrets.** Login code and 2FA password ride in the existing `Secret`
   (redacted Debug; plaintext leaves only via the crate-private request
   builder onto the wire — SEC-020). Phone number deliberately not wrapped:
   TDLib echoes it in clear in `code_info` and the UI must render it.
6. **Resend is not gated on `next_type`/timeout.** The machine stays
   permissive; an early resend gets TDLib's error, classified. Over-gating
   on a possibly-degraded update field would add a hard failure mode.

## Test coverage (all deterministic, mock-driven; no timing assertions)

Unit (13, in `src/auth.rs`): startup answer + no `@extra` minting; non-auth
updates ignored; code/password/QR payload extraction incl. degraded reports;
unknown states → typed Unsupported (email/registration/future); malformed
updates → typed error, state unchanged; resume mid-flow; full input-validity
state table; unsupported-state input errors; full classification table;
advice table; flood-message parsing (incl. overflow-safe huge number); Debug
redaction of code/password inputs.

Integration (8, `tests/auth_flow.rs`): phone→code→password success with
exact wire-conversation assertion and zero absorbed-event stats; QR path
with link rotation and password gate; wrong code → classified → retry
succeeds; expired code → resend → fresh code info → success; invalid
password → retry succeeds; network loss mid-flow (connection update ignored,
500 → Network/RetrySameInput, same input retried); cancellation mid-flow
(in-flight request abandoned, close → Closing → Closed, further input typed
error, runtime ends client); unknown state fail-safe with cancel escape.

## Verification

- `cargo test -p gramdrive-source-tdjson` — 38 unit + 29 integration, all
  green.
- `make check` (suite `all`, run-id local-all) — 8/8 green: toolchain,
  format, lint (clippy -D warnings), test (workspace), architecture,
  supply-chain, traceability, scripts. Provenance:
  `.temp/acceptance/local-all`.
