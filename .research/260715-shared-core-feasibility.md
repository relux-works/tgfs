# Research 05 — Shared-Core Feasibility (TDLib + Rust core + native shells)

Date: 2026-07-15. Status of all sources checked as of mid-2026.
Scope: verify feasibility claims for the architecture "TDLib (C++) engine + shared Rust core (archive/VFS logic) + thin native UI shells" for a Dropbox-like Telegram file-sync app (macOS/Windows/iOS/Android, File Provider / Cloud Filter API / DocumentsProvider).

---

## 1. TDLib inside an iOS File Provider extension

### Memory caps (confirmed numbers)

- **File Provider extension on iOS: 20 MB hard limit**, confirmed by an Apple engineer on the dev forums: "FileProvider app extension processes have a 20 MB memory limit on iOS." Users report OOM kills even when just downloading files >15 MB; the recommended workaround is streaming APIs (`URLSession.uploadTask(with:from:)`) that never load the file into memory.
  - https://developer.apple.com/forums/thread/739839
- Share extensions: ~120 MB limit; Notification Service Extension: ~24 MB (older docs said 5-15 MB depending on runtime).
  - https://blog.kulman.sk/dealing-with-memory-limits-in-app-extensions/
  - https://alastaircoote.github.io/notification-service/
- **No comparable documented 20 MB macOS File Provider cap was found.** This is not proof that macOS extensions are unbounded; the macOS adapter should still remain thin and prefer a companion process for the full engine.
  - https://developer.apple.com/forums/tags/fileprovider

### Evidence of TDLib in any app extension

- **None found.** No GitHub issues, projects, or blog posts show TDLib running inside an iOS/macOS share/notification/file-provider extension. Search of tdlib/td issues for "share extension" / "extension ios memory" returns nothing relevant.
- TDLib's iOS binary itself is heavy: prebuilt xcframeworks ~300 MB zip; a built dylib reported at 269 MB (debug; release with LTO much smaller but still tens of MB per arch). Code pages are clean memory (mostly not counted against jetsam dirty footprint), but TDLib's *heap* — SQLite caches, chat/user caches, update queues — is what kills the 20 MB budget.
  - https://github.com/tdlib/td/issues/3226
  - https://github.com/Swiftgram/TDLibFramework

### TDLib memory tuning (what exists, what it buys)

- Maintainer (levlam) on minimum RAM: "Make sure that all databases are enabled. If they are, then you can't decrease RAM usage more." I.e. **`use_message_database = true` (implies `use_chat_info_database` and `use_file_database`) is the memory-MINIMIZING configuration** — data is offloaded to SQLite on disk instead of being kept in RAM. Disabling the databases *increases* RAM.
  - https://github.com/tdlib/td/issues/2516
- Baseline per-instance footprint (from levlam, in a 100-client server context): **~10 MB of native heap per fresh client instance**, "less than 1 MB" if the whole chat list is not loaded; plus slow growth (~180 KB/h/client observed while idle listening to updates) largely attributed to allocator fragmentation. A real mobile account with a loaded chat list is well above this — typical mobile RSS is tens of MB.
  - https://github.com/tdlib/td/issues/2807
- Writable options that reduce RAM (official): `message_unload_delay` (unload messages from memory after 60-86400 s), `ignore_inline_thumbnails`, `ignore_file_names`, `use_storage_optimizer` (disk, not RAM), `disable_persistent_network_statistics`.
  - https://core.telegram.org/tdlib/options
- The aggressive memory options often cited (`disable_minithumbnails`, `disable_document_filenames`, `disable_notifications`, `ignore_server_deletes_and_reads`, `delete_chat_reference_after_seconds`, `getMemoryStatistics`) are **TDLight fork extensions, not upstream TDLib**.
  - https://github.com/tdlight-team/tdlight
- `optimizeStorage` exists upstream but manages *disk* usage of downloaded files, not RSS.

**Conclusion Q1: do not design around TDLib inside an iOS File Provider extension.** The reported ~10 MB per-instance figure comes from a particular multi-client server workload, not a formal universal heap floor. Combined with SQLite/update processing, Swift/File Provider overhead, the confirmed 20 MB process limit, and no found precedent, it leaves insufficient engineering margin. On macOS no comparable 20 MB cap was found, but a thin extension plus companion engine is still the safer process boundary.

### How Telegram-iOS itself solves this (the fallback pattern)

Telegram-iOS does NOT use TDLib at all — it has its own Swift/ObjC stack (TelegramCore + MTProtoKit + Postbox). Its extensions are instructive:

- **Share extension**: creates its own *temporary account context* inside the extension via `makeTempContext(sharedContainerPath: appGroupUrl.path, ..., apiId: buildConfig.apiId, apiHash: buildConfig.apiHash, ...)` — i.e. it **runs its own MTProto network instance in the extension process**, reusing the session/auth and the Postbox database from the app-group container. It fits because the 120 MB share-extension cap is generous and TelegramCore is modular (`extension_safe` build flags).
  - https://github.com/TelegramMessenger/Telegram-iOS/blob/master/Telegram/Share/ShareRootController.swift
- **NotificationService extension** (24 MB-class budget): uses a *stripped-down* pipeline — `standaloneStateManager` + `standaloneMultipartFetch` with full `NetworkInitializationArguments` — so even here a minimal MTProto connection is spun up in-process to decrypt pushes (MTProto v2-encrypted payloads per https://core.telegram.org/api/push-updates) and fetch small media, against the shared Postbox.
  - https://github.com/TelegramMessenger/Telegram-iOS/blob/master/Telegram/NotificationService/Sources/NotificationService.swift
- Pattern: **shared on-disk state via app group + purpose-built minimal network stack per extension**, NOT "run the full engine in the extension" and NOT "IPC to the main app" (iOS provides no sanctioned extension→app IPC while the app is suspended).

**Implication for tgfs**: the FP extension must be a thin metadata/placeholder layer over a database in the app-group container, with actual TDLib work done in the main app (launched via `NSFileProviderServiceSource`-style coordination or backgrounded work), or downloads done in the extension via plain HTTPS-ranged streams where possible. On macOS, TDLib can live inside the FP extension process or a companion daemon. The iOS story is the weakest link of the architecture and needs an explicit "extension = placeholders + enqueue; app = transfer engine" design.

---

## 2. Rust <-> Swift/Kotlin bindings in 2026

### uniffi-rs (mozilla/uniffi-rs)

- **Active and healthy**: repo pushed 2026-07-10; 4,785 stars; crates.io `uniffi` 0.32.0 published 2026-06-30; 9.28 M downloads. Release cadence: v0.29.5 → v0.30.0 → v0.31.0/.1/.2 → v0.32.0 over roughly the last year. Still pre-1.0 ("ready for production use, but a long way from a 1.0 release").
  - https://github.com/mozilla/uniffi-rs
  - https://crates.io/crates/uniffi
- **Production adoption**:
  - Firefox mobile (Android/iOS) and desktop — "used extensively by Mozilla"; JS bindings via uniffi-bindgen-gecko-js. https://firefox-source-docs.mozilla.org/rust-components/developing-rust-components/uniffi.html
  - **matrix-rust-sdk / Element X (iOS + Android)** — the flagship third-party case: `matrix-sdk-ffi` generates Swift/Kotlin bindings with UniFFI; SDK is "production ready" and powers shipping Element X apps. This is architecturally the closest precedent to tgfs (shared Rust protocol/sync core, thin SwiftUI/Compose shells).
    - https://github.com/matrix-org/matrix-rust-sdk
    - https://github.com/element-hq/element-x-ios
    - https://github.com/element-hq/element-x-android
  - uniffi-bindgen-react-native (Mozilla Hacks, Dec 2024): https://hacks.mozilla.org/2024/12/introducing-uniffi-for-react-native-rust-powered-turbo-modules/
  - NordSecurity maintains uniffi-bindgen-cs (pushed 2026-06) and uniffi-bindgen-go (pushed 2026-04) — used in NordVPN's cross-platform libs.
- **1Password**: shares a Rust core across macOS/iOS/Windows/Android/Linux/browser ("put every feasible piece of 1Password into the Core library"), but uses its own FFI + **typeshare** for type sync, not uniffi. Still strong evidence for the "Rust core + native shells" pattern itself.
  - https://1password.com/blog/typeshare-for-rust
  - https://dteare.medium.com/behind-the-scenes-of-1password-for-linux-d59b19143a23
  - https://corrode.dev/podcast/s04e06-1password/
- **Nextcloud**: no evidence of a Rust/uniffi client core (desktop client remains C++/Qt; Rust only in the server-side High Performance Back-end). Claim "Nextcloud uses uniffi" is NOT confirmed.

### Alternatives

- **swift-bridge** 0.1.59 (Jan 2026), 1.9 M downloads — viable Swift-only bridge, more ergonomic async/borrowing than uniffi but no Kotlin side.
- **flapigen** 0.11.0 (Apr 2026), 179 K downloads — alive but niche.
- **cxx** (dtolnay) — the standard for the Rust↔C++ seam; the natural choice for wrapping TDLib's C++/JSON interface from Rust (or just use `td_json_client` C interface directly, no cxx needed).

**Conclusion Q2: uniffi is the right default** — one interface definition → Swift + Kotlin, proven at scale by Firefox and Element X. gomobile-style alternatives are inferior for this stack (see §5).

---

## 3. Rust Windows Cloud Filter wrapper

- **ho-229/cloud-filter-rs** (fork of ok-nick/wincs): crates.io `cloud-filter` 0.0.6 (2025-09-29), last repo push 2025-11-02, 8 stars, 0 open issues, 25 K downloads. Fixed Rust 1.90 compat in Sep 2025. **Alive but early (0.0.x) and a one-maintainer project; no known production deployments.**
  - https://github.com/ho-229/cloud-filter-rs
  - https://crates.io/crates/cloud-filter
- **Apache OpenDAL** added a `cloud_filter` integration (PR #4779) and maintained it through 2025 (e.g. fix #5416, Rust 1.90 fix #6602 using the upgraded crate), then **removed it in Oct 2025** (PR #6727) — but as part of a general scope-reduction restructuring (issue #6689 "Move oli/ofs out; remove oay"), not a stated quality rejection. Net effect: the most visible consumer is gone.
  - https://github.com/apache/opendal/pull/6727
- Fallback: the raw `windows` crate exposes `Win32::Storage::CloudFilters` fully (Microsoft-generated) — writing a thin own wrapper over CfAPI from Rust is well-trodden ground.
  - https://microsoft.github.io/windows-docs-rs/doc/windows/Win32/Storage/CloudFilters/
- **styletronix/cfapiSync (C#)**: sample-grade sync engine using CfAPI; has open crash bugs (e.g. `System.AccessViolationException` on placeholder delete, issue #2); useful as reference code, not as a dependency.
  - https://github.com/styletronix/cfapiSync
- CfAPI itself is the production path (OneDrive, Dropbox, Google Drive all use it).
  - https://learn.microsoft.com/en-us/windows/win32/cfapi/cloud-files-api-portal

**Conclusion Q3: feasible, but budget for owning the CfAPI layer** — use cloud-filter-rs as a starting point/reference, expect to fork or write against `windows` crate directly.

---

## 4. Rust FUSE (fuser) and macFUSE/FSKit, mid-2026

- **fuser** (cberner/fuser): 0.17.0 (Feb 2026), repo pushed 2026-07-09, 1,257 stars, 4.1 M downloads — actively maintained, the standard Rust FUSE binding (Linux; macOS via macFUSE).
  - https://github.com/cberner/fuser
- **FSKit**: Apple's user-space file system API, introduced macOS 15.4, now the sanctioned successor path to kexts.
  - https://developer.apple.com/forums/tags/fskit
- **macFUSE**: very much alive — 5.2.0 (Apr 2026), 5.3.3 (Jul 2026); now ships an **FSKit backend** so FUSE file systems run fully in user space on macOS 26 with no kext / recovery-mode dance. Caveat: FSKit-backend I/O performance is not yet on par with the kext backend; kext remains default until parity.
  - https://macfuse.github.io/2026/04/09/macfuse-5.2.0.html
  - https://github.com/macfuse/macfuse/wiki/FUSE-Backends
- Note: for this product, File Provider (not FUSE/FSKit) remains the App Store-compatible integration on macOS; FUSE/FSKit is a power-user alternative distribution path.

---

## 5. gomobile / Go inside iOS app extensions

- Documented failure mode: Go framework for an iOS NetworkExtension occupied ~13 MB with the **Go runtime alone taking ~5 MB**; goroutine memory does not return to the OS; extension killed at the (then) 15 MB NE limit.
  - https://groups.google.com/g/golang-nuts/c/4OmowR7gjXc
- Go's GC/scavenger holds reserved memory against the process jetsam footprint; slab allocator over-reserves (8 KB OS ask for a 24-byte alloc).
  - https://github.com/golang/go/issues/21489 , https://github.com/golang/go/issues/16598
- Tailscale's "Hey linker, can you spare a meg?" documents their multi-year fight to keep Go inside the iOS network extension memory limit — even for a mature team this is an ongoing tax.
  - https://tailscale.com/blog/go-linker
- Community consensus in the traffic-obf/WireGuard discussions: prefer C/Swift inside extensions; Go only if you already have the codebase and accept the pain.

**Conclusion Q5: Go/gomobile is NOT viable inside a 20 MB File Provider extension.** The Go runtime baseline alone eats 25% of the budget and its allocator behavior is hostile to jetsam accounting. Rust (no runtime, no GC; ~hundreds of KB overhead) is strictly better for the appex-adjacent core.

---

## 6. grammers (Rust MTProto, Codeberg)

- Moved to https://codeberg.org/Lonami/grammers ; active (last commit 2026-07-13), version 0.10.0 (bumped 2026-07-02). Crates: client / mtsender / session / mtproto / tl-types / crypto / tl-gen / tl-parser.
- README self-assessment: "It works! The high-level interface is slowly taking shape, and it can already be used to build real projects" (RSS bots cited). **Explicitly unaudited**: "this code has not been audited... review at least grammers-crypto and the authentication part of grammers-mtproto" if security-critical.
- **Downloads**: `Client::iter_download` with chunking (`MAX_CHUNK_SIZE` = 512 KiB), `skip_chunks(n)` for arbitrary-offset resume, `download_media`, plus a multi-connection document download with retry offsets. So resume-at-offset: YES.
- **CDN redirects: NOT supported** — requests are made with `cdn_supported: false` and the code panics on `File::CdnRedirect` ("API returned File::CdnRedirect even though cdn_supported = false"). CDN-served popular media would break downloads. (grammers-client/src/client/files.rs)
- **Takeout: raw TL only** — `account.initTakeoutSession`, `invokeWithTakeout`, `inputTakeoutFileLocation` exist in the generated `grammers-tl-types/tl/api.tl`, but there is **no high-level takeout support** in grammers-client; you'd hand-roll session management over raw invocations.
- **Updates**: real machinery exists — `stream_updates` with a message-box, `catch_up` configuration, gap resolution ("updates are guaranteed to be in order, and any gaps will be resolved", with the caveat that peers must be known), `sync_update_state`. Decent, but you own peer/session persistence; there is no chats/messages database — everything TDLib's SQLite layer does for free (dialog cache, file metadata, flood-wait handling maturity) you rebuild yourself.

**Conclusion Q6: grammers is a credible library for bots/tools but NOT a TDLib replacement for this product in 2026**: no CDN download support (fatal for a file-sync app), no high-level takeout, no persistent message/chat store, unaudited crypto. Its value here: proof that a pure-Rust MTProto stack is possible, and a potential long-term escape hatch.

---

## 7. Precedents for the architecture

- **Dropbox "Rewriting the heart of our sync engine" (Mar 2020)**: desktop sync engine "Nucleus" rewritten in Rust after Python engine hit correctness limits; single control thread + typed state ("designing away invalid states"); shipped to all users. The canonical "sync engine in Rust" precedent.
  - https://dropbox.tech/infrastructure/rewriting-the-heart-of-our-sync-engine
  - https://dropbox.tech/infrastructure/-testing-our-new-sync-engine
- **Dropbox "The (not so) hidden cost of sharing code between iOS and Android" (Aug 2019)**: abandoned shared C++ *mobile UI/business* layer in favor of Swift/Kotlin — reasons: custom frameworks overhead, worse debugging/tooling, hiring C++-for-mobile engineers. Key nuance: they abandoned **shared C++ for app code**, while keeping the shared-engine idea alive on desktop in Rust. I.e. the industry lesson is exactly the architecture under evaluation: **share the engine, not the UI**.
  - https://dropbox.tech/mobile/the-not-so-hidden-cost-of-sharing-code-between-ios-and-android
- **1Password (2021→now)**: single Rust core across macOS/iOS/Windows/Android/Linux/browser + native UIs; typeshare keeps FFI types in sync; still their architecture per 2024-2025 podcasts (corrode "Rust in Production" S04E06, Syntax #776 — Rust core + WASM in browser).
  - https://1password.com/blog/1password-8-the-story-so-far
- **Element X / matrix-rust-sdk (2023→2026)**: shared Rust SDK (sync engine, crypto, sliding-sync UI logic) + SwiftUI/Compose shells, bindings via uniffi; shipping production messengers on both platforms. The most recent, most similar, fully open precedent.
  - https://element.io/blog/element-x-ignition/
- **Firefox**: Rust components shared across desktop+mobile via uniffi (Application Services).
- **Nextcloud**: counter-example — still per-platform native clients (C++/Qt desktop, Kotlin Android, Swift iOS), no shared core; no 2025-2026 announcement of one.

---

## 8. TDLib as a library in desktop background services

- **Unigram (Telegram for Windows)**: full-featured client built on the **TDLib SDK for UWP** — TDLib runs **in-process** as a native library consumed from C# ("rewritten from scratch in C# using TDLib SDK for Universal Windows Platform" — tdlib's own example README; Unigram docs confirm all core functionality goes through the TDLib client object in-process). Years of production use on Windows.
  - https://github.com/UnigramDev/Unigram
  - https://github.com/tdlib/td/blob/master/example/README.md
- **Headless/service usage**: the official Telegram Bot API server is built on TDLib and runs server-side; TDLib's README claims "each TDLib instance handles more than 37000 active bots simultaneously". ClientManager supports many clients per process; all native memory freed on client close (levlam, issue #2807) with the caveat of allocator fragmentation on churn (observed ~+400 MiB not returned to OS after closing/reopening 100 clients — use jemalloc/mimalloc and long-lived clients).
  - https://github.com/tdlib/td
  - https://github.com/tdlib/td/issues/2807
- No blocking issues found for TDLib in a Windows/Linux background service: it is thread-safe, actor-based, ships official C/C++/JSON interfaces, and is exactly how Unigram and the Bot API server consume it. Practical notes: OpenSSL/zlib deps, per-account SQLite database directory, keep `use_message_database=true` for the lowest RAM profile.

---

## Verdicts

1. **TDLib in an iOS File Provider extension: excluded from the architecture** (confirmed 20 MB process cap, material TDLib native memory in reported workloads, and insufficient safety margin). Fallback = shared state on disk in the App Group; the FP extension stays a thin placeholder/enumerator layer; transfers run in the main app's TDLib or a separately proven minimal fetch path. On macOS, no comparable 20 MB cap was found; prefer a companion engine anyway.
2. **Rust core + uniffi: recommended and production-proven** (Firefox, Element X/matrix-rust-sdk; 1Password validates Rust-core-plus-native-shells with different tooling). gomobile is disqualified for the extension story (Go runtime ~5 MB baseline, GC scavenger vs jetsam, Tailscale's ongoing pain).
3. **grammers cannot replace TDLib today**: no CDN download support (panics on CdnRedirect), takeout only via raw TL, no persistent chat/message store, unaudited crypto — despite active development (0.10.0, Jul 2026) and offset-resumable downloads.
4. **Strongest precedents**: Element X (shared Rust engine + uniffi + native UIs, shipping since 2023) and the two Dropbox posts jointly teach "share the engine (Rust), not the UI" — which is exactly this architecture. Windows engine-in-service is validated by Unigram (TDLib in-process) and the official Bot API server.
5. **Windows CfAPI from Rust: feasible but thin ice** — cloud-filter-rs is alive but 0.0.x/one-maintainer; OpenDAL dropped its integration in a restructuring. Plan to own that layer (fork or `windows`-crate direct).
