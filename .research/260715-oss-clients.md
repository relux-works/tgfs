# Research Stream 02: Official/Major OSS Telegram Clients as Reusable Codebases

- **Date:** 2026-07-15
- **Project context:** tgfs — app (web + mobile + desktop) that logs into a user's Telegram account, downloads ALL files/media from all chats, exports chat history as text, organized into folders named after chats (mirroring the dialog list order).
- **Scope of this stream:** official and major third-party Telegram clients — tech stacks, licenses, forkability, and the Telegram Desktop export feature as prior art. Verified as of mid-July 2026 (GitHub API metadata pulled live on 2026-07-15).

---

## 1. Telegram Web A (`Ajaxy/telegram-tt`)

- **Repo:** https://github.com/Ajaxy/telegram-tt
- **License:** GPL-3.0 (GitHub API SPDX: `GPL-3.0`; also listed as "GNU GPL v. 3" on https://telegram.org/apps)
- **Status (2026-07):** actively maintained — latest release v2.10.3 (May 2026), pushed 2026-07-01, ~3.5k stars, 97 releases.
- **Tech stack:**
  - TypeScript (~71% of codebase)
  - **NOT React** — a custom in-house framework called **Teact** that reimplements the React paradigm (hooks, VDOM) with zero dependency on React itself.
  - **MTProto layer: a customized/vendored fork of GramJS** (the MIT-licensed JS MTProto library, https://github.com/gram-js/gramjs) living inside the repo.
  - Web Workers, WebAssembly (rlottie, crypto), WebSockets, PWA, Vite + npm build.
- **API credentials:** requires your own `api_id`/`api_hash` from my.telegram.org, set via `.env`.
- **Forkability assessment:**
  - Technically the most approachable official codebase for a web product: pure TS, standard npm/Vite toolchain, runs anywhere a browser runs.
  - The GramJS-derived MTProto/auth/file-download layer is reasonably separable from the Teact UI (lives under `src/lib/gramjs`), but the state/store layer ("global" store, actions/reducers) is intertwined with UI expectations.
  - Note: upstream GramJS itself is **MIT** (verified via GitHub API 2026-07-15: `MIT`, pushed 2026-07-14, ~1.8k stars) — meaning the *library* can be used in a proprietary product directly; it's the telegram-tt *application code* that is GPL-3.0.
- **Sources:** https://github.com/Ajaxy/telegram-tt , https://github.com/gram-js/gramjs , https://telegram.org/apps

## 2. Telegram Web K (`morethanwords/tweb`)

- **Repo:** https://github.com/morethanwords/tweb
- **License:** GPL-3.0 (GitHub API SPDX: `GPL-3.0`; "GNU GPL v. 3" on https://telegram.org/apps)
- **Status (2026-07):** actively maintained — pushed 2026-07-14, ~2.6k stars, ~5k commits.
- **Tech stack:**
  - TypeScript (~91%), SCSS.
  - UI on **Solid.js** (migrated progressively; historical Webogram lineage).
  - **Own MTProto implementation in TypeScript** (in-repo, descended from Webogram's mtproto code — no GramJS).
  - Vite build, pnpm, Docker support for dev and prod; runs as PWA/SPA at web.telegram.org/k.
- **Forkability assessment:** similar profile to Web A — self-contained web client with in-repo MTProto. The MTProto layer (`src/lib/mtproto`) uses workers and is fairly modular, but is a bespoke implementation maintained for this one app; GramJS (used by Web A) has a much bigger standalone ecosystem.
- **Sources:** https://github.com/morethanwords/tweb , https://telegram.org/apps

## 3. Telegram Desktop (`telegramdesktop/tdesktop`) — and its Export feature (prior art)

- **Repo:** https://github.com/telegramdesktop/tdesktop
- **License:** **GPL v3 with OpenSSL linking exception**. LICENSE file (quoted from https://raw.githubusercontent.com/telegramdesktop/tdesktop/dev/LICENSE): "Telegram Desktop is licensed under the GNU General Public License version 3 with the addition of the following special exception: … permission to link the code of portions of this program with the OpenSSL library." (GitHub API reports SPDX `NOASSERTION` because of the custom exception.)
- **Status (2026-07):** very active — 853 releases, latest v7.0.1, pushed 2026-07-14, ~32.3k stars, ~24.6k commits.
- **Tech stack:** C++ (~97.5%), Qt 6 / Qt 5.15 (patched), Objective-C++ for macOS glue, CMake + Ninja, Docker-based Linux builds. Own C++ MTProto implementation (not TDLib).

### 3a. Built-in "Export Telegram Data" — direct prior art for tgfs

- Announced in the official blog "Export and More" (2018-08-27): https://telegram.org/blog/export-and-more — "export some (or all) of your chats, including photos and other media they contain … you'll get all your data accessible offline in JSON-format or in beautifully formatted HTML."
- Two entry points: full-account export (Settings → Advanced → Export Telegram Data) and single-chat export (⋮ menu in a chat).
- **Implementation lives in** `Telegram/SourceFiles/export/` (verified file listing 2026-07-15):
  - `export_controller.cpp/h`, `export_manager.cpp/h` — orchestration/state machine
  - `export_api_wrap.cpp/h` — the API request layer (uses the **takeout API**, see §10)
  - `export_settings.cpp/h` — user-selected scope (data types, chats, size caps, format)
  - `data/` — `export_data_types.cpp` etc.: mapping TL objects to export records
  - `output/` — pluggable format writers (verified): `export_output_html.cpp`, `export_output_json.cpp`, **`export_output_html_and_json.cpp`** (both at once), `export_output_file.cpp`, `export_output_stats.cpp`, `export_output_abstract.h`
- **What it exports:** personal info, contact list, sessions, all selected chats (private/groups/channels) with messages, and media subject to a per-file size cap chosen by the user.
- **Media folder layout** (verified in `export/data/export_data_types.cpp`, 2026-07-15): media is written into per-type subfolders next to `result.json` / `messages.html`:
  - `photos/`, `video_files/`, `voice_messages/`, `round_video_messages/`, `stickers/`, `profile_pictures/`, plus generic `files/` for other documents.
  - Source: https://github.com/telegramdesktop/tdesktop/blob/dev/Telegram/SourceFiles/export/data/export_data_types.cpp (folder-name switch around line ~1275).
- **Key architectural takeaways for tgfs:**
  1. tdesktop organizes exported media **by media type**, not by chat-per-folder — our chat-per-folder layout is a differentiator.
  2. The whole export pipeline is behind the takeout API session (rate-limit-friendly bulk access) — we should do the same.
  3. The `export/` module is cleanly layered (controller → api_wrap → data → output writers) and is the best reference implementation of "download everything reliably with resume/stats" — worth reading even if we never write C++.
- **Sources:** https://github.com/telegramdesktop/tdesktop , https://telegram.org/blog/export-and-more , https://github.com/telegramdesktop/tdesktop/tree/dev/Telegram/SourceFiles/export

## 4. Telegram iOS (`TelegramMessenger/Telegram-iOS`)

- **Repo:** https://github.com/TelegramMessenger/Telegram-iOS
- **License:** **legally murky.** GitHub API reports `license: None`; verified 2026-07-15 that the repo root contains **no LICENSE file** (only README, build files, submodules). https://telegram.org/apps states "GNU GPL v. 2 or later", but that declaration lives only on telegram.org, not in the repo. Community issues flag exactly this gap: https://github.com/TelegramMessenger/Telegram-iOS/issues/97 ("Is this actually GPL?") and https://github.com/TelegramMessenger/Telegram-iOS/issues/336 (GPL violation complaint). README does say forks must "publish your code too in order to comply with the licences."
- **Status (2026-07):** pushed 2026-06-09, ~8.7k stars; GitHub Releases lag far behind App Store versions (latest tagged release 10.0.3, Sep 2023) but the code itself keeps moving.
- **Tech stack:** Swift (~45%) + C (~42%) + ObjC/C++/asm; **Bazel** build (`MODULE.bazel`, `build-system/Make/Make.py`); **265 submodules** (verified count via GitHub API).
- **MTProto/core:** yes — own stack, all in-repo as Bazel modules (verified in `submodules/`): **`MtProtoKit`** (MTProto transport/crypto), **`TelegramApi`** (TL schema layer), **`TelegramCore`** (sync engine, account state), **`Postbox`** (message DB/storage). This is the reusable sync/download layer; UI lives in `TelegramUI` and friends.
- **Reuse feasibility:** the core stack (MtProtoKit + TelegramApi + Postbox + TelegramCore) is a real, production-grade Swift MTProto client and is architecturally separable (they are distinct Bazel targets). But: Bazel monorepo friction, no license file (forking is legally risky — the GPL grant exists only as a website statement), and GPLv2 obligations if you do rely on it. Best treated as **reference architecture**, not a dependency.
- **Sources:** https://github.com/TelegramMessenger/Telegram-iOS , https://telegram.org/apps , issues #97/#336 above

## 5. Telegram Android (`DrKLO/Telegram`)

- **Repo:** https://github.com/DrKLO/Telegram
- **License:** GPL-2.0 (GitHub API SPDX: `GPL-2.0`; telegram.org/apps: "GNU GPL v. 2 or later")
- **Status (2026-07):** pushed 2026-06-16, ~29.5k stars; latest GitHub release 11.4.2 (Nov 2024) — releases lag Play Store, code pushes continue.
- **Tech stack:** Java (~44%), C++ (~32%, NDK — own native MTProto stack "tgnet"), C, asm, a little Kotlin. Gradle build; reproducible builds supported; multiple flavors (standard/beta/Huawei/standalone).
- **README fork rules:** "Obtain your own api_id for your application", "Do not use the name Telegram for your app", don't use the official logo, and "publish your code too in order to comply with the licences."
- **Reuse feasibility:** notoriously monolithic — giant `ui` package, God-classes (`MessagesController`, `FileLoader`), native tgnet tightly coupled. Hardest of all official clients to strip. Study `FileLoader`/`DownloadController` for parallel-download patterns at most.
- **Sources:** https://github.com/DrKLO/Telegram , https://telegram.org/apps

## 6. Telegram macOS (`overtake/TelegramSwift`)

- **Repo:** https://github.com/overtake/TelegramSwift
- **License:** GPL-2.0 (GitHub API SPDX: `GPL-2.0`; README: "Telegram for macOS is licensed under the GNU Public License, version 2.0"; telegram.org/apps: "GNU GPL v. 2")
- **Status (2026-07):** pushed 2025-09-22 — **~10 months without a push to the public repo as of 2026-07-15**, the stalest official client mirror; ~5.7k stars.
- **Tech stack:** Swift (~60%) + C (~29%); Xcode workspace; in-repo `packages/` (TGUIKit, TelegramUI, TelegramMedia, FetchManager, InAppSettings, …) plus `submodules/` and `core-xprojects`.
- **MTProto core:** it does **not** implement its own protocol stack — it consumes the same core family as Telegram-iOS (MtProtoKit / TelegramCore / Postbox pulled in via its submodules/core projects). So the "one Swift core, two UIs (iOS + macOS)" pattern is proven by Telegram itself — relevant if tgfs goes Swift-native on Apple platforms.
- **Reuse feasibility:** same story as iOS core, plus GPLv2-only text in-repo (GPLv2 ≠ GPLv3 compatible — do not mix with tdesktop code).
- **Sources:** https://github.com/overtake/TelegramSwift , https://telegram.org/apps

## 7. Unigram (`UnigramDev/Unigram`) — Windows

- **Repo:** https://github.com/UnigramDev/Unigram
- **License:** GPL-3.0 (GitHub API SPDX: `GPL-3.0`; telegram.org/apps: "GNU GPL v. 3 or later")
- **Status (2026-07):** very active — v12.8 released 2026-06-12, pushed 2026-07-03, ~5.3k stars, 192 releases. Ships via Microsoft Store.
- **Tech stack:** C# (~90%) on Windows (UWP/WinUI lineage), native C++ helpers (`Telegram.Native`, `Telegram.Native.Calls`, tgcalls).
- **TDLib-based: confirmed** — the app has a `Telegram/Td/` directory with TDLib bindings (verified in repo tree 2026-07-15); all protocol/sync/download logic is delegated to TDLib.
- **Why it matters for tgfs:** Unigram is the best proof that a **full-featured client can be built on TDLib alone** by a small team, with the entire MTProto/sync/file-download problem outsourced to a **Boost-1.0-licensed** (permissive!) library. Its GPL-3.0 covers the C# app code, but the pattern — "thin app over TDLib" — is exactly replicable in a proprietary product because TDLib itself is Boost 1.0 (https://core.telegram.org/tdlib, https://telegram.org/apps).
- **Sources:** https://github.com/UnigramDev/Unigram , https://core.telegram.org/tdlib , https://telegram.org/apps

## 8. Notable third-party clients (verified 2026-07-15 via GitHub API)

| Client | Repo | Base | License (SPDX) | Activity | Worth studying for |
|---|---|---|---|---|---|
| **Telegram X (Android)** | https://github.com/TGX-Android/Telegram-X | **TDLib** (native Android UI, official alternative client) | `GPL-3.0` | pushed 2026-07-14, ~5.8k stars | The canonical open-source **TDLib Android integration** (JNI bindings, file download UX, sync) |
| **Nekogram** | https://github.com/Nekogram/Nekogram | Fork of official Android (tgnet, NOT TDLib) | `GPL-2.0` | pushed 2026-06-24, ~3.7k stars | How forks of DrKLO/Telegram manage upstream merges; not architecturally interesting for us |
| **Materialgram** | https://github.com/kukuruzka165/materialgram | tdesktop fork | `NOASSERTION` (inherits GPLv3+OpenSSL exc.) | pushed 2026-06-25, ~1.1k stars | Evidence tdesktop is fork-friendly at build level; cosmetic fork only |
| **Kotatogram** | https://github.com/kotatogram/kotatogram-desktop | tdesktop fork | `NOASSERTION` (inherits GPLv3+OpenSSL exc.) | pushed 2026-07-02, ~1.3k stars | Feature-level tdesktop fork (was semi-dormant, still gets pushes); shows cost of tracking tdesktop upstream |
| **MonoGram** | https://github.com/monogram-android/monogram | TDLib + Jetpack Compose | (check repo) | newer project | Greenfield TDLib + modern Android UI pattern |
| GramJS (library, not client) | https://github.com/gram-js/gramjs | own MTProto in TS | **`MIT`** | pushed 2026-07-14, ~1.8k stars | The MTProto engine inside Telegram Web A, usable standalone in proprietary code |

- Additional context source: https://wiki.archlinux.org/title/Telegram , https://alternativeto.net/lists/40876/telegram-third-party-clients/

## 9. Telegram API Terms of Service for third-party clients (as of mid-2026)

Source: https://core.telegram.org/api/terms (fetched 2026-07-15)

- **Own api_id mandatory:** every app must "obtain your own api_id" (https://my.telegram.org). Official clients' api_id must not be reused by forks (each fork README repeats this).
- **Branding:** cannot use the name "Telegram" in the app title unless prefixed "Unofficial"; official paper-plane logo prohibited.
- **User consent:** no actions "on behalf of the user without the user's knowledge and consent" — fine for tgfs (user-initiated export of own data).
- **Feature-parity obligations for full clients:** "all the basic features of the main Telegram apps [must] function correctly" — prohibited: disabling self-destruct timers, ghost-mode read-status hiding, hiding typing indicators, forcing other clients' users to download your app. **Relevance:** these obligations target *messaging clients*. tgfs is an export/backup tool, not a messenger — but since it logs in as a user session via the same API, the safe posture is: don't present as a chat client, don't implement anti-features, use takeout sessions.
- **Sponsored messages:** apps that *display* channel content must support official sponsored messages. An exporter that only downloads history arguably doesn't "display" channels as a feed; risk is low but nonzero — track this.
- **AI-training ban (2026 terms):** apps are "prohibited from using, accessing or aggregating data obtained from the Telegram platform to train, fine-tune or otherwise engage in the development" of AI models. tgfs must not pipe exported data into model training.
- **Enforcement:** Telegram may cut API access if violations aren't fixed within 10 days of notice.
- **Security guidelines** compliance required: https://core.telegram.org/mtproto/security_guidelines

## 10. Official takeout ("Export") API — what powers tdesktop's export

Source: https://core.telegram.org/api/takeout (fetched 2026-07-15)

- **Flow:** `account.initTakeoutSession` (flags: `contacts`, `message_users`, `message_chats`, `message_megagroups`, `message_channels`, `files` + `file_max_size`) → returns a takeout ID → wrap every subsequent call in `invokeWithTakeout`; message history pagination goes through `invokeWithMessagesRange`; finish with `account.finishTakeoutSession(success)`.
- **Coverage:** private/group/supergroup/channel messages, media attachments (`upload.getFile` "respecting the per-file size limit chosen by the user"), profile pictures, stories, contacts, sessions, forum topics, custom emoji.
- **Why use it:** takeout sessions exist precisely for bulk export of one's own data — they relax/segregate rate limits vs. a normal session and signal legitimate intent to Telegram's anti-abuse systems.
- **Constraint to design around:** `TAKEOUT_INIT_DELAY_%d` — Telegram can force a wait (up to ~24 h, e.g. for freshly authorized sessions) before a takeout session may start; the error carries the seconds to wait. Sources: https://core.telegram.org/method/account.initTakeoutSession , https://docs.telethon.dev/en/stable/concepts/errors.html
- tdesktop's `export_api_wrap.cpp` is the reference consumer of this API (§3a).

## 11. License analysis for a commercial tgfs

1. **Every official client app codebase is copyleft:** GPL-3.0 (Web A, Web K, Unigram, Telegram X), GPLv3+OpenSSL-exception (tdesktop), GPL-2.0 / "v2 or later" (Android, macOS, iOS-per-website). Forking any of them and **distributing** the result (app stores, downloadable desktop builds, or shipping JS bundles to browsers — client-side JS delivered to users is conveyance) obliges releasing the entire derivative work's source under the same GPL. A closed-source commercial fork is not an option.
2. **GPL is not fatal for a web-only SaaS in theory** (GPLv3 is not AGPL; server-side use isn't conveyance), but a web *client* ships its JS to the browser, which is distribution — so forking telegram-tt/tweb still triggers copyleft for the shipped frontend.
3. **GPLv2-only vs GPLv3 incompatibility:** TelegramSwift's in-repo license is GPLv2; tdesktop is GPLv3+exception. Code cannot be mixed across these forks.
4. **App Store + GPL friction:** GPLv2 code distributed through Apple's App Store has a known history of takedowns (VLC precedent). Telegram itself is fine because the copyright holder can self-license; a third-party fork doesn't get that luxury.
5. **Telegram-iOS has no in-repo LICENSE at all** — relying on a website statement ("GPL v2 or later" at telegram.org/apps) is shaky ground for a commercial derivative; treat as read-only reference.
6. **The clean commercial path:** build on permissively licensed protocol layers — **TDLib (Boost 1.0)** for native/desktop/mobile, **GramJS (MIT)** or similar for web — and keep the app code proprietary (or license it however we like). Use the GPL clients purely as *reference reading* (architecture, takeout usage, folder layouts, rate-limit handling), never copying code.
7. **API terms compliance checklist for tgfs:** own api_id; "Unofficial"/distinct branding; explicit user consent (inherent — user logs in to export own data); use takeout sessions for bulk export; no AI-training on exported data; respect security guidelines.

## 12. Verdict table

| Codebase | License | Fork for product? | Study as reference? | What to study |
|---|---|---|---|---|
| Web A (telegram-tt) | GPL-3.0 | No (copyleft on shipped JS) | **Yes — top pick for web** | GramJS integration, workers, media streaming/caching |
| Web K (tweb) | GPL-3.0 | No | Yes | Bespoke TS MTProto, worker architecture |
| tdesktop | GPLv3+OpenSSL exc. | No | **Yes — top pick overall** | `SourceFiles/export/*`: takeout usage, resumable pipeline, HTML/JSON writers, media-folder layout |
| Telegram-iOS | "GPLv2+" (no LICENSE file!) | No (legally murky) | Yes | MtProtoKit/TelegramApi/TelegramCore/Postbox layering |
| Telegram Android | GPL-2.0 | No | Marginal | FileLoader/download manager only |
| TelegramSwift (macOS) | GPL-2.0 | No | Yes | Proof of shared Swift core across iOS/macOS |
| Unigram | GPL-3.0 (app) over TDLib (Boost) | No (app code) — **but its pattern, yes** | **Yes** | Thin-app-over-TDLib architecture |
| Telegram X | GPL-3.0 over TDLib | No | **Yes** | TDLib on Android done right |
| GramJS (lib) | **MIT** | **Yes — usable directly** | — | Web MTProto engine |
| TDLib (lib) | **Boost 1.0** | **Yes — usable directly** | — | Whole sync/download layer for native targets |

---

*All GitHub metadata (SPDX license IDs, stars, pushed_at) pulled live from the GitHub API on 2026-07-15; repo file listings verified the same day via api.github.com/repos/.../contents and raw.githubusercontent.com.*
