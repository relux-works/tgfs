# TASK-260715-kkglhx — Rework results (review-01 surgical fixes)

Scope: two REQUIRED fixes from `TASK-260715-kkglhx_review-01.md`. Architecture
accepted; no redesign. Both fixes plus tests below; all gates green.

## Fix 1 — fd-reuse race in client cancellation (`HydrationClient.swift`)

`HydrationConnection` never cleared the adopted descriptor, so a `cancel()`
racing the exchange's unwind (legal until `withTaskCancellationHandler`
returns, i.e. after `close(fd)`) could `shutdown()` a descriptor NUMBER the OS
had already handed to an unrelated fd (one of the other concurrent fetches,
scratch I/O, XPC).

- The connection now OWNS the descriptor once adopted. New `finish()` closes
  it exactly once under the lock and retires it (`descriptor = nil`,
  `closed = true`); `cancel()` gained the `!closed` guard, so a post-finish
  cancel is a no-op. This mirrors the server's accepted `Connection.finish()`
  closed-flag pattern.
- `exchange()` now: `socket` → `fcntl` → `adopt` (guard: on refusal closes the
  fd itself, since the connection never took ownership) → `defer { connection.finish() }`.
  During the exchange `cancel()` still shuts the live descriptor down (the wire
  cancel / blocked-read unblock) — preserved.
- `HydrationConnection` made `internal` (was `private`) so the guard is
  unit-testable.

## Fix 2 — raw `UnixSocketError` escaped NFR-030 mapping (`ContentFetcher.swift`)

`performFetch` mapped `HydrationFailure` and `HydrationTransportError`, but the
client also throws raw `UnixSocketError.failed` (socket() fd exhaustion, EPIPE
on send to a dead agent, EINTR/ECONNRESET on read, sandbox-EPERM/other connect
errors) and `.pathUnrepresentable`. These escaped uncaught as non-provider
errors instead of `serverUnreachable`.

- Added a `catch let socketError as UnixSocketError` → `NSFileProviderError(.serverUnreachable)`,
  scoped to the wire only. The deliberate DriveError storage passthrough from
  `liveFile` is untouched: the initial `liveFile` is outside the `do`, and the
  restart-branch `liveFile` throws from within a sibling `catch`, so neither is
  folded into this catch. `CancellationError` still propagates to the caller's
  cancel handler.

## Tests

- `ContentFetcherTests`: parametrized `rawSocketFaultMapsToServerUnreachable`
  (EPIPE, ECONNRESET, EINTR, EMFILE, EPERM, ENOTCONN) + `unrepresentableSocketPath…`
  pin Fix 2. New `enqueueSocketFailure` helper on `ScriptedHydration`.
- `HydrationConnectionTests` (new suite): `adoptRefusesADescriptorOnceCancelled`,
  `cancelWhileLiveShutsTheDescriptorDown` (peer reads EOF; finish closes once),
  `finishRetiresTheDescriptorSoALaterCancelIsANoOp`. Deterministic state-machine
  coverage of Fix 1.

### Testability note (important)
The fd-reuse race itself cannot be forced portably: Swift Testing runs suites in
parallel, and fd numbers are process-global, so a "reuse the freed number with a
live victim socket" test flakes (a first attempt using `socketpair` + a
`victim.contains(sacrifice)` assertion failed ~1-in-N). Replaced with
deterministic state-machine assertions on `HydrationConnection`. The guard is
also structurally the mirror of the already-reviewed server pattern.

## Gates
- `swift test`: 244/244 in 46 suites, stable across repeated + looped runs.
- `make check`: 8/8 (provenance `.temp/acceptance/local-all`).
- `swift build`: clean (only the pre-existing `Progress: @retroactive Sendable` warning).
