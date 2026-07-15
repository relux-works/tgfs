# Research: OS-level Filesystem / Cloud-Drive Integration Technologies

**Stream:** 03 — FS integration for "Telegram chats as folders" app (web + mobile + desktop)
**Date:** 2026-07-15
**Status:** verified against primary sources (Apple / Microsoft / Google docs, GitHub) as of mid-2026

---

## TL;DR — Recommended per-platform stack

| Platform | Production-grade approach (what Dropbox/GDrive/OneDrive use) | Language requirement | Notes |
|---|---|---|---|
| iOS / iPadOS | File Provider extension (`NSFileProviderExtension` classic, or `NSFileProviderReplicatedExtension` on iOS 16+) | Swift/ObjC (native appex) | Files app integration; ~20 MB extension memory limit is the main constraint |
| macOS | File Provider `NSFileProviderReplicatedExtension` (macOS 11+, effectively 12.1+) | Swift/ObjC (native appex) | What GDrive/OneDrive/Box/Dropbox ship today; App Store-compatible |
| Windows | Cloud Filter API (cfapi) sync engine | Any Win32-capable language (C++/C#/Rust) | Same API as OneDrive Files On-Demand; good Rust (`cloud-filter`) and C# (`cfapiSync`, Vanara) wrappers |
| Android | `DocumentsProvider` (Storage Access Framework) | Kotlin/Java component | Cheapest integration of all; appears in system Files/DocumentsUI automatically |
| Linux | FUSE (`go-fuse` / `fuser` / libfuse) or `rclone mount` | Any | onedriver is the best on-demand reference |
| Shortcut / MVP | `rclone serve webdav` / WebDAV or SMB server + OS mounts | Any | Works everywhere except iOS Files (no native WebDAV); Windows WebDAV client is deprecated & crippled — don't bet on it |

Most reusable open-source references: **Apple FruitBasket sample**, **nextcloud/desktop (macOS FP module) + nextcloud/nextcloudfileproviderkit**, **Microsoft CloudMirror sample + ho-229/cloud-filter-rs + styletronix/cfapiSync**, **Android StorageProvider sample pattern (create-document-provider guide)**, **jstaf/onedriver**, **tgdrive/teldrive + TheodoreKrypton/tgfs** (Telegram-specific, name collision with this project!).

---

## 1. iOS — File Provider framework

### API landscape

- The File Provider framework has two extension types:
  - `NSFileProviderExtension` — the **classic / nonreplicated** extension. Available iOS 8+, iPadOS, Mac Catalyst, visionOS 1.0. **Not available on macOS** and not formally deprecated (only some methods are). Apple explicitly says "Don't use the NSFileProviderExtension class in macOS." Source: Apple docs JSON for `NSFileProviderExtension` (https://developer.apple.com/documentation/fileprovider/nsfileproviderextension).
  - `NSFileProviderReplicatedExtension` — the **modern replicated** protocol where *the system* owns the on-disk replica. Availability: **iOS 16.0+, iPadOS 16.0+, macOS 11.0+, visionOS 1.0+**. Source: Apple docs (https://developer.apple.com/documentation/fileprovider/nsfileproviderreplicatedextension).

- Required surface of `NSFileProviderReplicatedExtension` (per Apple docs, same URL):
  - `init(domain:)`, `invalidate()`
  - `item(for:request:completionHandler:)` — metadata lookup
  - `fetchContents(for:version:request:completionHandler:)` — **on-demand download** of file content
  - `createItem(basedOn:fields:contents:options:request:completionHandler:)`
  - `modifyItem(...)`, `deleteItem(...)`
  - plus `NSFileProviderEnumerating` (enumerators for directory listing + change enumeration / sync anchor).
- The system decides what to materialize; extension only *informs the system of the file-system state to replicate* and serves content when asked. Optional protocols (partial/incremental content fetching) improve large-file behavior. Sources: Apple File Provider docs (https://developer.apple.com/documentation/fileprovider), Apriorit overview (https://www.apriorit.com/dev-blog/730-mac-how-to-work-with-the-file-provider-for-macos).

### Constraints (the painful parts)

- **Memory limit: ~20 MB** for File Provider extension processes on iOS (jetsam kill above that). Developers report OOM crashes when enumerating folders with a few thousand items or when uploading large files without streaming. You must stream uploads/downloads, never load whole files, and page enumerations. Sources: Apple Developer Forums thread "Upload file in File Provider Extension" (https://developer.apple.com/forums/thread/739839), extension memory discussion (https://developer.apple.com/forums/thread/73148).
- Extensions are short-lived, launched on demand by the system; no long-running background work inside the extension. Heavy sync logic belongs in the main app / via `NSFileProviderManager` signaling.
- Debugging extensions is notoriously awkward (attach-to-process, console logs).
- Files app is the only UI surface; your provider shows up in Files "Browse" sidebar (and in every `UIDocumentPicker`), which is exactly the Dropbox/GDrive-on-iOS experience.

### Open-source implementations on iOS

- **Nextcloud iOS** (`nextcloud/ios`) — ships a File Provider extension using the **classic** API: `final class FileProviderExtension: NSFileProviderExtension` (Swift). Source: https://github.com/nextcloud/ios/blob/master/File%20Provider%20Extension/FileProviderExtension.swift (verified via raw file fetch).
- **ownCloud iOS** (`owncloud/ios-app`) — classic `NSFileProviderExtension`, **Objective-C** (`ownCloud File Provider/FileProviderExtension.m`, implements `itemForIdentifier:error:`, `URLForItemWithPersistentIdentifier:` etc.). Source: https://github.com/owncloud/ios-app/blob/master/ownCloud%20File%20Provider/FileProviderExtension.m.
- Nextcloud's newer **apple-clients** work implements `NSFileProviderReplicatedExtension` + `NSFileProviderEnumerating` for both iOS and macOS with XPC app↔extension communication and a `MaterialisedEnumerationObserver` for local-cache cleanup. Source: DeepWiki nextcloud/apple-clients "File Provider System" (https://deepwiki.com/nextcloud/apple-clients/6.1-file-provider-system).
- Blog reference (Nextcloud dev who built the macOS FP client): Claudio Cambra, "Build your own cloud sync on iOS and macOS using Apple FileProvider APIs" (https://claudiocambra.com/posts/build-file-provider-sync/) — site returned 403 to fetcher; widely referenced as the best practitioner writeup.

### Verdict (iOS)

File Provider is the only way to appear inside the iOS Files app. For a mid-2026 project targeting iOS 16+ there is no reason to use the classic API; use `NSFileProviderReplicatedExtension` and share code with macOS. Read-only first (browse chats + download media on demand) fits the API naturally; the 20 MB budget forces a thin extension that talks to a local DB/daemon-in-app-group and streams Telegram downloads.

---

## 2. macOS — File Provider vs FUSE

### File Provider (replicated) — the mainstream path

- `NSFileProviderReplicatedExtension` is macOS 11+; the ecosystem migration really happened on **macOS 12.1+** when Apple deprecated kernel extensions for this use. **Box, Google Drive, Microsoft OneDrive migrated fully; Dropbox migrated later and still documents an opt-out.** Sources: TidBITS "Apple's File Provider Forces Mac Cloud Storage Changes" (https://tidbits.com/2023/03/10/apples-file-provider-forces-mac-cloud-storage-changes/), Google "Use Drive for desktop on macOS" (https://support.google.com/drive/answer/12178485) — "Drive for desktop uses File Provider on macOS 12.1 and up", Dropbox help (https://help.dropbox.com/installs/dropbox-for-macos-support, https://help.dropbox.com/installs/macos-support-for-expected-changes) — requires macOS 12.5+, migration optional with opt-out.
- Files live under `~/Library/CloudStorage/<Provider>`; provider appears in Finder sidebar under **Locations** with sync badges, online-only/offline pin context menus — all system-provided.
- Known user-facing gripes of the FP model (documented in the TidBITS piece and Dropbox/Google help): online-only files aren't Spotlight-indexed or Time-Machine-backed, no streaming (full download before open — a movie must fully hydrate before playback), files buried in `~/Library/CloudStorage`.
- Apple's official sample: **FruitBasket** — "Synchronizing files using file provider extensions": 6 targets, app + FP extension + local HTTP server backend with SQLite; demonstrates the full replicated sync loop, decorations, custom actions, for macOS **and** iOS. Sources: https://developer.apple.com/documentation/fileprovider/replicated_file_provider_extension/synchronizing_files_using_file_provider_extensions, WWDC21 "Sync files to the cloud with FileProvider on macOS" (https://developer.apple.com/videos/play/wwdc2021/10182/), mirror repo https://github.com/seanses/FileProviderTrial.
- **App Store:** File Provider extensions are the Apple-sanctioned mechanism and are Mac App Store compatible (they're regular appexes). Kernel extensions are effectively dead for this purpose.

### FUSE options

- **macFUSE** (https://macfuse.github.io/, https://osxfuse.github.io/): current version **5.3.3 (July 2026)**, macOS 12+. Historically kext-based (Apple-discouraged: reduced-security boot / recovery-mode approval on Apple Silicon). **New in macFUSE 5 on macOS 26: an FSKit backend — file systems run entirely in user space, no kext, no recovery-mode reboot.** Sources: macfuse.github.io (fetched), macFUSE wiki "FUSE Backends" (https://github.com/macfuse/macfuse/wiki/FUSE-Backends). License: custom macFUSE license; kext path is not App-Store-distributable.
- **FUSE-T** (https://www.fuse-t.org/, https://github.com/macos-fuse-t/fuse-t): kext-less FUSE that emulates FUSE over a **local NFSv4 / SMB server**, and now also a **native FSKit backend on macOS 26+**. Explicitly "can be embedded within an existing app bundle to be published in the App Store." Free for personal use, **commercial license required for embedding/shipping**. 
- **FSKit** (https://developer.apple.com/documentation/FSKit): Apple's user-space filesystem framework, introduced **macOS 15.4**. Successor to VFS/kext filesystems. Current limits: mount points essentially restricted to `/Volumes`, I/O performance below kext backend. Sample: https://github.com/KhaosT/FSKitSample. Sources: FSKit docs, HN discussions (https://news.ycombinator.com/item?id=43540157), macfuse FSKit backend wiki.
- FUSE advantages over File Provider: real filesystem semantics, streaming reads (start playing a video before full download), arbitrary mount points. File Provider advantages: zero install friction, App Store, Finder-native badges/pins, Apple owns the sync-state machinery. Source: macFUSE maintainer discussion "Future of kexts and File Provider" (https://groups.google.com/g/osxfuse-group/c/vY9w1d9N-bQ) — "macFUSE functionality cannot be moved to the File Provider API… File Providers don't support streaming."

### Verdict (macOS)

Ship File Provider (same Swift code as iOS via a shared package, à la `nextcloudfileproviderkit`). Consider FSKit/FUSE-T only if streaming playback of large Telegram videos without full hydration becomes a hard requirement.

---

## 3. Windows — Cloud Filter API (cfapi)

### The platform

- **Cloud Files API** = Cloud Filter API (Win32, placeholder create/manage, hydration) + `Windows.Storage.Provider` WinRT (sync-root registration). Introduced Windows 10 1709. Backed by the in-box minifilter driver `cldflt.sys` (NTFS-only). **No driver to write, but "Cloud sync engines must be implemented in desktop apps" (no UWP), and Desktop Bridge (MSIX identity) is "an implementation requirement."** Source: Microsoft "Build a Cloud Sync Engine that Supports Placeholder Files" (https://learn.microsoft.com/en-us/windows/win32/cfapi/build-a-cloud-file-sync-engine, fetched in full).
- Feature set (same doc):
  - Placeholders (~1 KB), three states: placeholder / full file / pinned full file; automatic hydration on any file API access.
  - Standardized sync-root registration → branded node in Explorer navigation pane.
  - Shell integration: system hydration-state icons, inline progress + toasts, thumbnails and metadata for placeholders, context-menu verbs (pin/unpin), copy-hook (`IStorageProviderCopyHook`, Win10 19624+), Share handler (Win11 21H2+), and **cloud search handler integration (Win11 24H2, Copilot+ PCs)**.
  - Hydration policies: Always full > Full > Progressive > Partial; effective policy = max(app, provider).
- **This is exactly what OneDrive Files On-Demand, Google Drive for desktop, and Dropbox use on Windows.** Source: cloud-filter crate docs "used in production by OneDrive, Google Drive, Dropbox" (https://docs.rs/cloud-filter), ownCloud docs ("behaves like OneDrive… based on the same interface", https://doc.owncloud.com/desktop/5.3/vfs.html).

### Samples & wrappers

- **CloudMirror** official C++/WinRT sample: https://github.com/Microsoft/Windows-classic-samples/tree/master/Samples/CloudMirror — demonstrates registrar, placeholders, shell services, chunked "download" with progress. Explicitly not production quality.
- **Rust:** `cloud-filter` crate — "safe and idiomatic wrapper", supports placeholders (partial/full/pinned), Explorer icons/progress, thumbnails/metadata; MIT; fork of `wincs`; no stable release on crates.io yet (moderate maturity — usable but budget for gaps). Sources: https://github.com/ho-229/cloud-filter-rs, https://github.com/ok-nick/wincs, https://docs.rs/cloud-filter. Raw bindings also in the official `windows` crate (`Win32::Storage::CloudFilters`, https://microsoft.github.io/windows-docs-rs/doc/windows/Win32/Storage/CloudFilters/).
- **.NET:** `styletronix/cfapiSync` — C# cloud sync engine sample built on cfapi (https://github.com/styletronix/cfapiSync); `Vanara.PInvoke.CldApi` — maintained P/Invoke bindings, .NET 4.8→9.0 (https://www.nuget.org/packages/Vanara.PInvoke.CldApi, https://github.com/dahall/Vanara); `JDanielSmith/CloudFilter.NET` (https://github.com/JDanielSmith/CloudFilter.NET).
- **Go:** no meaningful cfapi wrapper found (searches surfaced nothing beyond raw syscall potential) — Go on Windows would mean writing bindings yourself or going the WinFsp route.

### Alternatives on Windows

- **ProjFS** (Projected File System, https://learn.microsoft.com/en-us/windows/win32/projfs/projected-file-system): designed for *high-speed local* backing stores (built for VFS-for-Git); no hydration progress, no online/offline state; **Microsoft explicitly says to use Cloud Files API for slow/cloud backends.** Managed wrapper: https://github.com/microsoft/ProjFS-Managed-API. → Wrong tool for a Telegram-backed drive.
- **WinFsp** (FUSE for Windows, https://github.com/winfsp/winfsp, https://winfsp.dev/): mature, actively maintained (WinFsp 2025 release), millions of installs — it's what `rclone mount` uses on Windows. License **GPLv3 with FLOSS exception + commercial license available**. Requires installing its driver → more install friction than cfapi (which is in-box). Good for a power-user "mount" mode, not for the polished Dropbox-like default.
- **Dokan/Dokany** (https://github.com/dokan-dev/dokany, LGPL/MIT): user-mode FS with own kernel driver; `DokanCloudFS` (https://github.com/viciousviper/DokanCloudFS) is an older cloud-FS abstraction over it. Less momentum than WinFsp.
- **CBFS Connect** (commercial, Callback Technologies, https://www.callback.com/cbfsconnect): cross-platform virtual-drive SDK; 2026 release adds FUSE-like API across platforms incl. Android/iOS. Enterprise pricing — an option if buying is acceptable.

### Verdict (Windows)

cfapi sync engine, packaged MSIX. If the shared core is Rust — `cloud-filter` crate + `windows` crate; if .NET — cfapiSync/Vanara as references. Explorer integration (badges, progress, pin/unpin, sidebar node) comes almost free.

---

## 4. Android — Storage Access Framework `DocumentsProvider`

- To expose chats-as-folders in the system Files app (DocumentsUI) and every `ACTION_OPEN_DOCUMENT` picker, implement a `DocumentsProvider` (API 19+). Official guide: https://developer.android.com/guide/topics/providers/create-document-provider.
- Core = 4 methods: `queryRoots`, `queryChildDocuments`, `queryDocument`, `openDocument` (returns `ParcelFileDescriptor`). Manifest: `<provider>` with `android.content.action.DOCUMENTS_PROVIDER` intent filter, `android:permission="android.permission.MANAGE_DOCUMENTS"`, `exported=true`, `grantUriPermissions=true`. (Same guide.)
- Capabilities via `COLUMN_FLAGS`: `FLAG_SUPPORTS_CREATE/DELETE/RENAME/COPY/MOVE/REMOVE`, thumbnails (`openDocumentThumbnail`), **recents** (`queryRecentDocuments`), **search** (`querySearchDocuments`), **virtual documents** (`FLAG_VIRTUAL_DOCUMENT` + `openTypedDocument`) — virtual docs are ideal for "chat export as .txt/.html" pseudo-files. (Same guide.)
- Remote/cloud content: `openDocument` may do network work (must poll `CancellationSignal`); for streaming use `ParcelFileDescriptor.createReliablePipe()` / socket pair, or `StorageManager.openProxyFileDescriptor` for random-access on-demand (proxy FD). Hide roots by returning an empty cursor from `queryRoots` when logged out; call `notifyChange(buildRootsUri(authority))` on login. (Same guide.)
- Limits/notes: provider runs in your app process (normal app memory limits, far more generous than iOS's 20 MB); provider must be Kotlin/Java (the component itself), but it can delegate to Rust/Go/C++ core over JNI. No system-driven "sync engine" — SAF is pull-based browse/open; there is no OS-managed offline/placeholder pinning UX like cfapi/FileProvider (apps like DocumentsUI just stream via your FD).
- DocumentsProvider is *the* mechanism third-party clouds use to appear in Files on Android (Google's own guide frames it as "cloud storage service" integration). Reference implementation pattern: Android StorageProvider sample in the guide; DeepWiki/AOSP DocumentsUI shows roots automatically.

### Verdict (Android)

Cheapest platform by far. Read-only chats-as-folders + on-demand `openDocument` streaming from Telegram is a small, well-trodden component.

---

## 5. Linux — FUSE (brief)

- **libfuse** (C, reference implementation) — the standard.
- **go-fuse v2** (https://github.com/hanwen/go-fuse) — actively maintained (commits through 2026), protocol support up to 7.12.28, performance competitive with libfuse; powers gocryptfs. Source: https://pkg.go.dev/github.com/hanwen/go-fuse/v2/fuse.
- **fuser** (Rust, https://github.com/cberner/fuser) — maintained successor of the abandoned `fuse-rs`/`rust-fuse` line; implements most of libfuse ≤3.10.3; tested on Linux & FreeBSD. Source: https://docs.rs/fuser.
- Best on-demand-cloud-FS reference: **jstaf/onedriver** (https://github.com/jstaf/onedriver) — Go + go-fuse; *not* a sync client: on-demand download with aggressive local caching, offline mode, multipart downloads for >10 MB files. Exactly the semantics a Telegram drive wants on Linux.
- Alternatively just document `rclone mount` (see §6) for Linux users.

---

## 6. Cross-platform shortcut approaches (WebDAV / SMB / NFS / rclone)

### Local server + OS mount

- **macOS Finder:** native WebDAV/SMB/NFS mounting (Cmd-K "Connect to Server") — works fine; no Finder sync badges/pins, appears as network volume.
- **Windows Explorer:** WebDAV via the WebClient service is a dead end: **50 MB default file-size limit** (`FileSizeLimitInBytes` registry), **hard 4 GB max**, poor performance/memory behavior, and **the WebClient (WebDAV) service was officially deprecated by Microsoft (Windows 11 23H2 era, no longer starts by default)**. Sources: https://www.myworkdrive.com/blog/webdav-file-size-limit, https://learn.microsoft.com/en-us/troubleshoot/windows-client/networking/cannot-access-webdav-web-folder, deprecation: https://learn.microsoft.com/en-us/answers/questions/1609470/inquiry-about-the-deprecation-of-webclient-webdav, https://petri.com/microsoft-deprecates-features-windows-11/. SMB loopback servers on Windows conflict with the in-box SMB stack (port 445) — also painful.
- **iOS Files:** natively supports **SMB** ("Connect to Server" since iOS 13), but **not WebDAV** — WebDAV requires a third-party app (e.g., FileBrowser) that itself exposes a File Provider. Sources: https://www.howtogeek.com/devops/how-to-connect-to-network-shares-with-the-ios-files-app/, https://www.stratospherix.com/articles/how-to-access-filebrowser-locations-from-files-app.php, https://discussions.apple.com/thread/252421470. An on-device local SMB server for loopback mounting is not a realistic iOS architecture (background execution limits) — iOS effectively *requires* the File Provider route.
- **Android:** Files by Google/DocumentsUI have no network-mount feature; third-party managers (CX, Solid) speak SMB/WebDAV themselves. DocumentsProvider remains the right answer.

### rclone as integration layer

- `rclone mount` (https://rclone.org/commands/rclone_mount/) — FUSE on Linux/macOS (macFUSE or FUSE-T), **WinFsp on Windows**; VFS caching modes make it usable for random access.
- `rclone serve webdav` (https://rclone.org/commands/rclone_serve_webdav/) and `rclone serve nfs` — serve any remote over a protocol the OS can mount.
- **Telegram backend status:** official rclone has **none** — feature request open since 2021, "Help Wanted" milestone (https://github.com/rclone/rclone/issues/5829; forum: https://forum.rclone.org/t/telegram-unlimited-cloud-storage-and-rclone/27490). Community options:
  - **teldrive** (https://github.com/tgdrive/teldrive) — Go server exposing Telegram storage via its own API/UI; used with a **patched rclone fork carrying a `teldrive` backend** (docs: https://teldrive-docs.pages.dev/docs/guides/rclone; fork moved to Codeberg: https://codeberg.org/teldrive/rclone; note forum thread "Teldrive project has ended?" — maintenance is volatile: https://forum.rclone.org/t/teldrive-project-has-ended/50429).
  - **TGFS** (https://github.com/TheodoreKrypton/tgfs) — "Telegram becomes a WebDAV server": private-channel storage, folder emulation, file chunking for unlimited size, video streaming. **Name collision with this project — check before branding.**
  - `birdup000/RcloneTelegram` (https://github.com/birdup000/RcloneTelegram) — bot-API based rclone backend, hobby-grade.
- Caveat for all of these: they treat Telegram as a *blob store* (upload files to a channel), not as "existing chats as folders" — different problem than ours, but their Telegram chunking/rate-limit handling is directly reusable prior art.

### Verdict (shortcuts)

`rclone serve webdav`/`serve nfs` on top of our own backend is a great **dev-tool / power-user MVP** (macOS + Linux mounts, docs for Windows via WinFsp-based `rclone mount`), but it cannot deliver the Dropbox-grade UX (badges, on-demand placeholders, Files-app presence on mobile). Native integrations remain necessary for the real product.

---

## 7. Open-source projects worth studying / reusing

| Project | Stack | What to steal |
|---|---|---|
| **Apple FruitBasket sample** (https://developer.apple.com/documentation/fileprovider/replicated_file_provider_extension/synchronizing_files_using_file_provider_extensions, mirror https://github.com/seanses/FileProviderTrial) | Swift | Canonical replicated-extension structure for iOS+macOS, domain management, decorations, local test server |
| **nextcloud/desktop** (https://github.com/nextcloud/desktop) | C++/Qt + Swift | Production client: **Windows VFS via cfapi (`WindowsCfApiVFS`)**, **macOS VFS via a separate File Provider client** (https://deepwiki.com/nextcloud/desktop/4.2-windows-integration, https://github.com/nextcloud/desktop/wiki/Virtual-files-client-for-macOS) |
| **nextcloud/nextcloudfileproviderkit** (https://github.com/nextcloud/nextcloudfileproviderkit) | Swift package | The closest thing to a reusable abstraction over `NSFileProviderReplicatedExtension` (items, enumeration, materialized-set handling, mocks for testing) |
| **nextcloud/ios**, **owncloud/ios-app** | Swift / ObjC | Classic-API File Provider extensions in production (patterns + what to avoid) |
| **owncloud/client** (https://github.com/owncloud/client, https://doc.owncloud.com/desktop/5.3/vfs.html) | C++/Qt | Clean cfapi VFS plugin architecture (`wincfapi` plugin; VFS is pluggable per-platform — good abstraction blueprint) |
| **Microsoft CloudMirror** (https://github.com/Microsoft/Windows-classic-samples/tree/master/Samples/CloudMirror) | C++/WinRT | cfapi end-to-end: registrar, placeholders, shell services, hydration progress |
| **ho-229/cloud-filter-rs** (https://github.com/ho-229/cloud-filter-rs) / **ok-nick/wincs** | Rust | Idiomatic cfapi wrapper if core is Rust |
| **styletronix/cfapiSync** (https://github.com/styletronix/cfapiSync), **Vanara CldApi** (https://github.com/dahall/Vanara) | C# | cfapi from .NET |
| **jstaf/onedriver** (https://github.com/jstaf/onedriver) | Go + go-fuse | On-demand (non-sync) cloud FUSE FS with offline cache — the exact semantics for Linux |
| **Maestral** (https://github.com/samschott/maestral) | Python | **Archived June 2026** — study only; classic full-sync (no virtual files); shows the maintenance cost of a solo sync client |
| **rclone** (https://github.com/rclone/rclone) | Go | VFS layer (`rclone mount`), serve webdav/nfs; backend interface design |
| **tgdrive/teldrive** (https://github.com/tgdrive/teldrive), **TheodoreKrypton/tgfs** (https://github.com/TheodoreKrypton/tgfs) | Go / TS | Telegram-as-storage prior art: chunked uploads, bot/MTProto rate limits, WebDAV bridging |
| **CBFS Connect 2026** (https://www.callback.com/cbfsconnect) | commercial SDK | Only true cross-platform virtual-drive abstraction (now incl. FUSE-like API + Android/iOS) — buy-vs-build benchmark |

**Reusable abstraction layers over cfapi/FileProvider/FUSE:** effectively none open-source and cross-platform. Nextcloud/ownCloud each built an internal VFS plugin abstraction (Qt-based, sync-engine-coupled); `nextcloudfileproviderkit` covers Apple only; `cloud-filter` covers Windows only. The only unified commercial one is CBFS Connect. Plan for a **shared core + thin per-platform native adapters** architecture.

---

## 8. Cross-platform app frameworks & native extensions

- **Flutter (iOS/macOS):** officially supports adding native app extensions — including File Provider extensions — as ordinary Xcode targets in the Runner project; extension code itself is Swift/ObjC (Dart/Flutter UI can technically be embedded but is pointless for a headless FP extension; ~100 MB+ engine memory makes Dart-in-FP-extension a non-starter against the 20 MB iOS cap). Communication via App Groups (shared files/DB). Source: https://docs.flutter.dev/platform-integration/ios/app-extensions.
- **React Native:** same story — native extension targets alongside the RN app; the extension is Swift/ObjC.
- **Electron / desktop:**
  - Windows: cfapi is plain Win32 callable from a Node native addon or a sidecar service/daemon (Rust/C++/C#). Nothing framework-specific blocks it; note Desktop Bridge/MSIX packaging requirement for sync-root registration.
  - macOS: an Electron app **can** embed a File Provider appex in its bundle, but the appex must be native Swift/ObjC built with Xcode; signing/notarization/provisioning of appex-inside-Electron is fiddly but done in the wild (e.g., commercial clients). The realistic architecture is: Electron UI + native Swift FP extension + shared local daemon/DB.
- **Bottom line:** on every platform the OS-integration component is native-ish (Swift appex on Apple, Kotlin/Java provider on Android, any-language Win32 on Windows, any-language FUSE on Linux), and *all of them can be thin shims over a shared core* (Rust/Go/Kotlin-MP core exposed via FFI/JNI/local RPC). The cross-platform framework choice affects only the app shell UI, not the drive integration.

---

## 9. Effort signals

- **Windows cfapi:** lowest-risk desktop target. In-box driver, official sample, mature wrappers. A read-only placeholder drive: weeks. Full two-way sync engine: months (conflicts, offline, shell polish).
- **Apple File Provider:** the sync-state machine is system-owned, which removes work but adds opacity; practitioners (Nextcloud) took roughly **1.5–2 years to a solid macOS VFS client** (announced as wiki/experimental 2023 → stable "one client" flow by v4.0 2025; see https://github.com/nextcloud/desktop/wiki/Virtual-files-client-for-macOS and https://help.nextcloud.com/t/desktop-client-4-0-0-for-macos-is-here-questions-vfs/234427) — though that includes full read-write sync against a generic server. A **read-only, on-demand** provider (our case) is drastically simpler: FruitBasket + nextcloudfileproviderkit patterns → several weeks to a demo, a quarter to production polish. iOS adds the 20 MB memory discipline but shares ~90% of extension code with macOS.
- **Android DocumentsProvider:** days-to-weeks for read-only roots/browse/open; thumbnails/search/recents another week or two.
- **Linux:** if rclone-target docs aren't enough, an onedriver-style go-fuse/fuser FS: weeks.
- **Ongoing tax:** each macOS release breaks something in FP land (see Nextcloud issue tracker, e.g. https://github.com/nextcloud/desktop/issues/8599); Dropbox still maintains an opt-out path years into its migration — budget continuous platform-maintenance capacity.

---

## 10. Key source list

- Apple File Provider: https://developer.apple.com/documentation/fileprovider · https://developer.apple.com/documentation/fileprovider/nsfileproviderreplicatedextension · https://developer.apple.com/documentation/fileprovider/nsfileproviderextension · FruitBasket sample: https://developer.apple.com/documentation/fileprovider/replicated_file_provider_extension/synchronizing_files_using_file_provider_extensions · WWDC21: https://developer.apple.com/videos/play/wwdc2021/10182/
- FP memory limit: https://developer.apple.com/forums/thread/739839 · https://developer.apple.com/forums/thread/73148
- macOS FP migration: https://tidbits.com/2023/03/10/apples-file-provider-forces-mac-cloud-storage-changes/ · https://support.google.com/drive/answer/12178485 · https://help.dropbox.com/installs/dropbox-for-macos-support
- macFUSE: https://macfuse.github.io/ · https://github.com/macfuse/macfuse/wiki/FUSE-Backends · FUSE-T: https://www.fuse-t.org/ · https://github.com/macos-fuse-t/fuse-t · FSKit: https://developer.apple.com/documentation/FSKit
- Windows cfapi: https://learn.microsoft.com/en-us/windows/win32/cfapi/build-a-cloud-file-sync-engine · https://learn.microsoft.com/en-us/windows/win32/cfapi/cloud-files-api-portal · CloudMirror: https://github.com/Microsoft/Windows-classic-samples/tree/master/Samples/CloudMirror · ProjFS: https://learn.microsoft.com/en-us/windows/win32/projfs/projected-file-system
- cfapi wrappers: https://github.com/ho-229/cloud-filter-rs · https://github.com/ok-nick/wincs · https://github.com/styletronix/cfapiSync · https://www.nuget.org/packages/Vanara.PInvoke.CldApi · https://github.com/JDanielSmith/CloudFilter.NET
- WinFsp: https://github.com/winfsp/winfsp · Dokany: https://github.com/dokan-dev/dokany · CBFS Connect: https://www.callback.com/cbfsconnect
- Android: https://developer.android.com/guide/topics/providers/create-document-provider · https://developer.android.com/reference/android/provider/DocumentsProvider
- Linux FUSE: https://github.com/hanwen/go-fuse · https://github.com/cberner/fuser · https://github.com/jstaf/onedriver
- WebDAV/SMB: https://rclone.org/commands/rclone_serve_webdav/ · https://www.myworkdrive.com/blog/webdav-file-size-limit · https://learn.microsoft.com/en-us/answers/questions/1609470/inquiry-about-the-deprecation-of-webclient-webdav · https://www.howtogeek.com/devops/how-to-connect-to-network-shares-with-the-ios-files-app/
- rclone/Telegram: https://github.com/rclone/rclone/issues/5829 · https://github.com/tgdrive/teldrive · https://teldrive-docs.pages.dev/docs/guides/rclone · https://github.com/TheodoreKrypton/tgfs
- Clients: https://github.com/nextcloud/desktop · https://github.com/nextcloud/nextcloudfileproviderkit · https://deepwiki.com/nextcloud/apple-clients/6.1-file-provider-system · https://github.com/nextcloud/ios · https://github.com/owncloud/ios-app · https://github.com/owncloud/client · https://doc.owncloud.com/desktop/5.3/vfs.html · https://github.com/samschott/maestral (archived 2026)
- Flutter extensions: https://docs.flutter.dev/platform-integration/ios/app-extensions
- Blog (practitioner): https://claudiocambra.com/posts/build-file-provider-sync/
