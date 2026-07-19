# Review: TASK-260715-kkglhx — File Provider content fetch

**Verdict: CHANGES REQUESTED → to-dev.** Two defects in the new code, both
with small surgical fixes. Everything else is accepted as delivered.

## Verified independently by the reviewer

- `swift test`: **239/239 in 45 suites passed** (re-run by reviewer).
- `make check`: **8/8 passed** (re-run by reviewer; provenance `.temp/acceptance/local-all`).
- Smoke logs confirmed (`.temp/TASK-260715-kkglhx/smoke-agent-01.log`,
  `smoke-shared-state-01.log`): both PASSED.
- All new/changed product source read line by line: HydrationContract,
  HydrationClient, HydrationServer, ContentFetcher, FileProviderExtension,
  AgentLifecycle, AgentRuntimeLayout, UnixSocketAddress move.
- Test harnesses inspected: server tests exercise the REAL client against the
  REAL server over a real unix socket; ContentFetcher tests use a scripted
  hydration seam plus real files for materialization. Not mock theater.

## Architecture / AC assessment

- DEC-006 respected: extension never touches TDLib/engine; bytes never cross
  the socket; staged-file handoff + APFS clone. Correct.
- POL-4 enforced on BOTH sides (extension refuses before any IPC —
  test-pinned zero agent contact; agent admission refuses `restricted`). Correct.
- PRD-043 atomic materialization: clone → byte-count verify → delete on
  mismatch; URL handed to the system only complete. Correct.
- Version races: pin + restart-once against a moved snapshot; unmoved store →
  transient fail (no spin); staged-version divergence treated as conflict;
  availability re-checked across restart. Matches AC "stale versions
  restart/fail safely".
- Memory bounded by construction: capped line buffers both sides, no
  user-space byte loops. Matches AC.
- Concurrency bounded: extension FIFO gate 4 < agent bound 8; busy refusal
  tested against the real server. Correct.
- Deferral of `NSFileProviderPartialContentFetching` and of the engine-backed
  hydrator (no FFI fetch export exists; `hydrator=nil` ⇒ endpoint not offered,
  extension truthfully sees `serverUnreachable`): defensible, honestly
  documented, no production stub fakes bytes. ACCEPTED as scoped-out; needs
  the follow-up board task the implementer already recommended (FFI fetch
  export + hydrator composition + partial-fetch adoption; TASK-260715-3s461k
  also depends on it).

## Finding 1 (REQUIRED FIX) — fd-reuse race in client cancellation

`HydrationClient.swift`, `HydrationConnection` + `exchange()`:
`exchange()` closes the socket via `defer { close(fd) }`, but the adopted
descriptor is NEVER cleared from `HydrationConnection`. `cancel()` can legally
fire until `withTaskCancellationHandler` returns — i.e. in the window after
`close(fd)` but before the continuation resumption propagates — and then calls
`shutdown()` on a stale fd NUMBER. If another thread (another of the 4
concurrent fetches, scratch-file I/O, or the extension's XPC machinery) has
reused that number, an unrelated descriptor gets shut down. Realistic trigger:
`invalidate()`/`cancelAll()` or a user cancel landing while a fetch is
completing.

Proof it's an oversight, not a tradeoff: the server side already guards
exactly this — `Connection.shutdownWire()` checks the `closed` flag under the
lock and `finish()` sets it before `close()`. The client lacks the symmetric
guard.

Fix: before `close(fd)` in `exchange()`, mark the connection closed/release
the descriptor under the connection's lock; `cancel()` must skip `shutdown()`
once closed. Mirror of the server-side pattern, ~10 lines.

## Finding 2 (REQUIRED FIX, same pass) — raw UnixSocketError escapes NFR-030 mapping

`ContentFetcher.performFetch` maps `HydrationFailure` and
`HydrationTransportError`, but `AgentHydrationClient` also throws raw
`UnixSocketError.failed` on several real paths: `socket()` failure (fd
exhaustion), `EPIPE` on send (agent died between accept and request-read),
`EINTR`/`ECONNRESET` on read, non-ENOENT/ECONNREFUSED `connect` errors (e.g.
sandbox EPERM). These propagate uncaught to the system as a non-provider
error instead of `serverUnreachable`. Fix: catch `UnixSocketError` (or add a
transport catch-all) → `NSFileProviderError(.serverUnreachable)` in
`performFetch`, WITHOUT disturbing the deliberate, test-pinned DriveError
storage passthrough from `liveFile`. Plus a test pinning the mapping.

## Not blocking (noted for the record)

- `HydrationServer.stop()` closes the listener fd immediately after
  `acceptSource?.cancel()` (no cancellation-handler close). Same pattern as
  the pre-existing HealthChannel — consistent with the accepted codebase
  idiom, so not a finding against this task.
- `requestedVersion` intentionally unused (Telegram keeps no history; current
  version served, returned item carries the bytes' version) — documented,
  test-pinned, accepted.

## Routing

`to-dev` for the two fixes above. Everything else should be left as is; after
the fixes, re-run `swift test` + `make check` and route back `to-review`.
