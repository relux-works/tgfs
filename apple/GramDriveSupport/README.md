# GramDriveSupport

The Apple provider-support Swift package (`.spec/architecture.md`): the
app, the companion agent, and the File Provider extension all link this
package so every GramDrive process resolves the same shared container,
derives the same file layout, and follows the same multi-process rules.
Owned by TASK-260715-gnsa2s (STORY-260715-33oacu, macos-engine-host).

Policy lives in the Rust core (`crates/gramdrive-ffi/src/shared_state.rs`
— WAL-only open, role rights, snapshot reads, migration-on-open,
coordinator-only corruption recovery); this package binds it to the Apple
host: App Group resolution, `URL`-shaped entry points, and the Darwin
change doorbell.

## Surface

| Type | Owns |
|---|---|
| `AppGroup` | The container identity (`262RZ595FP.com.reluxworks.gramdrive`, the team-prefixed entitlement form v1 ships — DEC-019/POL-7) and the data-root rule: `Library/Application Support/GramDrive` inside the container. Everything below the data root comes from the core's `sharedStateLayout`, so Swift and Rust can never disagree about paths |
| `SharedState` | Role-based open: `openInAppGroupContainer(role:)` for product processes, `open(dataRoot:role:)` for tests and tools. The engine host opens as `.coordinator`, the File Provider extension and UI surfaces as `.provider` |
| `ChangeSignal` | The cross-process change doorbell: a payload-free Darwin notification (App-Group-prefixed name, which sandboxed processes may post and observe). Writers `post()` after commit; observers treat a ring as *check now* — compare `SharedStateStore.dataVersion()` and re-read only on change. Advisory, never authoritative: the database is the truth. Finder signaling (`signalEnumerator`) is a separate channel owned by the File Provider domain work |

The `gramdrive-shared-state-smoke` executable is the harness process for
`.scripts/smoke/run_shared_state_smoke.py` (reader / watcher / doorbell
modes); it is not a product target.

## The companion agent (TASK-260715-1yx9ly)

The package also ships the macOS background agent: `GramDriveAgentCore`
(the lifecycle library the agent binary and the app shell both link) and
`gramdrive-agent` (the launch-agent executable, PLAT-MAC-002/-005). The
lifecycle is `launching → recovering → running → draining → stopped`:

| Type | Owns |
|---|---|
| `AgentLifecycle` | The coordinator process's lifecycle: single-instance guard first, then shared state as `.coordinator` with corruption recovery (quarantine + one retry), the `DriveCore` handle, the health endpoint, power observation. `shutdown(reason:)` drains before tearing anything down |
| `AgentRuntimeLayout` | Host-owned runtime paths beside the core's layout: `agent/agent.lock`, `agent/health.sock`, `agent/settings.json` under the same data root |
| `SingleInstanceLock` | One coordinator per container, via `flock` — the kernel releases a crashed agent's lock, so recovery needs no stale-lock cleanup |
| `TransferRegistry` | The in-flight transfer ledger and the drain: admission refusal once draining, a grace period, then cancellation through each operation's FFI `CancellationToken`. Process-local by design; durable transfer state is the engine's, which is why a crash cannot duplicate work |
| `AgentHealthServer` / `AgentHealthClient` | The bounded local IPC: one endpoint, no request vocabulary — connect, receive one `AgentHealthSnapshot` (NFR-032 shape; unwired fields are honest `nil`s), EOF. A UNIX socket in the container rather than an XPC mach service so the channel stays provable in tests and the smoke; paths beyond `sun_path` are handled |
| `AgentSettings` / `AgentSettingsStore` | Durable host preferences — launch-at-login, managed-cache quota and global Archive Mode (POL-2) — as atomic JSON under `agent/`; never in the engine's database (DEC-006). The app writes it, the agent reads it; decoding tolerates a missing key as its default, so a shell or agent update never orphans the document |
| `LaunchAtLoginPolicy` / `SMAppServiceAgentLoginItem` | Idempotent reconciliation of the user's preference with `SMAppService` registration. Called by the *app* (the launchd plist lives in the app bundle — platform constraint); the agent honors the preference by reporting it and never self-registering |
| `PowerEventSource` / `WorkspacePowerEventSource` | Sleep/wake observation; wake re-probes `dataVersion` because a doorbell rung during sleep is lost |

Shutdown is signal-driven (`SIGTERM`/`SIGINT` → drain → exit 0), which is
exactly what launchd delivers on unload, logout, and update; the agent
carries its own version and the core's contract version in health so the
shell can detect a stale agent after an update. Accounts live inside the
shared database (`AccountScope`), so one agent hosts every account of the
container — the multiple-accounts path never means multiple coordinators.

## The companion shell (TASK-260715-13pxnu)

The package also ships the menu-bar companion app (PLAT-MAC-005):
`GramDriveCompanion` (the view-model + seam library) and
`gramdrive-companion` (the SwiftUI `MenuBarExtra` executable). It hosts no
engine and performs **no Telegram operation itself** — it is a presentation
layer that renders the agent's status and drives it through one seam.

| Type | Owns |
|---|---|
| `CompanionBackend` | The single boundary between shell and agent — the AC's "UI drives the agent via IPC; no Telegram ops from filesystem callbacks" is this seam existing and the shell holding nothing else. `LiveCompanionBackend` wires the reads that exist today (health over the bounded socket, settings over the durable document); commands (authorization, repair, removal) report `ControlChannelUnavailable.notWired` until the agent grows a control channel, because the health socket is read-only by design and the FFI exposes no such surface yet |
| `AuthorizationViewModel` / `CompanionAuthState` … | The sign-in flow (phone → code → optional 2FA, or QR → optional 2FA), a faithful mirror of the core's `gramdrive-source-tdjson::auth` vocabulary (`AuthState`/`AuthInput`/`AuthRejection`/`RetryAdvice`). The state stream from the `AuthorizationSession` seam is the single source of truth for the screen, exactly as TDLib's reported state is for the core machine |
| `CompanionStatusViewModel` | Account, File Provider domain, and diagnostics status — pure projections of the last `AgentHealthSnapshot`, with honest "not reported yet" where the engine has not wired a field |
| `CompanionSettingsViewModel` | The managed-cache quota, global Archive Mode with the POL-2 pre-enable check (projected disk usage + low-disk warning), and launch-at-login reconciled through `LaunchAtLoginPolicy` |
| `RepairViewModel` / `AccountRemovalViewModel` | The repair pass and the irreversible account removal (SEC-004), each gated and rendered by the shell but executed in the agent; removal is behind a typed, echo-the-label confirmation |
| `InMemoryCompanionBackend` / `ScriptedAuthorizationSession` | Preview- and test-support seam implementations (mirroring `gramdrive-testkit`) that make every screen state reachable deterministically |

Every screen state is a deterministic view-model tested via scripted fakes
(`Tests/GramDriveCompanionTests`); the SwiftUI views switch over those
states so every one is reachable. The command-channel wiring lands with the
control-channel story (this story blocks `STORY-260715-2pe5sa`).

## The File Provider domain layer (TASK-260715-3s44pc)

The package also ships `GramDriveFileProvider` (PLAT-MAC-001): the stable
per-account domain identity, the idempotent domain reconciler, and the
thin `NSFileProviderReplicatedExtension` skeleton. Thin is the design, not
a stage (DEC-006): the extension process never hosts TDLib or the engine —
this target's only dependencies are the support package and the core
bindings, and it opens shared state read-only as `.provider`.

| Type | Owns |
|---|---|
| `DomainIdentity` | The identity rule: a domain identifier is a pure function of the account's stable numeric identity (`account-<id>`) — never of display name, auth state, or namespace epoch — so the same account always maps to the same domain across restarts, reinstalls, and reauthorization. Naming follows POL-7: one account presents as exactly **GramDrive**; several disambiguate with the account's display name (collisions append the identity). Parsing back is strict by round-trip, so a foreign domain identifier can never alias a real account |
| `DomainReconciler` | The idempotent converge-toward-desired pass behind every acceptance path (first run, restart, duplicate install, reauthorization, multiple accounts): a pure `plan` (adds / renames / keeps / strays) diffed from the durable account rows, applied through the registrar seam. Registered domains no account explains are *reported* as strays, never touched — removal and repair are owned by TASK-260715-gnat2x, and the registrar seam has no remove operation at all, which makes "registration never destroys Finder state" structural |
| `DomainRegistrar` / `SystemDomainRegistrar` | The narrow seam to the platform registry, so reconciliation is fully testable against fakes. The live implementation wraps `NSFileProviderManager` (`domains()` / upserting `add`); platform constraint: it must run inside the app that embeds the extension — the companion shell calls it at launch — and is proven live by the signing/packaging task (TASK-260715-1dk9ik) |
| `DomainStartupReconcile` | The add-only launch pass the app runs at startup (SYNC-070): resolve container → open shared state as `.provider` → reconcile → report an outcome (`skipped`/`reconciled`/`failed`) for the log. Never throws and never blocks startup; registration is durable in the system, so this pass is what "recovers after app/provider restart" means. It only ever adds and renames — never removes — so a partial or empty canonical read at launch can never tear a domain down. Stray cleanup is the user-triggered `DomainRepair` (see below), never the launch path |
| `GramDriveFileProviderExtension` | The replicated extension: parse the domain identifier, open shared state, resolve the account and its root item identifier (`accountContext()`). Item resolution (TASK-260715-i3mp9x) and enumeration (TASK-260715-rhcnhc) sit on that context; content fetch is still its own story (STORY-260715-14n7wp) and answers `CocoaError.featureUnsupported` for a resolvable domain, `NSFileProviderError(.noSuchItem)` for one that maps to no configured account |
| `GramDriveFileProviderItem` | The read-only `NSFileProviderItem` over one core `ItemMetadata` (TASK-260715-i3mp9x; DEC-007/SYNC-060): pure and total, so every provider-visible attribute is testable from hand-built fixtures. No mutating capability for any kind; POL-4 restricted/unavailable content advertises no read either |
| `GramDriveEnumerator` | Paged listing and change enumeration (TASK-260715-rhcnhc; SYNC-003, NFR-021): keyset pages over `children` in stable id order (no duplicate/missing children across pages, memory bounded by page size), the working set as the domain-wide change feed (items list empty — macOS pulls only working-set *changes*), and journal-anchored `enumerateChanges` where a POL-3 tombstone reports as a deletion. Every callback answers synchronously from short snapshot reads |
| `EnumerationPageCursor` / `EnumerationSyncAnchor` | The durable cursor codecs, both versioned and self-describing: a page binds its container so a replayed page can never anchor one directory's listing inside another (foreign → `.pageExpired`, the explicit restart); an anchor binds account, namespace epoch, journal instance, and sequence, and anything foreign or overtaken answers `.syncAnchorExpired` (the explicit full resync) — recovery is always explicit, never a silently wrong diff |
| `ChangeSignalRelay` | The doorbell→Finder bridge (PLAT-MAC-004 change signaling): observes GramDrive's Darwin doorbell, probes `dataVersion()`, and calls `signalEnumerator(for: .workingSet)` — through the `WorkingSetSignaling` seam — only when the probe moved. Hosted by the domain-registering process (the extension only runs while requests are in flight); probe-on-start covers rings missed while not running |

The desired domain set derives from `SharedStateStore.accounts()` /
`account(accountId:)` — the contract-0.3.0 account snapshot reads
(identity, display name, auth state, namespace epoch, and the account
root's item identifier; never secret material). The cross-process happy
path is proven by the shared-state smoke's `domains` step: a separate
provider process maps the Rust-seeded account to its domain and the real
extension type resolves that domain back to the same root item the seeder
reported.

## Domain removal, repair, and cleanup (TASK-260715-gnat2x)

Registration only ever adds; taking a domain *away* — on logout, on
local-only removal, or when a registration is stale/corrupt — is a
distinct capability with its own seam, so "registration never destroys
Finder state" stays structural. The removal seam (`DomainRemover`) is
handed only to the explicit removal and repair flows, never to the
reconciler.

| Type | Owns |
|---|---|
| `DomainDataDisposition` | The preserve-or-delete-per-user-choice decision (PLAT-MAC-004; SEC-004), narrowed to what a read-only V1 can offer: `deleteLocalData` (the platform's `removeAll` — the trace-free wipe) or `preserveDownloads` (`preserveDownloadedUserData` — the system moves the user's downloaded files aside and returns where they now live). The third platform mode, `preserveDirtyUserData`, is deliberately absent: read-only means there is never dirty user data to keep |
| `DomainRemover` / `SystemDomainRemover` | The narrow *removal* seam — deliberately separate from `DomainRegistrar`, which has no remove operation at all. Removes one registered domain with a disposition and returns the preserved-downloads location. The live implementation wraps `NSFileProviderManager.remove(_:mode:)`; same platform constraint as the registrar (runs in the app that embeds the extension; proven live by the signing/packaging task, not unit tests) |
| `DomainRemoval` | Targeted, idempotent per-account domain removal — the provider-registration step of the SEC-004 cleanup sequence (logout and the on-disk wipe are the engine's). Reads the registered set, removes the one matching the account's stable identifier if present, and reports a no-op when it is already gone, so re-running a completed removal touches nothing. Interruption-safe by ordering: the engine drops the canonical account row first, then this removes the domain; a crash between the two leaves a stray that repair cleans up — never a domain re-registered for an account that is gone |
| `DomainRepair` | The **user-triggered** repair (SYNC-071): `DomainReconciler` plus the one thing it refuses to do — resolve strays. Re-registers every account's lost domain (recovering its Finder state under the stable identifier, so rebuild loses no data) and removes strays no account row explains (default `preserveDownloads`, so even orphan cleanup keeps files). Because it can *remove* domains it never runs at launch — only from the explicit "Repair File Provider Domains" action. Adds/renames run before stray removal, so an interrupted pass re-runs into a completed one from either side; a completed one re-runs into a settled no-op. It also guards the *total-teardown* case (`TotalTeardownPolicy`): an empty desired set makes every registered domain a stray, so unless the teardown is explicitly confirmed it withholds them all rather than trust a possibly-spurious empty read. `run(dataRoot:)` / `run()` are the never-throwing app entry points, mirroring `DomainStartupReconcile` |

The launch path runs the add-only `DomainStartupReconcile.run()`
(SYNC-070) — never `DomainRepair` — so startup only ever adds and renames
and can never tear a domain down. `DomainRepair.run()` is wired behind the
explicit **"Repair File Provider Domains"** command in the companion app
(SYNC-071): a menu action, off the main thread, that re-registers lost
domains and cleans genuine orphans. Repair fails closed — if the canonical
store cannot be read it removes nothing — and never removes a domain that
maps to a live account. It also refuses a *total teardown*: an empty
account set (no accounts configured) is a normal, non-throwing answer from
the canonical store, indistinguishable from a spurious-empty read (an App
Group id change across an upgrade, an externally reset state dir), so it
does **not** wipe every registered domain on that signal — it withholds
them unless the teardown is explicitly confirmed. Cleared strays keep their
downloads (default `preserveDownloads`). The genuine last-account logout
removes its domain through the targeted `DomainRemoval` flow, driven by the
logout itself — not by repair inferring teardown from emptiness.

### Uninstall cleanup (PLAT-004 / SEC-004)

macOS does not unregister a provider's File Provider domains when its
containing app is deleted from `/Applications`: the domains, and any
materialized files under them, persist and can show as a broken Finder
root. Clean uninstall is therefore **remove the account(s) first, then
delete the app** — the in-app removal unregisters each domain (disposing
of local data per the user's choice) so nothing is left behind. If the app
was deleted without removing accounts first, reinstalling and running a
repair (or a removal per account) reconciles the leftover registrations;
`NSFileProviderManager` domain management can only run from the app that
embeds the extension, so there is no way to clean these up once the app is
gone. This sequence is the macOS half of the SEC-004 documented cleanup
(credentials, session/database files, provider registrations, partial
transfers, cached content) and is validated on device by the
signing/packaging task.

## The core dependency is a built artifact

`Package.swift` resolves `GramDriveCore` (XCFramework + generated
bindings) by path from `.temp/packaging/GramDriveCore`, which `make
package` stages — built artifacts are never committed
(`.scripts/packaging/README.md`). Building here without the artifact fails
at dependency resolution:

```sh
make package                                  # stage the core artifact (repo root)
cd apple/GramDriveSupport
swift build                                   # macOS 14+ arm64 (POL-5)
swift test                                    # Swift Testing suite
```

`GRAMDRIVE_CORE_PACKAGE=<path>` overrides the artifact location when
consuming a staged or released artifact elsewhere.

## Verification

- `swift test` — Swift Testing suites: App Group identity and layout
  derivation, role-based open against a substitute container, provider
  quarantine refusal, coordinator corruption recovery through the
  bindings, doorbell post/observe/cancel round-trips, and the agent
  suites (lock contention, launch-policy matrix, health channel including
  beyond-`sun_path` sockets, registry drain semantics, and the full
  lifecycle against real shared state and a real hosted probe transfer);
  and the companion-shell suites — every authorization screen state and
  flow (phone/code/2FA/QR, rejection→advice, invalid-input refusal,
  control-channel-unavailable), status/diagnostics projection, settings
  round-trip with the Archive Mode preflight and launch-at-login reconcile,
  repair/removal outcomes, and `AgentSettings` forward/backward decode
  compatibility; and the File Provider domain suites — the identity rule
  (stability, strict round-trip parsing, POL-7 naming with multi-account
  disambiguation), the reconcile plan and its idempotence (repeat pass,
  duplicate install, reauthorization, rename, strays untouched), the
  startup pass over real shared state, and the extension skeleton's typed
  refusals over a substitute container; and the removal/repair suites — the
  disposition→platform-mode mapping, targeted removal idempotence
  (already-gone is a no-op, delete preserves nothing, preserve surfaces the
  kept-files location), and repair's re-registration after a lost domain,
  stray cleanup with downloads preserved, idempotence, recovery when a
  pass is interrupted mid-add or mid-stray-removal, and the total-teardown
  guard (an empty account set against still-registered domains is withheld
  by default, cleared only under an explicitly-confirmed teardown, while a
  genuine orphan alongside a live account is still cleaned).
- `make smoke-agent-lifecycle` (repo root) — the agent as real processes:
  startup with health over the socket, single-instance refusal of a
  second agent, SIGTERM drain (hosted transfer cancelled through its
  token, exit 0, endpoint removed), and instant successor startup after
  SIGKILL (see `.scripts/smoke/run_agent_lifecycle_smoke.py`).
- `make smoke-shared-state` (repo root) — the real multi-process proof:
  a Rust coordinator process seeds, two concurrent Swift provider
  processes must read byte-identical item metadata through the packaged
  artifact, a provider process runs the File Provider domain chain
  (seeded account → stable domain → the extension type resolving it back
  to the seeder's root item), and a watcher process must observe the
  doorbell plus the data-version probe across a foreign commit. The
  Rust-side stress and SIGKILL crash tests live in
  `crates/gramdrive-state/tests/multiprocess.rs`.

## Substitute containers

Product processes resolve the real App Group container (which requires
GramDrive signing and entitlements). Tests and the smoke pass a substitute
container directory through the same `AppGroup.dataRootURL(containerURL:)`
rule — the layout code path is identical; only the root differs.
