# Review-03: TASK-260715-kkglhx — File Provider content fetch (round-3 rework)

**Verdict: ACCEPTED → done.** The round-3 test-only rework closes the flaky-suite
determinism defect from review-02. All quality gates green under independent
re-run, including the harshest load that previously reproduced the flake. Product
code (Fixes 1 & 2, accepted in review-02) is untouched.

## Independently verified by the reviewer

- **`swift test` × 20 under NCPU-1 load** (15 busy CPU spinners on a 16-core box —
  the exact CPU-starvation condition the implementer used to reproduce the flake):
  **20/20 green, 244 tests / 46 suites each, 0 FAIL, 0 SIGPIPE crash.**
  Logs: `.temp/TASK-260715-kkglhx/review-r3/run-*.log`, `loop-summary.txt`.
  The pre-fix flake ran ~28% (busyBound ~20% + SIGPIPE ~8%); seeing 0/20 if it
  were still live is ≈0.72²⁰ ≈ 0.16% — decisive.
- **`make check`: 8/8 passed** — toolchain, format, lint, test (19.2s),
  architecture, supply-chain, traceability, scripts.
  Log: `.temp/TASK-260715-kkglhx/review-r3/make-check.log`.
- **`swift build --build-tests`: clean.**
- Read line by line: both changed test files
  (`HydrationServerTests.swift`, `HydrationChannelTests.swift`) plus the product
  code they exercise (`HydrationServer.swift` accept/serve/refuse path,
  `HydrationClient.swift`, `ContentFetcher.swift`, `AgentMain.swift` signal setup).

## The two named fixes — ACCEPTED

1. **`busyBound`** now catches `HydrationFailure(.busy)` **and** `is UnixSocketError`
   **and** `is HydrationTransportError`; any other error still fails the test
   (no catch-all → propagates). This matches the accepted refuse-before-read
   server design: the server admits before reading, so a `busy` refusal can close
   the socket mid-write and surface a raw transport fault. Fix 2 already maps that
   fault to `serverUnreachable` downstream, so the test now accepts the exact
   reality production handles. Not masking — the `.busy` structured path is still
   asserted when it wins the race.
2. **`SO_NOSIGPIPE` at `malformedRequestRefused`** (HydrationServerTests.swift:357)
   is set on the raw test fd **before** its `write`. Correct; matches the literal ask.

## Scope beyond the two fixes — ACCEPTED (all test-only, product-benign)

- **Process-wide `signal(SIGPIPE, SIG_IGN)`** in both socket-writing test modules,
  installed idempotently from each suite's `init()`. **Verified it masks no
  production bug:** every production socket write targets an fd with `SO_NOSIGPIPE`
  set beforehand — client `HydrationClient.swift:117-120` (before `send` @128),
  server `HydrationServer.swift:203-206` (accepted `conn`, before `writeEvent`
  @390), health `HealthChannel.swift:107` (before `write` @115). The lock-file
  write (`SingleInstanceLock.swift:55`) and stderr writes are not peer sockets.
  The production agent ignores only SIGTERM/SIGINT (`AgentMain.swift:126`), i.e.
  it relies solely on per-socket `SO_NOSIGPIPE`, which is complete. The test guard
  only hardens the *shared test process* that co-hosts client+server+raw sockets
  under 240-way parallelism, and it converts any stray EPIPE into a **catchable**
  error rather than hiding one — strictly better for determinism.
- **`hydrationRegistersInTheLedger`** now polls (bounded ≤2.5s) for the ledger
  drain. Root cause confirmed in product: `registry.end(ticket)`
  (`HydrationServer.swift:299`) runs **after** `writeEvent(.done)` (:285) — the
  event that unblocks the client — so `hydrate` can return a hair before cleanup.
  Product is correct and the entry *does* drain (no leak); the test was sampling
  an async counter synchronously.
- **`cancelWhileLive` + `finishRetires`** replaced the fd-number-reuse-racy
  `fcntl(freedFd, …)` checks with **reuse-immune** `socketpair` peer-EOF proofs.
  A parallel suite could reuse a freed fd number instantly; observing the close
  through the peer avoids reading the freed number back. Genuine improvement,
  aligned with the suite's own doc-comment; the accepted Fix-1 product code
  (`HydrationConnection`) is unchanged.

## Product code untouched — CONFIRMED

mtimes: all four product files (`HydrationClient` 18:42, `HydrationContract`
18:07, `ContentFetcher` 18:43, `HydrationServer` 18:25) predate the round-3
window; the two changed test files are 19:27 and 19:33. Fix 1
(`HydrationConnection` ownership/finish/cancel guard) and Fix 2
(`catch let socketError as UnixSocketError → serverUnreachable`,
ContentFetcher.swift:208-220, DriveError sibling-catch passthrough preserved)
are both intact.

## DoD

- Implementation matches AC (large-file streaming/cancellation, stale-version
  restart/fail, bounded memory) — accepted across review-01/02; unchanged.
- Solution fits architecture (DEC-006, POL-4, PRD-043, NFR-030) — accepted.
- Tests green: `swift test` deterministic (20/20 under harshest load) + `make check`
  8/8. **Met.**

Route: `done`.
