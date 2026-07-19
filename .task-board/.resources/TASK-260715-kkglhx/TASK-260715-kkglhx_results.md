# TASK-260715-kkglhx — File Provider content fetch (ready for review)

Implemented `fetchContents` for the macOS File Provider extension as a
bridge to the companion agent over a new bounded hydration IPC channel,
with atomic materialization, version-race handling, bounded concurrency,
progress, cancellation, POL-4 enforcement, and full error mapping.
`swift test` 239/239 (+45 new), `make check` 8/8, `make
smoke-agent-lifecycle` and `make smoke-shared-state` PASSED.

## Architecture fact that shaped the design

The FFI contract (0.4.0) exports **no fetch/transfer surface** — by DEC-006
the boundary has snapshot reads only and deliberately no writes — and the
agent does not host the engine yet. So "bridge fetch requests to the shared
transfer engine" is, on this side of the boundary, IPC to the coordinator
process (PLAT-MAC-002's narrow native service): the extension asks the
agent; the agent (once the engine bridge exists) drives the Rust
fetch/transfer engine, which stages verified content into the shared
container cache. **Bytes never cross the socket** — the terminal event
names the staged file; the extension clones it out.

The engine-backed `ContentHydrating` implementation requires an FFI fetch
export (shared-rust-core scope). **No board task exists for that export
yet** — recommend creating one (FFI fetch surface + agent hydrator
composition); until it lands, `AgentConfiguration.hydrator` is `nil` and
the endpoint is *not offered at all*, so a fetching extension truthfully
sees `serverUnreachable`. No production stub fakes bytes.

## What landed

### `GramDriveSupport` (shared by extension and agent)
- **`HydrationContract.swift`** — the wire contract: socket path rule
  (`<root>/agent/hydration.sock`), protocol version, size caps,
  `HydrationRequest` (accountId, itemId, pinned contentVersion),
  `HydrationProgress`, `HydratedContent` (stagedPath, contentVersion,
  byteCount), `HydrationFailure` with stable categories (DriveError's
  taxonomy + `versionConflict`, `restricted`, `draining`, `busy`; unknown
  categories decode leniently to `internal`), newline-framed JSON events.
- **`HydrationClient.swift`** — `HydrationRequesting` protocol (the
  extension's testable seam) + `AgentHydrationClient`: blocking socket I/O
  on a utility queue bridged into async; cancellation = `shutdown(2)` on
  the descriptor (unblocks reads, and the close *is* the wire's cancel);
  idle timeout between events; `agentUnavailable` / `timedOut` /
  `protocolViolation` transport errors.
- **`UnixSocketAddress.swift`** — moved from `GramDriveAgentCore` (now
  public): both sides of every agent IPC channel need it.

### `GramDriveAgentCore`
- **`HydrationServer.swift`** — per connection: capped request-line read,
  protocol-version check, **store-backed admission** (unknown
  account/item/tombstone → `notFound`; directory → `internal`; POL-4
  non-fetchable → `restricted`; stale version pin → `versionConflict` — all
  refused before any engine work), `TransferRegistry.begin` (a draining
  agent refuses with `draining`, and shutdown drains hydrations through the
  standard grace-then-cancel machinery), then the `ContentHydrating` seam
  with a fresh FFI `CancellationToken`. An EOF monitor turns client
  disconnect into the token's cancel. Concurrency bound (8) refuses excess
  with `busy`. `DriveError` thrown by an engine-backed hydrator maps by
  category onto the wire.
- **`AgentLifecycle`** — optional `hydrator` in `AgentConfiguration`;
  endpoint started after health, torn down on shutdown after the drain;
  `AgentStartError.hydrationEndpoint` case; `AgentRuntimeLayout.hydrationSocket`.

### `GramDriveFileProvider`
- **`ContentFetcher.swift`** — the whole `fetchContents` behavior
  (callback plumbing minus untestable `NSFileProviderRequest`):
  - refusals before any IPC: directory → `featureUnsupported`;
    POL-4 restricted/unavailable → `fileReadNoPermission` (zero agent
    contact, test-pinned);
  - bounded FIFO gate (4 < server's 8), waiters individually cancellable;
  - version pin + **one restart** on `versionConflict` against a fresh
    snapshot, only when the store actually moved (unmoved →
    `serverUnreachable`, never a spin); staged-version mismatch in `done`
    treated as the same conflict; availability re-checked across restarts;
  - a *requested* stale `NSFileProviderItemVersion` is served as current
    (Telegram keeps no history; the returned item carries the version the
    bytes belong to — the provider API's documented fallback);
  - **atomic materialization (PRD-043)**: APFS-clone staged → provider
    scratch (no user-space byte loop — memory bounded by construction),
    verify byte count, delete on mismatch; URL exists for the system only
    complete and verified; staged cache file never moved/modified;
  - byte-granular `Progress` (file-operation kind, cancellable), cancel
    while queued / mid-hydration / via `invalidate()` → `userCancelled`;
  - error mapping (NFR-030): `notFound→noSuchItem`,
    `restricted→fileReadNoPermission`, `authRequired→notAuthenticated`,
    `cancelled→userCancelled`, transient (`versionConflict`, `rateLimited`,
    `sourceUnavailable`, `draining`, `busy`, transport errors) →
    `serverUnreachable`, broken machinery (`storage`, `integrity`,
    `internal`) → `cannotSynchronize`; out-of-space during materialization
    → `insufficientQuota`.
- **`FileProviderExtension.swift`** — real `fetchContents` +
  `fetchContentsCore` testable seam; default wiring
  (`AgentHydrationClient` over the data root,
  `NSFileProviderManager.temporaryDirectoryURL()` scratch with a
  tmp-dir fallback outside a registered domain); `invalidate()` cancels
  in-flight fetches; dead `itemError` helper removed.

## Verification

- **`swift test` — 239/239 in 45 suites** (baseline 194; +45):
  - `HydrationContractTests`/`HydrationClientTests` (SupportTests): framing
    round-trips, lenient category decode, client vs a hand-scripted raw
    socket server — request verbatim, ordered progress, terminal failure,
    early-close / oversized / undecodable / idle-timeout violations,
    cancellation observed by the peer as EOF, absent + dead sockets.
  - `HydrationServerTests`/`AgentLifecycleHydrationTests` (AgentCoreTests):
    real-client round-trip with progress, admission refusal before the
    hydrator, drain refusal, failure + `DriveError` category crossing,
    client-disconnect → FFI token cancel, registry registration, busy
    bound, malformed/version-mismatched request refusal; lifecycle wiring
    (endpoint + real-state admission up with a hydrator, absent without,
    gone after shutdown).
  - `ContentFetcherTests` (FileProviderTests): verified atomic
    materialization, partial-content-never-published (+ scratch cleanup),
    vanished-staged-file as transient, POL-4/directory/tombstone/unknown
    refusals with zero agent contact, stale-requested-version-serves-
    current, restart-once / unmoved-store / second-conflict /
    staged-mismatch / restricted-across-restart races, full mapping table,
    cancellation (queued, mid-hydration, `cancelAll`), concurrency
    high-water == gate width with all fetches completing, progress
    completion; extension-level noSuchItem + storage-passthrough via
    `fetchContentsCore`.
- **`make check` — 8/8** (`.temp/acceptance/local-all`; Rust untouched).
- **`make smoke-agent-lifecycle` — PASSED** (lifecycle changes exercised as
  real processes).
- **`make smoke-shared-state` — PASSED** (extension init changes exercised
  in the real provider-process chain).
- Logs: `.temp/TASK-260715-kkglhx/*.log`.

## Range/partial semantics — the "where applicable" call

On macOS the replicated-extension fetch surface is whole-file
`fetchContents`; byte-range fetch at the provider boundary is the *optional*
`NSFileProviderPartialContentFetching` adoption. This task deliberately does
**not** adopt it yet: the wire contract would need a range vocabulary and
the staged-file model per-range staging, and with no engine-backed hydrator
existing, the only implementation would be scripted-test-only — proving
nothing about the real engine's 512 KiB range grid (SYNC-041, which lives
in the Rust engine and is already proven there). Without the adoption the
system always requests full contents — correct, just less optimal for
partial reads. The contract grows a range field additively when the engine
bridge lands; recommend folding partial-fetch adoption into that follow-up.
Ranged behavior *inside* a fetch is already streaming and memory-bounded:
progress is byte-granular and materialization is an APFS clone, never a
byte loop.

## Findings / follow-ups for the coordinator

1. **Missing board task**: FFI fetch export + agent engine-hydrator
   composition (the production `ContentHydrating`). This task's channel and
   seam are ready for it; the pinning task (TASK-260715-3s461k) will also
   need it, and `NSFileProviderPartialContentFetching` adoption belongs
   there too (see above).
2. Swift 6 strict-concurrency patterns recorded in LOGBOOK 2026-07-19 1826
   (NSLock-in-async, `Progress` retroactive Sendable, non-Sendable capture
   in `Task`).
3. Nothing committed (workflow: review first). Working tree left for
   review.

## Files

- `apple/GramDriveSupport/Sources/GramDriveSupport/{HydrationContract,HydrationClient,UnixSocketAddress}.swift`
- `apple/GramDriveSupport/Sources/GramDriveAgentCore/{HydrationServer,AgentLifecycle,AgentRuntimeLayout,HealthChannel}.swift`
- `apple/GramDriveSupport/Sources/GramDriveFileProvider/{ContentFetcher,FileProviderExtension}.swift`
- `apple/GramDriveSupport/Tests/GramDriveSupportTests/HydrationChannelTests.swift` (new)
- `apple/GramDriveSupport/Tests/GramDriveAgentCoreTests/{HydrationServerTests(new),HealthChannelTests}.swift`
- `apple/GramDriveSupport/Tests/GramDriveFileProviderTests/{ContentFetchTestSupport(new),ContentFetcherTests(new),FileProviderExtensionTests}.swift`
- `apple/GramDriveSupport/README.md`, `LOGBOOK.md`
