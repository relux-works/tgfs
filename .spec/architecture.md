# Native Drive Architecture

Date: 2026-07-15

## Decision

The primary product is a Dropbox/Google Drive-like native drive whose data source is Telegram. Finder, Explorer, iOS Files, Android's system picker, and a Linux mount are the primary interfaces. A web client and rich Telegram-style UI are deferred.

The maximum-reuse architecture is:

- a shared Rust drive core;
- UniFFI-generated Swift/Kotlin bindings;
- thin native provider adapters and native settings/status UI;
- a provider-neutral source trait with interchangeable local TDLib and remote-service implementations.

For v1, prefer **local-first on desktop and Android**. This removes the infrastructure requirement and permits a Dropbox-grade on-demand experience on those platforms. Preserve a remote backend for iOS cold hydration, self-hosting, and a possible hosted tier. Do not force the final engine-placement decision into the shared core.

## Architecture

```text
                         +--------------------------+
                         | Shared Rust drive core   |
                         | tree, cache, hydration,  |
                         | renderers, cursors       |
                         +------------+-------------+
                                      |
                          provider-neutral Source
                           /                     \
              +-----------+---------+   +--------+-----------+
              | LocalTdlibSource    |   | RemoteDriveSource  |
              | tdjson, per-device  |   | HTTPS to gotd svc  |
              +-----------+---------+   +--------+-----------+
                          |                      |
                    Telegram cloud       canonical DB/blobs

Rust core -> Swift -> iOS/macOS File Provider
Rust core -> Kotlin -> Android DocumentsProvider
Rust core ----------> Windows CfAPI host
Rust core ----------> Linux FUSE host
```

The abstraction should be provider-oriented, not a raw MTProto wrapper. A conceptual interface is:

```rust
trait DriveSource {
    async fn root(&self) -> Item;
    async fn list_children(&self, parent: ItemId, page: PageToken) -> Page<Item>;
    async fn changes(&self, cursor: ChangeCursor) -> ChangePage;
    async fn fetch(&self, item: ItemId, range: ByteRange, sink: ContentSink) -> Version;
    async fn thumbnail(&self, item: ItemId, size: ThumbnailSize) -> Bytes;
}
```

Both implementations must pass the same conformance suite. Stable IDs, versions, paging, cancellation, partial reads, retry classification, and change-cursor behavior are part of the contract.

## Shared Rust core

The core owns the code worth sharing:

- stable item identities and the virtual `chat -> folder -> files` tree;
- Telegram-order snapshot and filename-prefix policy;
- filename sanitization across all target filesystems;
- deterministic NDJSON and Markdown rendering;
- local SQLite state and schema migrations;
- hydration, pin/offline state, resumable range downloads, hashing, quota, and eviction;
- durable change cursors and restart recovery;
- bounded retries, cancellation, progress, and error normalization;
- provider-independent tests and fake source implementation.

UniFFI is the default bridge for Swift and Kotlin. Windows and Linux link the Rust crates directly. The target is to share most domain, sync, cache, and transfer logic; an exact reuse percentage should be measured after the provider spikes rather than promised in advance.

Do not put OS placeholder objects, extension lifecycle, secure-store APIs, or UI types into the shared core.

## Native layers

### macOS

Use `NSFileProviderReplicatedExtension` in Swift. Keep the extension thin even though no comparable documented 20 MB macOS cap was found. Run TDLib in a companion app/agent and share durable metadata through an App Group container or a narrowly scoped native service. Reuse the Apple provider-support Swift package with iOS where the APIs genuinely align.

### iOS / iPadOS

The File Provider extension has a 20 MB process memory limit according to an accepted Apple engineer response. Do not link or initialize TDLib inside it. TDLib remains in the containing app; the extension can enumerate shared metadata and serve already materialized content from the App Group container.

Local-first has one explicit degraded case: opening a dataless Telegram file from Files while the main app is not available to fetch it. V1 must choose one honest behavior:

1. ask the user to open the app and retry;
2. use `RemoteDriveSource` for iOS;
3. later build a purpose-specific, measured MTProto fetch path small enough for the extension.

Option 3 is research, not a committed solution. grammers currently lacks the CDN behavior required to select it as that fetcher.

### Android

Implement `DocumentsProvider` in Kotlin and call the Rust core through UniFFI/JNI. `LocalTdlibSource` may run in the application's process. Document IDs must be stable and independent of path. V1 does not advertise write/delete capabilities.

### Windows

Build the CfAPI sync-provider host in Rust and call the shared core directly. Use `windows`/`windows-sys`; treat `cloud-filter-rs` as reference or fork material, not a dependency assumed to be production-ready. Keep the WinUI shell limited to account, status, cache/offline settings, diagnostics, and logout.

### Linux

Implement a thin `fuser::Filesystem` adapter over the same Rust interface. The FUSE code translates inode and file-handle operations only; Telegram and cache policy remain in the core.

## Source implementations

### `LocalTdlibSource`

- One Telegram authorization per device, matching Telegram's multi-device model.
- TDLib supplies ordered updates, local database, file state, retry behavior, and on-demand downloads.
- Dataless placeholders are the default; content is downloaded only when opened or pinned.
- No Takeout interface: initial metadata/history is crawled through normal TDLib methods. Huge accounts will backfill more slowly, so desktop may later offer an optional Takeout importer.
- Secret chats remain device-specific and outside a global archive promise.

### `RemoteDriveSource`

- A gotd/td service holds the Telegram session and canonical database/blob store.
- Clients use revocable product tokens and HTTP range requests instead of Telegram keys.
- This solves iOS cold hydration and provides one canonical archive and Takeout backfill.
- It requires an always-on local/NAS/self-hosted instance or a hosted service with substantial credential-custody and privacy obligations.

Use OpenAPI or protobuf to generate the Go/Rust contract. The remote service must expose the same normalized item/change semantics as `LocalTdlibSource`, not leak gotd-specific types through the drive core.

## Practical constraints verified

- Apple states that iOS File Provider extension processes have a 20 MB memory limit. TDLib issue discussions report meaningful per-client native memory and little room for further tuning once its databases are enabled. Those figures come from server workloads and are not a formal universal TDLib floor, but the engineering margin is inadequate; TDLib-in-extension is excluded from the design.
- Rust plus native Swift/Kotlin shells is a production pattern used by Element X/matrix-rust-sdk and Firefox. Dropbox's Nucleus independently validates Rust for a correctness-heavy sync engine. Dropbox's separate mobile post warns against sharing platform/UI code through a custom cross-platform layer; the lesson is to share the engine, not the UI.
- Go/gomobile is not selected for the extension-adjacent core. Its runtime and GC create unnecessary pressure under iOS extension limits. This does not disqualify Go for the optional remote service.
- TDLib remains a valid embedded/headless engine on desktop and Android. grammers is not a drop-in replacement today because the surveyed implementation lacks CDN download support and a TDLib-equivalent persistent client store.
- Rust CfAPI is feasible through Microsoft-generated `windows-rs` bindings, but the product should budget ownership of the higher-level callback/state layer.

## Storage and process rules

- Provider item identity never depends on chat title, numeric ordering prefix, or path.
- Generated Markdown/NDJSON files are versioned views; structured records remain canonical.
- On Apple, assume the app and extension are separate processes. Use short SQLite transactions and durable queues; never rely on shared in-memory state.
- Read-only v1: rename, move, edit, create, and Telegram delete are not filesystem operations.
- Cache eviction and Telegram deletion are distinct explicit actions.
- Native providers must support cancellation, partial reads, restarts, expired credentials, unavailable source, and file-version changes during hydration.

## Revised implementation order

1. Define the Rust item/change/source contracts and build a deterministic fake source plus conformance tests.
2. Implement `LocalTdlibSource` through TDLib's C/JSON interface.
3. Build a macOS File Provider vertical slice: one account, one chat, placeholders, one ranged hydration, restart recovery.
4. Build the Windows Rust CfAPI host against the same fixtures and source.
5. Add Android DocumentsProvider.
6. Build iOS with shared metadata/materialized-file support and explicitly choose the cold-hydration UX.
7. Add Linux FUSE.
8. Add `RemoteDriveSource` plus the gotd service when required for iOS, self-hosting, or SaaS.

## Primary references

- Apple File Provider memory answer: https://developer.apple.com/forums/thread/739839
- Apple replicated provider: https://developer.apple.com/documentation/fileprovider/replicated-file-provider-extension
- TDLib C/JSON interface: https://core.telegram.org/tdlib/docs/td__json__client_8h.html
- TDLib memory discussions: https://github.com/tdlib/td/issues/2516 and https://github.com/tdlib/td/issues/2807
- UniFFI: https://mozilla.github.io/uniffi-rs/
- Matrix Rust SDK bindings: https://github.com/matrix-org/matrix-rust-sdk/tree/main/bindings
- Element X architecture: https://element.io/blog/element-x-ignition/
- Dropbox Nucleus: https://dropbox.tech/infrastructure/rewriting-the-heart-of-our-sync-engine
- Dropbox mobile code-sharing retrospective: https://dropbox.tech/tech/2019/08/the-not-so-hidden-cost-of-sharing-code-between-ios-and-android/
- Android DocumentsProvider: https://developer.android.com/guide/topics/providers/create-document-provider
- Windows Cloud Files API: https://learn.microsoft.com/en-us/windows/win32/cfapi/cloud-files-api-portal
- Rust for Windows: https://github.com/microsoft/windows-rs
- Rust FUSE: https://docs.rs/fuser/latest/fuser/

Detailed evidence ledger: `.research/260715-shared-core-feasibility.md`.
