# Review-02: TASK-260715-kkglhx — File Provider content fetch (rework pass)

**Verdict: CHANGES REQUESTED → to-dev.** The two surgical fixes from
review-01 are **correct and accepted**. Blocking reason is a separate defect:
`swift test` is **not deterministically green** — a pre-existing flaky test on
the hydration wire fails ~28% of full-suite runs (recorded-issue + a rarer
process-killing SIGPIPE). The DoD requires "tests green / make check + swift
test green"; it is not met. Fix is test-only hardening, no redesign.

## Independently verified by the reviewer

- `make check`: **8/8 passed** (toolchain, format, lint, cargo test 19.3s,
  architecture, supply-chain, traceability, scripts). Provenance
  `.temp/acceptance/local-all`.
- `swift build`: clean (only the pre-existing `Progress: @retroactive
  Sendable` warning).
- `swift test`: **244/244 on a good run**, but re-run under load it is FLAKY
  (see the blocking finding). Logs: `.temp/TASK-260715-kkglhx/`.
- Read line by line: `HydrationClient.swift` (Fix 1), `ContentFetcher.swift`
  (Fix 2), `HydrationChannelTests.swift`, `ContentFetcherTests.swift`,
  `ContentFetchTestSupport.swift`, plus `HydrationServer.swift` accept/refuse
  path and `HydrationServerTests.swift`.

## Fix 1 (fd-reuse race) — ACCEPTED, correct

`HydrationConnection` now owns the descriptor. `finish()` sets `closed = true`
and nils `descriptor` under the lock *before* the single `close()`; `cancel()`
only `shutdown()`s while `!closed`. The lock makes the two critical sections
mutually exclusive, so: (a) if `cancel()` wins the lock it shuts down a still-
live fd, then `finish()` closes it — `shutdown` then `close`, same live number;
(b) if `finish()` wins, `cancel()` sees `closed` and skips `shutdown()`. A
post-close `cancel()` can therefore never `shutdown()` a reused number. This is
a faithful mirror of the server's accepted `Connection.finish()` pattern
(`HydrationServer.swift:438`). The `adopt()==false` early-cancel path closes
the fd itself and does not arm the `finish()` defer — no double close. Pinned
by 3 deterministic state-machine tests (`HydrationConnectionTests`), which pass
in isolation and under load. The testability note (the reuse itself can't be
forced portably) is honest and correct.

## Fix 2 (raw UnixSocketError → serverUnreachable) — ACCEPTED, correct

New `catch let socketError as UnixSocketError` maps to
`NSFileProviderError(.serverUnreachable)`, scoped to the wire. DriveError
storage passthrough is structurally intact: the initial `liveFile` is outside
the `do`, and the restart-branch `liveFile` throws from inside a *sibling*
`catch` (not caught by same-level clauses). `CancellationError` still
propagates. Pinned by parametrized `rawSocketFaultMapsToServerUnreachable`
(EPIPE, ECONNRESET, EINTR, EMFILE, EPERM, ENOTCONN) +
`unrepresentableSocketPathMapsToServerUnreachable`. Mapping table test intact.

## BLOCKING — `swift test` is flaky/crashy (pre-existing, test-only)

Measured over repeated full-suite runs: **~20% record an issue in
`busyBound`, ~8% die by SIGPIPE (signal 13)** — combined ~28% of runs fail.
In isolation `busyBound` is 0/20; the flake needs parallel-load timing.

Root cause (one class, two faces): the server refuses `busy`/`malformed`
**before reading the request** (`HydrationServer.swift:227-234` — `admit`
precedes `readRequest`). So `connection.refuse()` → `finish()` can close the
socket *while the client is still writing its request line*.

1. **`busyBound` (HydrationServerTests.swift:277, ~20%).** The client's
   `send()` gets EPIPE and throws `UnixSocketError.failed("write", 32)`. The
   test only `catch let failure as HydrationFailure`, so the socket error
   escapes uncaught → recorded issue. NOTE: this is *not* a product bug — it is
   a real, legitimate outcome of a racy busy path, and `ContentFetcher` now
   maps exactly this to `serverUnreachable` thanks to Fix 2. The flake is the
   over-narrow test expectation.
   - Fix: accept EITHER `HydrationFailure(.busy)` OR a transport/socket error.

2. **SIGPIPE process crash (~8%).** Kills the whole test process (all 244
   abort); no `.ips` is produced because SIGPIPE terminates by signal. The only
   unprotected raw-socket `write` in the tree is
   `HydrationServerTests.swift:315` (`malformedRequestRefused`) — a `write(fd,…)`
   on a test socket with no `SO_NOSIGPIPE`. Under load, if the peer closes
   first, that write raises SIGPIPE. (Plausible/only-consistent site; the
   implementer should confirm while fixing.)
   - Fix: set `SO_NOSIGPIPE` on any raw test socket before writing (macOS has
     no `MSG_NOSIGNAL`). Every production socket already sets it; only this test
     fd does not.

Provenance: **pre-existing** — the rework touched only `HydrationClient.swift`
and `ContentFetcher.swift`; the `send()` path and `SO_NOSIGPIPE` ordering are
unchanged, and the busy/malformed refusal race predates the rework. review-01
ran the suite and got lucky. The rework-results claim "swift test 244/244
stable across repeated + looped runs" is not reproducible.

## Routing

`to-dev` for the two test-hardening fixes above **only**. Do not touch the
accepted product code (Fixes 1 & 2) or the server design. After: re-run
`swift test` in a loop (≥20×, or `--repeat-until-failure`) to prove it green,
plus `make check`, then route back `to-review`.
