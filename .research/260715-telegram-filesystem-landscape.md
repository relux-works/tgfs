# Telegram-backed Virtual Filesystem: Library and Architecture Research

Date: 2026-07-15

## Executive conclusion

There is no single library that provides a complete Telegram client, exhaustive export, a web application, and native filesystem integration on all operating systems.

The strongest choices are:

1. **TDLib** for a full, continuously synchronized Telegram client engine, particularly when the same engine must run locally on mobile and desktop. It is official, actively maintained, cross-platform, maintains its own encrypted cache, preserves Telegram chat-list ordering, and uses the permissive Boost Software License.
2. **gotd/td** for a server-side export/synchronization service. It is an active pure-Go MTProto implementation, exposes raw API methods including the Takeout API, has efficient multi-account operation, and uses MIT. It requires substantially more application-level work than TDLib: dialog state, ordering, persistence, update-gap handling, and export semantics belong to the product.
3. **WTelegramClient** when the team prefers C#/.NET or Windows is the first-class target. It exposes the full raw MTProto API, file transfer, updates, and secret chats under MIT, but it is not a unified native mobile/web solution.

Recommended product direction:

- Treat the product primarily as a **native Dropbox/Google Drive-like filesystem client**, not as another Telegram UI. Defer the web application and rich chat UI.
- Put the Telegram/source boundary behind one provider-neutral Rust trait with two implementations: **local TDLib through tdjson** and an optional **remote gotd/td service over HTTPS**.
- Prefer **local-first for desktop and Android v1** to avoid mandatory infrastructure. Preserve the remote implementation for iOS cold hydration, Takeout backfill, self-hosting, and a possible hosted tier.
- If the remote tier is built, Telethon remains the fastest Python Takeout spike, while gotd/td is the preferred long-lived Go service core.
- Put shared client logic in a **Rust drive core**: virtual tree, local SQLite/cache, change cursor, hydration, range downloads, retries, naming, and offline state. Use UniFFI for Swift/Kotlin and link the core directly on Windows/Linux.
- Keep native provider layers thin: Swift File Provider, Kotlin DocumentsProvider, Rust CfAPI host, and Rust FUSE adapter. Native settings/status UI is acceptable and does not meaningfully reduce core reuse.
- Embed TDLib in the local source host on macOS, Windows, Linux, and Android. Do **not** embed it in the iOS File Provider extension; the extension has a 20 MB process limit and must stay thin. The iOS containing app may host TDLib.
- Put the canonical metadata in PostgreSQL/SQLite and binary media in content-addressed object storage. Generate filesystem views from that canonical model.
- Implement each OS integration with its native provider API: Apple File Provider, Android DocumentsProvider, Windows Cloud Files API, and Linux FUSE. A cross-platform UI toolkit does not replace these extensions.
- Start read-only. Mapping arbitrary filesystem writes, moves, and deletes back to Telegram is ambiguous and dangerous.

## Telegram libraries

| Candidate | Status on 2026-07-15 | License | Best use | Main issue |
|---|---|---:|---|---|
| [TDLib](https://github.com/tdlib/td) | Active; upstream commits in June 2026, version 1.8.65/1.8.66-era master | Boost Software License 1.0 | Full client engine on iOS, Android, macOS, Windows, Linux, server | C++ build/FFI complexity; no general raw MTProto escape hatch; Takeout API is not its primary interface |
| [gotd/td](https://github.com/gotd/td) | Active; v0.161.0 released 2026-07-14 | MIT | Multi-account server sync, Takeout/export, high-throughput downloader | Lower-level; the product must build its own durable client/database model |
| [WTelegramClient](https://github.com/wiz0u/WTelegramClient) | Active; API layer updates and commits in 2026 | MIT | C# backend, Windows-first product, rapid raw API implementation | Mostly one maintainer; not a browser/mobile-native universal engine |
| [teleproto](https://github.com/sanyok12345/teleproto) | Active but young; v1.228.1 released 2026-07-14 | MIT | Browser or Node prototype; GramJS-compatible migration | Small/new project; browser sessions hold account-equivalent auth keys; not suitable as the only product core yet |
| [tdl](https://github.com/iyear/tdl) | Active; v0.20.3 released 2026-05-23 | AGPL-3.0 | Reference CLI/prototype for downloads and JSON export | Application/tool, not a reusable full-client SDK; AGPL obligations; its protected-chat download feature conflicts with Telegram's client rules for a compliant product |
| [Telegram Desktop](https://github.com/telegramdesktop/tdesktop) | Active official desktop app | GPL-3.0 + OpenSSL exception | Reference implementation for Takeout/export and desktop behavior | Very large Qt application, not a library; GPL obligations if code is incorporated |
| [Telegram Web K](https://github.com/morethanwords/tweb) | Active official web client | GPL-3.0 | Web UI and browser-client reference | Does not solve export storage or OS providers; tightly coupled full application |
| [Telethon](https://codeberg.org/Lonami/Telethon) | **Active — moved to Codeberg**; the GitHub repository was archived 2026-02-21 and points to the new home; current docs identify v1.44 | MIT | Fast Python takeout/export prototype; convenient high-level `client.takeout()` wrapper | Python-only; a separate worker adds operational complexity if the main daemon is Go |
| [mtcute](https://github.com/mtcute/mtcute) | Active in July 2026 | MIT | Browser or TypeScript MTProto prototype via `@mtcute/web` | Pre-1.0; browser deployment stores account-equivalent authorization material client-side and duplicates sync state |
| Pyrogram | Archived 2024-12-23 | LGPL/GPL | Existing Python projects only | Explicitly unmaintained |
| GramJS | Archived 2026-07-14 | MIT | Existing JS projects during migration | Replaced by teleproto |

### Why TDLib is the safest full-client engine

TDLib handles networking, MTProto encryption, local storage, data consistency, ordered updates, reconnects, and file downloads. It supports Android, iOS, Windows, macOS, Linux, WebAssembly, and other targets through native interfaces or a stable C JSON interface. It can maintain a message database and encrypt its local data with an application-provided key.

TDLib also directly models the user's chat lists. Telegram currently has Main, Archive, and folder lists; the correct order is the descending pair `(position.order, chat.id)` within each list. This is much safer than reverse-engineering Telegram's pin/order rules from raw dialogs.

Sources:

- https://github.com/tdlib/td
- https://core.telegram.org/tdlib/getting-started
- https://github.com/tdlib/td/commits/master/

### Why gotd/td fits the export backend

gotd exposes the entire generated raw API and is designed for many concurrent user or bot clients. Its own documentation describes low idle overhead, parallel downloads with CDN support, pluggable sessions, automatic data-center migration, update handling, rate limiting, and broad tests. The latest visible release was v0.161.0 on 2026-07-14.

The decisive advantage is raw access to Telegram's Takeout API, which is explicitly designed to export all account data and media. The tradeoff is that gotd is lower-level: the product must implement a durable dialog/message model, update checkpointing, retries, ordering, and user-facing client behavior.

Sources:

- https://github.com/gotd/td
- https://core.telegram.org/api/takeout

### Initial export should use the Takeout API

Telegram's Takeout API supports flags for private chats, basic groups, supergroups, channels, contacts, and files with a selected maximum size. It defines split message ranges, dialog/history pagination, left-channel retrieval, forum topics, attached media, custom emoji, and file-reference refresh behavior. It may impose a security delay (`TAKEOUT_INIT_DELAY`) before data can be downloaded.

Use Takeout for a user-initiated initial snapshot. After it completes, switch to ordinary updates/history calls for continuous synchronization. Do not keep a Takeout session open indefinitely.

Sources:

- https://core.telegram.org/api/takeout
- https://core.telegram.org/method/account.initTakeoutSession

## API and policy constraints

### A bot library is insufficient

The product needs authorization as the user through Telegram's MTProto Client API. A bot does not represent the user's account and cannot enumerate the user's complete dialog/history set. Telegram's `messages.getHistory` is explicitly user-only.

Each distributed application must obtain its own `api_id` and `api_hash`; the sample IDs in Telegram's open-source apps are not for production distribution.

Sources:

- https://core.telegram.org/api/obtaining_api_id
- https://core.telegram.org/method/messages.getHistory

### “All files” has unavoidable exceptions

The accurate promise is: **all currently accessible and saveable cloud-chat messages/media selected by the user**.

Exceptions include:

- Deleted messages and earlier edit revisions that disappeared before the service observed them.
- Expired/self-destructing/view-once media. Telegram's API terms forbid preventing self-destructing content from disappearing.
- Protected-content chats. Telegram says clients must disable forwarding, downloads, copying, and screenshots for protected groups/channels. A compliant product must respect this even if a third-party downloader can technically bypass it.
- Secret chats from other devices. Secret chats are associated with device authorization keys, not the account globally; old events are not a durable cloud history and server queue entries may disappear after acknowledgment or seven days.
- Media whose file reference can no longer be refreshed or whose location is unavailable.

TDLib exposes message properties such as `can_be_saved` and protected-content flags; the product should enforce them.

Sources:

- https://core.telegram.org/api/terms
- https://core.telegram.org/api/content-protection
- https://core.telegram.org/tdlib/docs/classtd_1_1td__api_1_1message_properties.html
- https://core.telegram.org/api/end-to-end

### Branding and product behavior

Telegram's API terms require privacy care, a product-specific API ID, prominent disclosure that Telegram API is used, and support for sponsored messages when channel content is shown. The app title cannot contain “Telegram” unless prefixed by “Unofficial”, and the official logo cannot be used. The terms also prohibit using aggregated Telegram data to train or develop AI/ML systems.

Source: https://core.telegram.org/api/terms

## Filesystem integration by platform

| Platform | Correct integration | Capability | Important limitation |
|---|---|---|---|
| iOS / iPadOS | `NSFileProviderReplicatedExtension` (iOS 16+) or legacy nonreplicated File Provider | Appears in the system Files app; system manages placeholders and local materialization | Must be a native app extension; background/runtime limits make an always-available backend highly desirable |
| macOS | `NSFileProviderReplicatedExtension` | Finder integration with dataless placeholders, on-demand content, offline materialization, Spotlight working set | Native Swift/ObjC extension; FUSE is not the Dropbox-like system path on modern macOS |
| Android | `DocumentsProvider` through Storage Access Framework | Appears as a root in the system document picker; supports cloud-backed directories/files and lazy loading | URI/document-provider model, not a universal POSIX mount visible to every app |
| Windows 10 1709+ | Cloud Files API / Cloud Filter API (CfAPI) | Explorer sync root, placeholder files, hydration, pin/offline state | Native Win32/WinRT integration and sync-engine state machine |
| Linux | FUSE/libfuse | Ordinary mounted filesystem supplied by a userspace daemon | The daemon must remain running and implement caching/error semantics |
| Browser | Web UI plus downloads; optional File System Access export | Can write into a user-selected directory in supported Chromium browsers | Not an OS cloud-drive provider; requires explicit user permission, is not universally supported, and cannot create a persistent Finder/Explorer sidebar drive by itself |

Primary sources:

- Apple File Provider: https://developer.apple.com/documentation/fileprovider
- Apple replicated provider: https://developer.apple.com/documentation/fileprovider/replicated-file-provider-extension
- Android DocumentsProvider: https://developer.android.com/reference/android/provider/DocumentsProvider
- Windows Cloud Files: https://learn.microsoft.com/en-us/windows/win32/cfapi/cloud-files-api-portal
- Linux FUSE: https://docs.kernel.org/filesystems/fuse/
- Browser File System Access: https://developer.chrome.com/docs/capabilities/web-apis/file-system-access

## Recommended product architecture

### Deployment model

Keep deployment reversible behind the shared Rust `DriveSource` contract:

1. **Local-first:** each device has its own Telegram authorization and TDLib database. The drive core creates dataless placeholders and hydrates content on demand. This is the v1 preference for macOS, Windows, Linux, and Android.
2. **Remote:** a continuously running gotd/td service holds Telegram authorization and the canonical archive; clients use the product API. It can run on a desktop, NAS/home server, or hosted infrastructure.

The remote model gives consistent cross-device state, Takeout backfill, and reliable iOS cold hydration. The local model removes the infrastructure and credential-custody requirement. A public SaaS operator would possess account-equivalent Telegram authorization keys and plaintext cloud-chat access, requiring a materially stronger security, privacy, abuse, deletion, incident-response, and compliance program.

### Components

- **Telegram session worker:** one logical worker per account; encrypted session state; initial Takeout job; live update loop; flood-wait-aware scheduler.
- **Canonical metadata store:** PostgreSQL for hosted/multi-user or SQLite for single-user/self-hosted. Store stable Telegram IDs, message versions, entities, reply relationships, album/topic IDs, update checkpoints, and chat positions.
- **Blob store:** S3-compatible storage or local content-addressed store, addressed by SHA-256 after download. Keep Telegram remote locator/file-reference metadata separately; deduplicate exposed copies.
- **Renderer:** deterministic Markdown, HTML, and NDJSON generation. Do not treat generated text files as the source of truth.
- **Virtual filesystem API:** stable item IDs, directory enumeration, range downloads, thumbnails, versions, and change journal. This common API backs every native provider.
- **Local TDLib source:** tdjson adapter mapping Telegram dialogs, history, updates, and file downloads into the normalized drive item/change contract.
- **Optional remote source:** HTTP client for a gotd/td service exposing the same normalized contract and range content.
- **Provider-oriented client API:** stable item lookup, child enumeration, durable change cursors, byte-range content, thumbnails, and pin/offline state. Chat-specific endpoints can wait.
- **Shared Rust drive core:** provider-neutral virtual tree, SQLite/cache, hydration, range downloads, restart recovery, retries, naming, and generated-file handling; exposed to Swift/Kotlin with UniFFI.
- **Provider adapters:** Swift File Provider for Apple, Kotlin DocumentsProvider for Android, a Rust CfAPI host for Windows, and a Rust FUSE adapter for Linux.

Detailed revised architecture: `.spec/architecture.md`.

### Suggested directory view

```text
Account Name/
  00 Main/
    000001 — Alice — @alice/
      chat.json
      messages.ndjson
      2026/
        07.md
        media/
          2026-07-15_143208__m12345__report.pdf
    000002 — Project Group/
      ...
  01 Archive/
    ...
  02 Telegram Folders/
    Work/
      ...
```

The numeric prefix is necessary for portable ordering. Finder, Explorer, and Files choose their own sort mode and do not expose Telegram's arbitrary 64-bit chat order. Without a prefix the application cannot guarantee the Telegram sequence. Because chat order changes frequently, offer two modes:

- **Stable names:** no numeric prefixes; exact order only inside the product UI, with `order.json` metadata.
- **Telegram-order names:** numeric prefixes, periodically renamed as chat positions change.

A chat can appear in Main/Archive/custom folder views. The canonical chat is one record; provider paths are virtual appearances that reference the same blobs. Use stable provider item IDs based on account ID, Telegram chat ID, message ID, and attachment index, never on the display filename.

### Chat text format

Use three representations:

- `messages.ndjson`: lossless machine-readable export, one current message/event record per line or an append-only event log with revisions/tombstones.
- `YYYY/MM.md`: human-readable, bounded-size generated files.
- Database index: canonical query model and full-text search.

Include message ID, timestamp/timezone, sender ID/name, edits, reply target, topic, album, text entities, caption, reactions, service actions, and attachment paths. Record future deletions as tombstones if audit mode is enabled, while respecting product policy and user choice. Historical revisions from before first sync cannot be recovered.

### File naming

Do not trust original filenames as unique or filesystem-safe. Use:

`YYYY-MM-DD_HHMMSS__m<MESSAGE_ID>__<sanitized-original-name>`

For photos without original names, generate a stable extension from the MIME type. Retain original name, MIME type, size, Telegram IDs, and SHA-256 in metadata. Sanitize Windows reserved names, separators, control characters, trailing dots/spaces, Unicode normalization, and path length.

### Read-only first

The first provider version should allow enumerate, open, download, pin/offline, and search. It should reject rename, move, edit, create, and delete operations.

Reasons:

- Editing a generated Markdown file has no unambiguous mapping to Telegram messages.
- Moving media between chat folders has no Telegram equivalent.
- Deleting a file could mean cache eviction, removal from the archive, or deletion of the Telegram message for one/all users.
- A provider callback can time out while Telegram reports flood wait or authorization challenges.

Later, add explicit application actions such as “send file to chat”, “delete local cached copy”, and “delete Telegram message” instead of overloading filesystem operations.

## Relevant prior art

- [Teldrive](https://github.com/tgdrive/teldrive) is the most useful service-side reference: active Go code, gotd-based Telegram access, web UI, streaming/file organization, and rclone compatibility under MIT. Its model primarily treats Telegram as backing storage for user-managed files, not as an archival view of every existing chat, so it is not a drop-in solution.
- [tdl](https://github.com/iyear/tdl) demonstrates fast parallel download and JSON export on top of gotd. It is useful for protocol behavior and test scenarios, but AGPL and protected-content behavior make direct product reuse a deliberate licensing/compliance choice.
- [Telegram Archive](https://github.com/GeiserX/Telegram-Archive) demonstrates incremental message/media backup, albums, edits/deletions, deduplication, and a local web viewer. Its cron plus real-time-listener split is a useful synchronization reference. Its Python/Telethon stack is viable, but this project should still be evaluated as prior art rather than adopted wholesale.
- [Telegram Drive](https://github.com/caamer20/Telegram-Drive) demonstrates a Tauri/Rust/React desktop file UI and streaming, but it maps Saved Messages/channels as user-created storage rather than exporting all chat histories, and its documented Telegram engine is grammers, whose GitHub home was archived/moved in 2026.

None of these implements the complete combination of exhaustive chat takeout, continuously updated textual archive, Apple/Android/Windows native file providers, and a full multi-platform client.

## Implementation phases

### Phase 0: shared contracts and local source spike

- Define stable item IDs, versions, paging, changes, ranged content, cancellation, and source errors in Rust.
- Build a deterministic fake source and conformance tests.
- Implement a tdjson local source for one test account and one chat.
- Persist local metadata/cache state and verify edits, deletes, flood waits, file refresh, resumable downloads, restart recovery, and account/session revocation.

Exit criterion: a deterministic re-run produces no duplicate messages/blobs and resumes after interruption.

### Phase 1: macOS and Windows vertical slices

- Complete the shared Rust core: SQLite/cache, resumable hydration, eviction, generated files, and UniFFI bindings.
- macOS File Provider plus companion TDLib host: placeholder enumeration, ranged hydration, pin/offline, and restart recovery.
- Windows Rust CfAPI host over the same fixtures and source contract.

### Phase 2: Android and Linux

- Android DocumentsProvider with TDLib in the application process.
- Linux FUSE adapter over the shared Rust core.
- Keep dataless/on-demand behavior as the default; download only explicitly pinned content eagerly.

### Phase 3: iOS and optional remote source

- Build the iOS containing app plus thin File Provider extension sharing durable App Group state.
- Choose the cold-hydration behavior explicitly: open-app prompt, remote source, or a separately proven lightweight fetch path.
- If required, implement the gotd/td service and `RemoteDriveSource`, with Takeout backfill and HTTP range delivery.
- Keep UI native and minimal; defer rich chat and web clients.

### Phase 4: write operations and hosted service

- Only after read-only semantics, security review, deletion policy, quota/billing, privacy export/deletion, account recovery, and Telegram API compliance are proven.

## Cross-check of the second report (2026-07-15)

A second report on the same question was checked against primary documentation and current repositories. Its raw research streams are retained for traceability:

- `.research/260715-core-libraries.md` — MTProto/TDLib library landscape
- `.research/260715-oss-clients.md` — official/major OSS clients as codebases, licenses, API terms
- `.research/260715-filesystem-integration.md` — OS filesystem/cloud-drive integration technologies
- `.research/260715-prior-art.md` — exporters, mirrors, Telegram-FUSE/WebDAV projects

### Accepted additions

1. **Telethon is not dead.** The archived GitHub repository explicitly points to Codeberg, while the current documentation identifies version 1.44. Its high-level `client.takeout()` proxy also handles `TakeoutInitDelayError`, making it the most convenient option found for a rapid Python export spike. This does not prove that it is the only library capable of Takeout. Sources: https://github.com/LonamiWebs/Telethon, https://docs.telethon.dev/en/stable/modules/client.html#telethon.client.account.AccountMethods.takeout
2. **mtcute belongs in the comparison.** It is an active MIT TypeScript MTProto implementation with explicit Node and web packages. If browser-side MTProto becomes a hard requirement, it is the strongest candidate found; it is still not the recommended architecture because the browser would hold Telegram authorization material and create a second sync engine. Source: https://github.com/mtcute/mtcute
3. **TDLib remains the native-client choice.** Its file state, prioritized/resumable downloads, local database, and explicit chat-list positions match the hard client requirements. It does not provide a documented general Takeout interface. For Swift, TDLibKit is an active MIT wrapper and a useful implementation lead, not an official Telegram component. Sources: https://core.telegram.org/tdlib/docs/, https://github.com/Swiftgram/TDLibKit
4. **Telegram Desktop's export subsystem is valuable prior art.** The module at `Telegram/SourceFiles/export/` separates export orchestration, API access, data types, and output writers. Copying GPL code is a licensing decision, but studying its pipeline and fixtures is useful. Source: https://github.com/telegramdesktop/tdesktop/tree/dev/Telegram/SourceFiles/export
5. **Operational pacing belongs in the design.** Telegram states that applications using unofficial API clients are monitored for abuse; the sync scheduler therefore needs bounded concurrency, flood-wait handling, backoff, and resumable jobs. `TAKEOUT_INIT_DELAY_X` must be surfaced in onboarding, but the official schema supplies a number of seconds and does not establish a fixed 24-hour maximum. Sources: https://core.telegram.org/api/obtaining_api_id, https://core.telegram.org/api/takeout
6. **Useful provider references were identified.** TDLibKit is relevant on Swift; Nextcloud's File Provider code can inform the Apple extension; OneDriver demonstrates Go/FUSE hydration and caching on Linux; Rust and C# CfAPI projects are implementation leads for Windows. Absence of a mature Go CfAPI wrapper was not proven exhaustively, so this remains a spike question rather than an architectural fact.
7. **The prior-art and naming findings are useful.** Telegram Archive's batch plus live-listener approach, `tdl`'s download patterns, and Teldrive's Go service structure deserve study. The working name `tgfs` conflicts with the existing `TheodoreKrypton/tgfs`, which exposes Telegram-backed storage through WebDAV. Source: https://github.com/TheodoreKrypton/tgfs
8. **The iOS File Provider limit is sufficiently verified for architecture.** An accepted response from an Apple engineer states that File Provider extension processes have a 20 MB memory limit on iOS. Treat this as a current platform constraint and keep the extension streaming and thin; re-measure on every supported iOS release rather than assuming all Apple extensions or future versions use the same limit. Source: https://developer.apple.com/forums/thread/739839

### Claims deliberately not adopted as facts

- “Forking GPL clients is impossible for a commercial product.” GPL permits commercial distribution but imposes source and copyleft obligations; these clients are unsuitable for a proprietary fork unless the product has a compatible licensing plan. Telegram-iOS's top-level repository does not display a license file, but its README explicitly tells derivative-app developers to publish their code to comply with the component licences. That needs legal review, not the label “legally toxic.” Source: https://github.com/TelegramMessenger/Telegram-iOS
- A universal **20 MB limit across Apple platforms/extensions**. The verified statement is specifically about File Provider extension processes on iOS; no equivalent macOS limit was established.
- A guaranteed **24-hour** Takeout delay, a universal requirement for Windows adapters to use MSIX, “four methods” as a complete Android provider implementation, and “every Telegram-FUSE project is abandoned.” These are either context-dependent, implementation shorthand, or overly broad absence claims.
- The hosted Bot API's file-download limit is not the deciding issue. Bot authorization already fails the core requirement because a bot cannot enumerate the user's account-wide dialog history; a local Bot API server also has different file limits.

## Decisions still required

1. Self-hosted/personal server only, hosted SaaS, or both.
2. Whether “web Telegram analog” means full messaging parity or read/search/export only.
3. Whether secret chats are out of scope. They cannot be reconstructed globally like cloud chats.
4. Whether exact Telegram list order is worth frequent folder renames, or ordering should remain exact only in the app UI.
5. Whether generated exports preserve later deletions/edits as an audit log or mirror the current Telegram state.
6. Maximum media size, storage quota, and eager-versus-on-demand policy.
7. Required open-source licensing model; reusing Telegram Desktop/Web/tdl code has GPL/AGPL consequences.
