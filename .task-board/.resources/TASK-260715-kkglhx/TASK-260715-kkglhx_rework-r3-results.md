# TASK-260715-kkglhx — Round-3 test-only rework results (flaky-suite determinism)

**Status: ready for review.** All quality gates green. Product code (Fixes 1 & 2
from review-01) and server design untouched. Every change is in test code.

## What review-02 asked for (the two named fixes)

1. **`busyBound` expectation** — accept EITHER `HydrationFailure(.busy)` OR a
   transport/socket error, because the server refuses `busy` **before** reading
   the request, so the refusal races the client's request write.
2. **`SO_NOSIGPIPE` on the raw test socket** at `HydrationServerTests.swift:315`
   area, "so SIGPIPE cannot kill the process." Reviewer flagged 315 as the
   "plausible/**only-consistent** site; the implementer should confirm while
   fixing."

## What I changed

### Fix 1 — `busyBound` (`HydrationServerTests.swift`)
`busyBound` now catches `HydrationFailure` (expects `.busy`) **and** `is
UnixSocketError` / `is HydrationTransportError` (the write/read race outcomes).
Any other error still fails the test. This is the exact race Fix 2 (raw
`UnixSocketError` → `serverUnreachable`) maps downstream, so the test now
accepts the same reality the product already handles.

### Fix 2 — SIGPIPE, in two parts
- **2a (the literal ask):** `SO_NOSIGPIPE` is now set on the raw client socket
  in `malformedRequestRefused` **before** its `write` (`HydrationServerTests.swift`).
- **2b (necessary addition, evidence-backed):** a process-wide
  `signal(SIGPIPE, SIG_IGN)`, installed idempotently via each socket-writing
  suite's `init()` in **both** test modules (`GramDriveAgentCoreTests`,
  `GramDriveSupportTests`).

  **Why 2a alone is insufficient (measured):** I audited *every* `write`/`send`
  on a socket in both product and test trees — all are already preceded by
  `SO_NOSIGPIPE` on their fd (client `HydrationClient.swift:119`, server
  `HydrationServer.swift:205`, health `HealthChannel.swift:107`, the scripted
  test server `HydrationChannelTests.swift:63`, and now the raw fd at 315). Yet
  after fixing 315 the process **still** died by SIGPIPE (signal 13) under load
  — the crash intersection was the client/server *refuse-races-write* path
  (`busyBound`, `lifecycleServesHydration`), not line 315. The residual window
  is a tight scheduling race that even `lldb` hid entirely (0 catches in 40
  instrumented iterations). The reviewer's own hedge ("confirm while fixing")
  anticipated this. `SIG_IGN` is the correct realization of the stated goal
  ("SIGPIPE cannot kill the process"): the write still returns `EPIPE` (same
  errno `SO_NOSIGPIPE` yields), so all error handling — including the Fix-1
  catch — is unchanged. **Production is untouched and unaffected**: it keeps its
  per-socket `SO_NOSIGPIPE`; this guard only hardens the shared test *process*
  that hosts client+server+raw sockets together under 240-way parallelism.

### Additional same-class hardening found while proving determinism under load
The task is "flaky suite determinism." Stress-looping surfaced two more latent
flakes of the **same parallel-runner-timing class**, both test-only, both
product-correct:

- **`hydrationRegistersInTheLedger`** (`HydrationServerTests.swift`): the final
  `registry.pendingCount == 0` now polls (bounded, ≤2.5s) for the ledger drain.
  Root cause: the server calls `registry.end(ticket)`
  (`HydrationServer.swift:299`) **after** `writeEvent(.done)`
  (`:285`) — the event that unblocks the client — so `hydrate` can return a hair
  before cleanup lands. Product is correct; the test sampled an async counter
  synchronously.
- **`cancelWhileLive` + `finishRetires`** (`HydrationChannelTests.swift`):
  replaced the fd-number-reuse-racy `fcntl(closedFd, F_GETFD) == -1` assertions
  with **reuse-immune** proofs. `finishRetires` now uses a `socketpair` and
  proves `finish()` closed the descriptor via the peer reading EOF (never reads
  the freed fd number back — a parallel suite reuses it the instant it frees).
  `cancelWhileLive` keeps its cancel→peer-EOF proof and swaps the racy check for
  the no-double-close observable. This aligns both tests with the suite's **own**
  doc-comment: "the reuse itself … cannot be forced portably … pinned by its
  observable state machine instead."

  Scope note: (c) and (d) exceed the two enumerated fixes but are exactly the
  flake class this task exists to kill, are test-only, and touch no product or
  server-design code. `HydrationConnection` (the accepted Fix-1 product code) is
  unchanged.

## Verification

| Run | Load | Result |
|-----|------|--------|
| `swift test` × 30 (deliverable command) | none | **30/30 pass**, 0 crash |
| `swift test` × 25 (earlier, post 1/2a/2b/c) | none | 25/25 pass |
| `swift test` × 40 | NCPU-1 busy loops (harshest) | **40/40 pass**, 0 crash |
| `make check` | — | **8/8 pass** (`.temp/acceptance/local-all`) |
| `swift build --build-tests` | — | clean (only pre-existing `Progress:@retroactive Sendable` warning) |

Total post-all-fix corpus: **95 full-suite runs, 0 failures, 0 SIGPIPE crashes**
— including the exact CPU-starvation load that previously reproduced every
flake (SIGPIPE ~8%, `busyBound` ~20%, plus the registry and fd-reuse races).

Before the fixes, under the same NCPU-2/NCPU-1 load: SIGPIPE process-kills,
`busyBound` uncaught socket error, `hydrationRegistersInTheLedger` (~3%), and
`cancelWhileLive`/`finishRetires` fd-reuse (~3%) all reproduced. All eliminated.

Logs: `.temp/TASK-260715-kkglhx/{plain-final,plain2,load4,load3,load2,runs}/`.
Provenance for make check: `.temp/acceptance/local-all`.
