# TASK-260715-2ulon7 — Safe asynchronous tdjson wrapper: implementation notes

Status: implementation complete, handed to review. All gates green
(`make check`, 8/8), real-linkage smoke green against the staged artifact.

## What was built

New workspace crate **`gramdrive-source-tdjson`** (the name reserved by
`crates/README.md` for the local TDLib source) implementing the safe runtime
over TDLib's modern C JSON interface (`td_create_client_id`/`td_send`/
`td_receive`/`td_execute`):

| Piece | File | Notes |
|---|---|---|
| Trait seam | `src/api.rs` | `TdSendApi` (Send+Sync: create/send/execute) and `TdReceiveApi` (`receive(&mut self)`) — the one-receiver rule of `td_receive` is enforced by ownership, not discipline |
| Runtime | `src/runtime.rs` | `TdRuntime` (single receive-loop owner), `TdClient`, `PendingRequest` (bounded blocking wait + `Future` + cancellation), `UpdateStream`, `RuntimeConfig`, `RuntimeStats` |
| Errors | `src/error.rs` | `TdError`: typed conversion of `{"@type":"error"}` (code+message) plus runtime lifecycle failures (`ClientClosed`, `Shutdown`, `InvalidRequest`, `Protocol`) |
| Envelope | `src/envelope.rs` | Event classification: `@extra` → response (single point of error conversion), `@client_id` → update, else malformed (counted, never fatal) |
| Sync primitives | `src/slot.rs`, `src/queue.rs` | Hand-rolled oneshot (blocking timed wait + waker-based `Future` polling) and bounded queue whose `close()` can be driven by a third party — the property shutdown needs and std/tokio channels lack |
| Mock | `src/mock.rs` | Deterministic in-process tdjson double (scripted events, synchronous responder hook, sent-request record); always compiled — this is what lets every gate run without the artifact |
| Real FFI | `src/real.rs` | Compiled only under `cfg(real_tdjson)`; `RealTdJson::claim()` hands out the process's single receiver once (atomic claim) |

## Key decisions

1. **Env gate, not a cargo feature.** The lint/test gates run
   `--all-features`; a `real-tdjson` feature would drag TDLib linkage into
   exactly the runs that must stay artifact-free. Instead `build.rs` enables
   `cfg(real_tdjson)` + link flags + rpath only when
   `GRAMDRIVE_TDLIB_ARTIFACT_DIR` is set (`make tdjson-smoke`). Documented in
   the crate README and the feature-policy section of `crates/README.md`.
   Rpath (not `DYLD_LIBRARY_PATH`) because macOS SIP strips `DYLD_*` across
   the make → sh → cargo chain.
2. **No async runtime dependency.** `PendingRequest` is a `Future` via a
   hand-rolled waker-correct slot, and also offers `wait_timeout` (handle
   returned on timeout, request stays pending). The coming async
   `DriveSource` adapter can await it; tests block on it — no tokio in this
   crate.
3. **Backpressure over dropping.** Updates route into bounded per-client
   queues; a full queue blocks the receive loop (TDLib's update order is
   contractual, so mid-stream drops are not an option). Stated tradeoff:
   one slow consumer stalls the shared loop — fine for the v1 single-account
   shape. The block is released by shutdown (queue close), consumer drain,
   or consumer stream drop.
4. **Deadlock-free shutdown, drain-first.** `shutdown()` (also on drop):
   flag under lock → close all update queues (wakes a backpressured loop —
   this ordering is the deadlock fix) → join the loop, which drains every
   ready event with zero-timeout receives (pending requests whose answers
   already arrived resolve `Ok`) → fail the remainder with
   `TdError::Shutdown`. Buffered updates stay readable after shutdown.
5. **Close is destroy.** The modern tdjson interface has no destroy call;
   `authorizationStateClosed` ends a client: pending requests fail
   `ClientClosed`, the stream ends after delivering the closed update, new
   requests are rejected. The deprecated `td_json_client_*` interface stays
   link-proved (tdlib link smoke) but is never called — TDLib forbids mixing
   the interfaces.
6. **No C pointer outlives validity — by construction.** The trait seam
   traffics only in owned `String`s; the real impl copies every
   tdjson-returned C string before returning and keeps request `CString`s
   alive across the call. Miri cannot run FFI, so the full ownership
   justification is in `src/real.rs` module docs (per the AC's
   "careful ownership justification" alternative), and the identical runtime
   logic is exercised under the mock in every gate.
7. **Everything absorbed is counted.** `RuntimeStats` (discarded responses,
   dropped updates, unroutable updates, malformed events) — the observable
   the cancellation/shutdown tests assert on, later health data (NFR-030
   direction).

## Tests (33, all deterministic, no artifact needed)

- Unit: slot (3), queue (5), envelope (4) — including
  close-wakes-blocked-producer and buffered-items-survive-close.
- `tests/runtime_lifecycle.rs` (8): out-of-order correlation, typed error
  conversion, request validation, execute round-trips, closed update fails
  pending + ends stream + rejects new requests, repeated create/close cycles
  with zero absorbed events, per-client update order, wait_timeout handle
  return.
- `tests/runtime_cancellation.rs` (5): drop-cancel and explicit cancel with
  late-response discard (order-based sync via a probe round-trip, no
  sleeps), duplicate-response discard, async path via dependency-free
  `block_on`, cancellation not disturbing other pendings.
- `tests/runtime_shutdown.rs` (4): drain resolves ready responses while the
  rest fail `Shutdown`; shutdown under update backpressure with a watchdog
  (the deadlock test — assertions are timing-independent: gap-free
  continuation, closed tail); drop-shutdown; clean idle shutdown.
- `tests/runtime_updates.rs` (4): routing, order under capacity-1
  backpressure (5 updates through a 1-slot queue), disconnected-consumer
  counting, unroutable/malformed counting.
- `tests/real_tdjson_smoke.rs` (1, `cfg(real_tdjson)` only): the same
  runtime against the real `libtdjson.dylib` — execute round-trip, correlated
  `getOption version` (asserted against the minted request id), clean client
  close through `authorizationStateClosed`, runtime shutdown, single-owner
  claim assertion. **Ran green locally** via `make tdjson-smoke` (0.5 s).

## Gates & commands run

- `make check` — 8/8 ok (toolchain, format, lint `--all-features`, test
  `--all-features`, architecture, supply-chain, traceability, scripts).
- `make tdjson-smoke` — 1 test ok against the staged artifact.
- `GRAMDRIVE_TDLIB_ARTIFACT_DIR=… cargo clippy -p gramdrive-source-tdjson
  --all-targets -- -D warnings` — the gated `real` module linted clean too
  (the default gate run cannot see it; noted for reviewers).

## Files changed outside the new crate

- `Cargo.toml` — workspace dep `serde_json` (already in the graph via
  uniffi's build tree; build script already named in deny.toml).
- `.scripts/check_crate_architecture.py` + `crates/README.md` — policy row
  for the new crate (deps: model, source; ffi may link it at composition
  time), diagram, feature-policy note on the env gate (same commit, per the
  keep-in-sync rule).
- `Makefile` — `tdjson-smoke` target (+ .PHONY).
- `README.md` — tools-table rows for the TDLib artifact pipeline (was
  missing entirely — omission from TASK-260715-rxjkpi) and the wrapper
  smoke.
- `.scripts/tdlib/README.md`, `.scripts/tdlib/link-smoke/Cargo.toml` —
  "reserved crate" wording now points at the real crate and its env gate.

## Deliberately out of scope (owned by follow-up tasks of this story)

`DriveSource` adapter, TDLib parameters/database paths/keys
(TASK-260715-1hdnuy), authorization flow, `SourceError` normalization of
`TdError` (DEC-003 boundary conversion), flood-wait `retry_after` parsing.
