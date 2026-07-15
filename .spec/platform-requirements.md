# Platform Requirements

Status: planning baseline
Last updated: 2026-07-15

## Shared rules

- **PLAT-001 (V1):** Native provider adapters translate OS callbacks into the shared Rust contract and contain no Telegram-specific business rules.
- **PLAT-002 (V1):** Each adapter supports enumeration, stable identity, metadata refresh, content fetch, cancellation, restart recovery, and native offline/pin intent where available.
- **PLAT-003 (V1):** Native UI is limited initially to authorization/account, provider registration, status, cache/offline settings, diagnostics, and removal.
- **PLAT-004 (V1):** Platform packaging includes code signing, upgrades, migrations, and uninstallation cleanup in acceptance scope.

## macOS

- **PLAT-MAC-001 (V1):** Use `NSFileProviderReplicatedExtension` and expose the drive in Finder through the supported File Provider domain APIs.
- **PLAT-MAC-002 (V1):** Run TDLib in a containing app or companion agent; keep the extension thin and communicate through durable shared state and a narrow native service where needed.
- **PLAT-MAC-003 (V1):** Use an App Group/shared container for provider metadata and materialized handoff; assume app and extension are separate processes.
- **PLAT-MAC-004 (V1):** Handle working-set enumeration, change signaling, dataless items, partial/range fetch where supported, offline pinning, and domain removal.
- **PLAT-MAC-005 (V1):** Provide a menu-bar/settings shell and launch/background lifecycle appropriate for the TDLib host.

## iOS / iPadOS

- **PLAT-IOS-001 (V1):** Use `NSFileProviderReplicatedExtension` and a containing Swift application.
- **PLAT-IOS-002 (V1):** Never initialize TDLib inside the File Provider extension; treat the currently verified 20 MB extension memory limit as a hard design budget and target materially below it.
- **PLAT-IOS-003 (V1):** Share only durable metadata, queues, and already materialized content through the App Group container.
- **PLAT-IOS-004 (Decision gate):** Before iOS release, choose and test cold hydration when the containing app is unavailable: explicit open-app UX, remote source, or separately proven minimal fetch path.
- **PLAT-IOS-005 (V1):** Authorization and two-step verification occur only in the containing application.
- **PLAT-IOS-006 (V1):** Extension memory, cancellation, background URL/session behavior, and jetsam recovery are measured on all supported iOS versions/devices.

## Windows

- **PLAT-WIN-001 (V1):** Use the Cloud Files API/CfAPI with a registered sync root, placeholders, hydration callbacks, in-sync state, and pin intent.
- **PLAT-WIN-002 (V1):** Implement the provider host in Rust and call CfAPI through `windows`/`windows-sys`; own or audit the higher-level state wrapper.
- **PLAT-WIN-003 (V1):** Keep opaque file identity within CfAPI limits and map it to stable shared `ItemId` values.
- **PLAT-WIN-004 (V1):** Handle callback cancellation, range hydration, restart/disconnect, rename/delete attempts in read-only mode, Explorer refresh, and long paths.
- **PLAT-WIN-005 (V1):** Package registration, upgrades, startup/background behavior, signing, and clean sync-root removal.

## Android

- **PLAT-AND-001 (V1):** Implement a Kotlin `DocumentsProvider` and expose one root per configured account or a clearly versioned combined-root policy.
- **PLAT-AND-002 (V1):** Use stable document IDs and capability flags; omit write/delete/move/rename flags in V1.
- **PLAT-AND-003 (V1):** Use UniFFI/JNI to call the Rust core and permit TDLib in the application/provider process subject to measured lifecycle/memory behavior.
- **PLAT-AND-004 (V1):** Support streaming opens, thumbnails where useful, process recreation, cancellation, multiple simultaneous readers, and persisted provider state.
- **PLAT-AND-005 (V1):** Use Android Keystore-backed credential protection and comply with background execution restrictions.

## Linux

- **PLAT-LNX-001 (V1):** Implement a Rust `fuser::Filesystem` adapter with a long-running user service/daemon.
- **PLAT-LNX-002 (V1):** Map stable item IDs to durable/reconstructible inode identities without treating paths as identity.
- **PLAT-LNX-003 (V1):** Implement lookup, readdir, getattr, open/read/release, statfs, xattr policy, cancellation/interruption, and read-only errors.
- **PLAT-LNX-004 (V1):** Provide packaging/service integration for at least one reference distribution and document FUSE prerequisites.

## Cross-platform conformance

- **PLAT-020 (V1):** The same fixture tree and content versions are tested through every adapter.
- **PLAT-021 (V1):** Cross-platform filename fixtures cover Unicode normalization, case collisions, Windows reserved names, trailing characters, separators, and length limits.
- **PLAT-022 (V1):** Provider-specific behavior that cannot be normalized is documented as an explicit capability, not hidden in business logic.
