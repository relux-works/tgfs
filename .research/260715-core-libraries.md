# Research: Core Telegram Client Libraries (MTProto + TDLib)

Research stream 01 for the tgfs project (web + mobile + desktop app: login to user's Telegram account, download ALL files/media from all chats, export chat history as text, folders named after chats in dialog-list order).

- Date of research: **2026-07-15**
- Method: primary sources only (GitHub/Codeberg repos, GitHub API, npm registry, PyPI, core.telegram.org, official docs), verified as of mid-2026.

---

## TL;DR / Landscape shift in 2025–2026

The ecosystem moved noticeably since 2024:

| Event | Date | Source |
|---|---|---|
| Pyrogram (original) archived on GitHub | last push 2024-12-23, archived | https://api.github.com/repos/pyrogram/pyrogram |
| grammers moved GitHub -> Codeberg (GitHub repo archived) | 2026-02-10 | https://github.com/Lonami/grammers |
| Telethon moved GitHub -> Codeberg (GitHub repo archived) | 2026-02-21 | https://github.com/LonamiWebs/Telethon |
| GramJS archived, points to fork "Teleproto" | 2026-07-14 (!) | https://github.com/gram-js/gramjs |

TDLib, gotd/td, mtcute, WTelegramClient, Kurigram, MadelineProto are all actively maintained as of July 2026.

---

## 1. TDLib (official, C++) — `tdlib/td`

- Repo: https://github.com/tdlib/td
- **License:** BSL-1.0 (Boost Software License 1.0) — very permissive, commercial-friendly. Source: https://api.github.com/repos/tdlib/td and README ("TDLib is licensed under the terms of the Boost Software License").
- **Activity:** last push **2026-07-14** (i.e., day before this research), not archived, ~8,940 stars, 74 open issues. Source: https://api.github.com/repos/tdlib/td
- **Version / release cadence:** rolling development on `master`. Current `project(TDLib VERSION 1.8.66 ...)` in CMakeLists (source: https://raw.githubusercontent.com/tdlib/td/master/CMakeLists.txt). Last *git tag* is `v1.8.0` (source: https://api.github.com/repos/tdlib/td/tags) — TDLib effectively stopped tagging releases years ago; consumers pin commits or rely on binding projects that ship prebuilt versions (e.g., TDLibKit ships 1.8.65 builds). Practical implication: no semver releases, but continuous layer updates directly from Telegram.
- **Platforms (official README):** "Android, iOS, Windows, macOS, Linux, FreeBSD, OpenBSD, NetBSD, illumos, Windows Phone, WebAssembly, watchOS, tvOS, visionOS, Tizen, Cygwin". Source: https://github.com/tdlib/td (README).
- **Interfaces:** JSON interface (`td_json_client`, callable from any language with C FFI), native C++, Java (JNI), .NET (C++/CLI, C++/CX). Source: https://github.com/tdlib/td (README).
- **Bindings ecosystem** (from official list https://github.com/tdlib/td/blob/master/example/README.md):
  - **Web/WASM:** `tdweb` — official wrapper for browsers ("TDLib can be compiled to WebAssembly and used in a browser from JavaScript"; `telegram-react` is the sample client). **Caveat:** the `tdweb` npm package is stale — latest 1.8.0 published **2021-12-30** (source: https://registry.npmjs.org/tdweb). For a current browser build you must compile TDLib to WASM from source (`example/web` in the repo). Note: the actual Telegram Web A client does NOT use tdweb — it uses a custom GramJS (see GramJS section).
  - **Node.js:** `tdl` (https://github.com/eilvelia/tdl) — MIT, last push **2026-06-13**, prebuilt binaries, supports Node/Bun/Deno. Good fit for Electron desktop.
  - **Swift:** `TDLibKit` (https://github.com/Swiftgram/TDLibKit) — MIT, release `1.5.2-tdlib-1.8.65` on **2026-07-14**, wraps TDLib 1.8.65 with prebuilt TDLibFramework for **iOS, macOS, watchOS, tvOS, visionOS**, async/await API.
  - **Kotlin/JVM:** `td-ktx`, `ktd`, `tdl-coroutines`; official Java JNI example + official Android example in-repo.
  - **Python:** `python-telegram`, `aiotdlib`, `Pytdbot`, etc.
  - **Go:** `zelenin/go-tdlib`, `Arman92/go-tdlib`, etc.
  - **Rust:** `rust-tdlib`, `tdlib-rs`, `rtdlib`.
  - **Dart/Flutter:** `tdlib-dart`, `flutter_libtdjson`, `telegram-flutter` sample.
  - **C#/.NET:** `tdsharp`, `tdlib-netcore`; Unigram (official-ish Windows client) is TDLib-based.
- **Suitability for full-history + media download:**
  - Dialog list: `getChats(chat_list, limit)` returns chats in **correct server-side order** per chat list (Main, Archive, and folders via `chatListFolder`); pinned chats are handled by TDLib's ordering (chat `positions`). TDLib maintains ordering for you — this is exactly the "same order as the Telegram dialog list" requirement. Source: https://core.telegram.org/tdlib/docs/ (td_api: getChats, chatListMain/chatListArchive/chatListFolder).
  - History: `getChatHistory` with `from_message_id` pagination; TDLib caches into its own local database (SQLite), which speeds re-iteration.
  - Media: `downloadFile(file_id, priority 1–32, offset, limit, synchronous)` — supports **partial/resumable downloads** (offset/limit) and **prioritized parallel downloads**; file state persists in TDLib's database across restarts. Source: https://core.telegram.org/tdlib/docs/classtd_1_1td__api_1_1download_file.html ("Priority of the download (1-32)... The starting position from which the file needs to be downloaded").
  - FLOOD_WAIT: TDLib handles network-level retries and queues requests internally; explicit 420 handling is largely abstracted away from the app.
  - **Takeout: NOT exposed.** The raw MTProto takeout methods (`account.initTakeoutSession`, `invokeWithTakeout`) are not part of TDLib's `td_api` interface; a bare feature-request issue was closed without implementation (source: https://github.com/tdlib/td/issues/3031, closed, opened 2024-08-22). Mass export through TDLib runs under normal rate limits.
- **Verdict:** the most "official", most durable choice; heaviest integration cost (native lib per platform), no takeout, but best-in-class file download manager and ordering semantics for free.

---

## 2. Telethon (Python)

- Active home: **https://codeberg.org/Lonami/Telethon** (GitHub https://github.com/LonamiWebs/Telethon archived **2026-02-21** with banner "Moved to https://codeberg.org/Lonami/Telethon. The GitHub repository may be deleted in the future.").
- **License:** MIT. Sources: https://api.github.com/repos/LonamiWebs/Telethon, https://pypi.org/pypi/Telethon/json.
- **Activity / v2 status:** actively maintained **in maintenance mode**: latest commit **2026-07-11** ("Update to layer 228"), **v1.44** released 2026-06-15 (PyPI latest: 1.44.0). README: "Telethon v1 is for the most part in maintenance mode. New layers are still updated to when released, bug fixes are welcome...". **v2 is effectively stalled** — a 2.0.0a alpha exists in docs (https://docs.telethon.dev/en/v2/developing/changelog.html) but no v2 branch is visible on the active repo and no v2 release shipped as of July 2026. Sources: https://codeberg.org/Lonami/Telethon, https://github.com/LonamiWebs/Telethon/issues/4482 (V2 Roadmap).
- **Takeout support: first-class, best in the ecosystem.** `client.takeout(contacts=..., users=..., chats=..., megagroups=..., channels=..., files=..., max_file_size=...)` context manager wraps everything in `invokeWithTakeout`; docs: "Some of the calls made through the takeout session will have lower flood limits"; `end_takeout()` finishes the session; raises `TakeoutInitDelayError` (maps to `TAKEOUT_INIT_DELAY_X`). Source: https://docs.telethon.dev/en/stable/modules/client.html
- **Dialogs:** `iter_dialogs()` returns dialogs "first pinned, then from those with the most recent message to those with the oldest message" — i.e., exact Telegram dialog-list order; `folder` parameter (0 = main, 1 = archive). Source: https://docs.telethon.dev/en/stable/modules/client.html
- **History / media:** `iter_messages()` (full history iteration with offsets), `download_media()` (supports progress callbacks; chunked download). FLOOD_WAIT: automatic sleep via `flood_sleep_threshold` ("if a FloodWaitError for 17s occurs and flood_sleep_threshold is 20s, the library will sleep automatically"). Source: https://docs.telethon.dev/en/stable/modules/client.html, https://docs.telethon.dev/en/stable/concepts/errors.html
- **Platforms:** Python only — backend/CLI/desktop-with-embedded-Python; not usable in browser or native mobile.
- **Verdict:** best takeout/export ergonomics anywhere; healthy but conservative maintenance; Python-only limits it to a backend/worker role in a multi-platform product. Note repo migration to Codeberg for dependency pinning/CI.

---

## 3. GramJS (JS/TS) — archived July 2026

- Repo: https://github.com/gram-js/gramjs — **archived 2026-07-14** ("This project is archived and no longer maintained"), MIT, npm package `telegram` latest **2.26.22**, npm shows deprecation notice: "Development continues in teleproto..., a largely compatible, actively maintained fork" (source: https://registry.npmjs.org/telegram).
- Browser + Node support; was the go-to JS MTProto lib; powers **Telegram Web A** — but Web A uses "a custom version of GramJS" vendored in-repo (source: https://github.com/Ajaxy/telegram-tt README), so Web A is unaffected by the archive; the vendored copy is maintained by the Web A team, not published as a library.
- **Successor — Teleproto:** https://github.com/sanyok12345/teleproto — MIT, standalone repo (not a GitHub fork object), created 2025-05-15, last push **2026-07-14**, 312 stars; npm `teleproto` latest **1.228.1** published 2026-07-14 (version tracks TL layer 228). Migration guide: https://docs.teleproto.dev/migrating-from-gramjs. Source: https://api.github.com/repos/sanyok12345/teleproto, https://registry.npmjs.org/teleproto
- **Caveat:** Teleproto is young (14 months old, small community, single lead maintainer); the archive+redirect happened literally the day before this research — treat as "compatible escape hatch", not a proven foundation.
- **Verdict:** do not start a new project on GramJS. If GramJS-style API is desired in JS, evaluate Teleproto vs mtcute; mtcute currently looks healthier as an original codebase.

---

## 4. mtcute (TypeScript)

- Repo: https://github.com/mtcute/mtcute — MIT, **not archived**, last push **2026-07-05**, 523 stars. Latest `@mtcute/node` **0.30.3** (source: https://registry.npmjs.org/@mtcute/node).
- **Runtimes:** Node (`@mtcute/node`), **browsers** (`@mtcute/web`, IndexedDB storage), Bun (`@mtcute/bun`), Deno (`jsr:@mtcute/deno`), "any environment that supports some basic ES2020 features"; runtime-agnostic with pluggable networking/storage. Sources: https://github.com/mtcute/mtcute, https://mtcute.dev/guide/
- **Quality:** strictly typed (including generated MTProto TL), tree-shakeable monorepo, up-to-date TL schema (layer badge), lightweight. Still **0.x** — no 1.0 stability guarantee; smaller community than Telethon/GramJS had.
- **Features relevant to us:**
  - Dialogs: `iterDialogs()` (AsyncIterableIterator) with `pinned: "include" | "exclude" | "only" | "keep"`, `archived`, `folder` params — covers pinned + folders ordering. Source: https://ref.mtcute.dev/funcs/_mtcute_web.methods.iterDialogs
  - History: `iterHistory()` with chunked pagination. Source: https://ref.mtcute.dev/
  - Media: `downloadToFile` (Node), `downloadStream`, `downloadBuffer`, `downloadIterable`. Source: https://mtcute.dev/guide/topics/files.html (parallelism/resume not documented as built-ins — offset-based re-request possible at raw API level).
  - FLOOD_WAIT: "mtcute automatically handles flood waits smaller than `floodSleepThreshold` by sleeping"; per-call `RpcCallOptions.floodSleepThreshold`. Sources: https://mtcute.dev/guide/intro/errors, https://jsr.io/@mtcute/core/doc/~/RpcCallOptions.floodSleepThreshold
  - Takeout: no first-class takeout helper found in docs; raw `account.initTakeoutSession`/`invokeWithTakeout` calls are possible via the typed raw API (`tg.call({ _: 'account.initTakeoutSession', ... })`) but session wrapping is DIY.
- **Verdict:** the strongest *actively-developed* pure-TS MTProto option in mid-2026; one codebase covers browser + Node/Electron + React Native (with adapters). Risk: 0.x maturity, small bus factor (primary author teidesu).

---

## 5. gotd/td (Go)

- Repo: https://github.com/gotd/td — MIT, **not archived**, last push **2026-07-14**, 2,285 stars, latest release **v0.161.0 (2026-07-14)**. Source: https://api.github.com/repos/gotd/td, README.
- **Quality:** README declares the project **stable**; "extensive testing: end-to-end with real servers, unit tests, and fuzzing"; auto-generated from official schema; performance-focused (~150KB per idle client); aims for "feature parity with TDLib".
- **User accounts:** yes — `auth.Flow` with 2FA, plus bot auth.
- **Relevant helpers:** upload/download helpers **with CDN support**; pagination "query helpers" for messages/dialogs iteration (`telegram/query`); **WebSocket transport for WASM** (Go-in-browser is possible but heavy). Source: https://github.com/gotd/td README.
- **FLOOD_WAIT:** `gotd/contrib/middleware/floodwait` — "Catches Telegram FLOOD_WAIT errors and retries transparently" (`Waiter` for daemons, `SimpleWaiter` for scripts) + `middleware/ratelimit` token-bucket pacing. Sources: https://github.com/gotd/contrib (latest v0.24.0, 2026-06-15), https://pkg.go.dev/github.com/gotd/contrib/middleware/floodwait
- **Takeout:** no high-level takeout helper; raw `account.InitTakeoutSession` + manual `invokeWithTakeout` wrapping via generated API is possible.
- **Ordering:** dialogs iteration is raw `messages.getDialogs`-based (returns server order incl. pinned; folders via `folder_id`); more manual assembly than TDLib/Telethon.
- **Verdict:** excellent for a **backend/server-side export worker** or a single-binary desktop CLI; lower-level than Telethon (more code for the same task); not a fit for browser/mobile UI layers.

---

## 6. grammers (Rust)

- Active home: **https://codeberg.org/Lonami/grammers** (GitHub https://github.com/Lonami/grammers archived **2026-02-10**, "Moved to https://codeberg.org/Lonami/grammers").
- **License:** dual **Apache-2.0 OR MIT**. Source: https://codeberg.org/Lonami/grammers (GitHub API had reported Apache-2.0; Codeberg README states dual "at your option").
- **Activity:** alive — last commit **2026-07-13** (retry-policy customization), **v0.10.0** bumped 2026-07-02.
- **Maturity:** README: "It works! The high-level interface is slowly taking shape, and it can already be used to build real projects" — and explicitly notes the crypto/auth code has "not been audited". High-level API is thinner than Telethon's (same author, much less coverage: no takeout helper, more manual iteration).
- **Verdict:** usable but pre-1.0 and intentionally minimal; pick only if the stack is Rust-first. Same single-maintainer (Lonami) risk as Telethon, spread across two projects.

---

## 7. Pyrogram and forks — who is alive in 2026?

- **Original `pyrogram/pyrogram`: dead.** Archived; last push **2024-12-23**. LGPL-3.0. Source: https://api.github.com/repos/pyrogram/pyrogram
- **Kurigram (`KurimuzonAkuma/pyrogram`): the live fork.** "Actively maintained pyrogram fork... designed as a drop-in replacement for Pyrogram"; last push **2026-07-14**; PyPI `kurigram` **2.2.24** released **2026-07-11**; Python 3.8–3.14; LGPL-3.0-or-later; supports current Telegram features (Gifts, Stories, Topics, Business). Sources: https://api.github.com/repos/KurimuzonAkuma/pyrogram, https://pypi.org/project/kurigram/
- **PyroFork (`Mayuri-Chan/pyrofork`): also alive** but smaller — last push 2026-07-01, 289 stars, LGPL-3.0. Source: https://api.github.com/repos/Mayuri-Chan/pyrofork
- Feature notes: Pyrogram-family has `get_dialogs`/`get_chat_history`/`download_media` with FloodWait sleep, but **no first-class takeout wrapper** (raw invoke possible). **License warning:** LGPL-3.0 is copyleft-ish — fine for a backend service, but more friction than MIT (Telethon) for embedded distribution.
- **Verdict:** if Pyrogram API style is required, use **Kurigram**. Otherwise Telethon is the better-supported Python option for this specific export use case (takeout).

---

## 8. WTelegramClient (.NET)

- Repo: https://github.com/wiz0u/WTelegramClient — MIT, **not archived**, last push **2026-07-08**, 1,306 stars. "Telegram Client API (MTProto) library written 100% in C# and .NET".
- **Platforms:** .NET 5.0+ (best), .NET Standard 2.0 (Framework 4.6.1+/Core 2.0+), Xamarin/Android — i.e., desktop Windows/macOS/Linux + MAUI mobile possible.
- **User accounts:** yes (phone login, 2FA, session persistence); full raw Client API surface; file upload/download helpers; even secret chats.
- **FLOOD_WAIT:** "For FLOOD_WAIT_X with X < 60 seconds (see `client.FloodRetryThreshold`), WTelegramClient will automatically wait the specified delay and retry the request for you." Source: https://wiz0u.github.io/WTelegramClient/FAQ
- **Takeout:** raw API access means `Account_InitTakeoutSession` + `InvokeWithTakeout` are callable; no dedicated convenience wrapper documented.
- **Risk:** effectively a **single-maintainer** project (wiz0u), donation-funded.
- **Verdict:** solid choice iff the product is .NET/MAUI-centric; otherwise skip.

---

## 9. MadelineProto (PHP) — brief

- Repo: https://github.com/danog/MadelineProto — **AGPL-3.0**, active (last push 2026-06-26; latest release **8.6.5 "Layer 225"**, 2026-04-20), 3,458 stars. Sources: https://api.github.com/repos/danog/MadelineProto, https://api.github.com/repos/danog/MadelineProto/releases/latest
- Async PHP, user + bot accounts, has flood-wait docs (https://docs.madelineproto.xyz/docs/FLOOD_WAIT.html). **AGPL-3.0 is a hard blocker** for most commercial/closed-source products, and PHP doesn't fit web-client/mobile/desktop distribution. **Verdict: not relevant for this project.**

---

## Cross-cutting: Bot API vs MTProto user API

Bot API is **not viable** for this product:

- **History access:** bots only receive messages via updates (getUpdates/webhook); they **cannot read arbitrary/pre-existing chat history**, and cannot enumerate a user's dialogs at all. Source: https://core.telegram.org/bots/api (Message/getUpdates semantics), https://core.telegram.org/bots/faq
- **Download limit:** "Use the getFile method. Please note that this will only work with files of up to **20 MB** in size." Upload limit **50 MB** ("Bots can currently send files of any type of up to 50 MB in size"). Source: https://core.telegram.org/bots/faq
- A **local Bot API server** removes the download limit and raises upload to 2000 MB — but the history-access limitation remains fatal. Source: https://core.telegram.org/bots/api ("If you switch to a local Bot API server, your bot will be able to download files without a size limit").
- Bot rate limits: 1 msg/s per chat, 20 msg/min per group, ~30 msg/s broadcast. Source: https://core.telegram.org/bots/faq

=> The product must log in as the **user** via **MTProto** (TDLib or an MTProto library).

## Cross-cutting: Telegram takeout (official bulk-export mechanism)

- `account.initTakeoutSession` flags: `contacts`, `message_users`, `message_chats`, `message_megagroups`, `message_channels`, `files` + `file_max_size`; every subsequent query **must be wrapped in `invokeWithTakeout`** with the returned id; finish via `account.finishTakeoutSession(success)`. Source: https://core.telegram.org/api/takeout
- Security delay: error **420 `TAKEOUT_INIT_DELAY_%d`** — "for security reasons, you will be able to begin downloading your data in %d seconds. We have notified all your devices about the export request..." (typically ~24h for fresh sessions; all devices get a notification). Source: https://core.telegram.org/method/account.initTakeoutSession
- Benefit: takeout sessions get **relaxed flood limits** for bulk operations (Telethon docs: "Some of the calls made through the takeout session will have lower flood limits"). Source: https://docs.telethon.dev/en/stable/modules/client.html
- Library support: **Telethon = first-class** (`client.takeout()`); mtcute/gotd/WTelegramClient/Kurigram = raw-API DIY; **TDLib = not exposed at all** (https://github.com/tdlib/td/issues/3031).

## Cross-cutting: api_id/api_hash + ToS considerations for mass downloading

- Every third-party client needs `api_id`/`api_hash` from https://my.telegram.org ("API development tools"); "Each number can only have one api_id connected to it"; sample/public api_ids are server-side limited (`API_ID_PUBLISHED_FLOOD`). Source: https://core.telegram.org/api/obtaining_api_id
- ToS: "flooding, spamming, faking subscriber and view counters" => permanent ban; "**all accounts that sign up or log in using unofficial Telegram clients are automatically put under observation**"; wrongful-ban recovery via recover@telegram.org. Sources: https://core.telegram.org/api/obtaining_api_id, https://wiz0u.github.io/WTelegramClient/FAQ
- Practical mass-download hygiene: use takeout where possible, respect FLOOD_WAIT (auto-sleep thresholds), pace requests (token-bucket like gotd's `ratelimit`), avoid hammering `messages.getHistory` across hundreds of chats in parallel, cap media parallelism per DC.

---

## Comparison matrix (mid-2026)

| Library | Lang | License | Alive (last activity) | Browser | Mobile | Desktop | User acct | Dialog order (pinned/folders) | History iter | Media DL (resume/parallel) | Takeout | FLOOD_WAIT auto |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| **TDLib** | C++ | BSL-1.0 | Yes (2026-07-14) | WASM (build from src; tdweb npm stale) | iOS/Android native | all | yes | yes (getChats per chat list) | yes | **yes: offset/limit resume, priority, persistent** | **no** | internal |
| **Telethon** | Python | MIT | Yes, maintenance (Codeberg, 2026-07-11, v1.44) | no | no | backend | yes | yes (iter_dialogs, folder param) | yes | yes (chunked; no built-in parallel per file) | **yes, first-class** | yes (flood_sleep_threshold) |
| **GramJS** | TS | MIT | **Archived 2026-07-14** → Teleproto | yes | RN possible | Node/Electron | yes | partial | yes | yes | raw | yes |
| **Teleproto** | TS | MIT | Yes (2026-07-14, 1.228.1) | yes | RN possible | Node/Electron | yes | GramJS-compatible | yes | yes | raw | yes |
| **mtcute** | TS | MIT | Yes (2026-07-05, 0.30.x) | **yes (@mtcute/web)** | RN w/ adapters | Node/Bun/Electron | yes | yes (iterDialogs: pinned/archived/folder) | yes (iterHistory) | yes (stream/file/iterable) | raw | yes (floodSleepThreshold) |
| **gotd/td** | Go | MIT | Yes (v0.161.0, 2026-07-14) | WASM (heavy) | no | backend/CLI | yes | manual (query helpers) | yes | yes + CDN | raw | contrib floodwait+ratelimit |
| **grammers** | Rust | Apache-2.0/MIT | Yes (Codeberg, 2026-07-13, v0.10) | no | no | backend/CLI | yes | manual | yes | yes (basic) | raw | manual/retry policy |
| **Kurigram** | Python | LGPL-3.0 | Yes (2.2.24, 2026-07-11) | no | no | backend | yes | yes | yes | yes | raw | yes |
| **WTelegramClient** | C# | MIT | Yes (2026-07-08) | no (Blazor untested) | MAUI/Xamarin | .NET | yes | manual | yes | yes | raw | yes (<60s auto) |
| **MadelineProto** | PHP | **AGPL-3.0** | Yes (8.6.5) | no | no | backend | yes | yes | yes | yes | raw | yes |

---

## Recommendation for tgfs (web + mobile + desktop)

**Primary: TDLib** as the core engine for the *apps* (mobile + desktop, optionally web):
- Official, permissive BSL-1.0, updated same-week as research; covers iOS/Android/macOS/Windows/Linux/WASM from one codebase; bindings are healthy (TDLibKit for Swift updated 2026-07-14; tdl for Node/Electron updated 2026-06; official Android/Java).
- The only library with a production-grade **download manager** (priorities, offset-resume, persistence) and server-order dialog lists (incl. pinned + folders) out of the box — the two hardest requirements of this product.
- Cost: native builds per platform; no takeout support (normal rate limits apply; must pace requests).

**Secondary: mtcute** for the **web app** (and to share TS code with an Electron desktop): only actively-developed original TS MTProto stack with first-class browser support (`@mtcute/web`), `iterDialogs` (pinned/folder-aware), `iterHistory`, streaming downloads, auto flood-sleep. Risk: 0.x. Fallback in TS-land: Teleproto (GramJS-compatible) — too young to bet on alone.

**Tertiary: Telethon (Codeberg)** for a **server-side export worker** if a backend-download architecture is chosen: the only first-class `takeout()` implementation (relaxed flood limits + official export semantics), MIT, still maintained. gotd/td is the alternative if the backend is Go and throughput matters (floodwait+ratelimit middleware, CDN downloads).

**Avoid:** GramJS (archived), original Pyrogram (archived; use Kurigram if Pyrogram-style is ever needed), MadelineProto (AGPL + PHP).

**Architecture note:** takeout (`TAKEOUT_INIT_DELAY`, device notifications, relaxed limits) is only practical in long-lived server/desktop sessions; a browser-only client downloading terabytes via WASM is limited by storage APIs — plan for a native/desktop or backend download path for the "ALL files" promise, with the web app as viewer/orchestrator.
