# Prior Art: Telegram ↔ Filesystem / Export Projects

Research date: **2026-07-15**. All stars / activity / license data verified via GitHub API on this date unless noted.

Our target product: web + mobile + desktop app that logs into a user's Telegram account, downloads **all** files/media from chats, exports chat history as text, and organizes everything into **folders named after chats** (cloud-drive metaphor: folder = chat), with **continuous sync**.

---

## Verdict up front

**Nothing does exactly this.** The closest existing projects are:

1. **[Yarusstyle-coder/telegram-bulk-exporter](https://github.com/Yarusstyle-coder/telegram-bulk-exporter)** — brand new (created 2026-07-01), ~90% conceptual overlap: bulk export of all chats + per-chat auto-sync + folder-per-chat layout + web UI. But: 0 stars, single-user local web app, Windows-first, no mobile/desktop apps, no drive metaphor.
2. **[GeiserX/Telegram-Archive](https://github.com/GeiserX/Telegram-Archive)** — continuous incremental backup with real-time listener + Telegram-like web viewer. But: chat-viewer metaphor, not a file-drive; media stored under `media/{chat_id}/`, no folder-per-chat-name; Docker/self-host only.
3. **[iyear/tdl](https://github.com/iyear/tdl)** — the strongest engine (Go, gotd, resume, takeout, fastest downloads), but CLI-only, one-chat-per-command, no sync daemon.

No project combines: continuous sync + folder-per-chat named after the chat + text export alongside media + multi-platform GUI (web/mobile/desktop). The niche is open.

---

## A) Telegram-as-storage (opposite direction, reusable infra)

### A1. teldrive — `tgdrive/teldrive` (formerly `divyam234/teldrive`, GitHub 301-redirects)

- **URL:** https://github.com/tgdrive/teldrive
- **What it does:** Uses Telegram as unlimited cloud storage ("drive"): upload files into Telegram, browse/manage via a polished web UI. Opposite data direction from us (writes files *into* Telegram).
- **Stack:** Go backend; UI is React/TypeScript ([tgdrive/teldrive-ui](https://github.com/tgdrive/teldrive-ui), 67★); PostgreSQL for metadata (repo is 85.7% Go, 14.3% PLpgSQL).
- **Telegram lib:** gotd — the org maintains its own fork [tgdrive/td](https://github.com/tgdrive/td) ("Telegram client, in Go. (MTProto API)").
- **rclone:** maintains an rclone fork with a teldrive backend: [tgdrive/rclone](https://github.com/tgdrive/rclone) (Go, 113★, pushed 2026-05-22). Also `tgdrive/teldrive-mcp` (MCP server) and `tgdrive/varc` (sparse read-through range cache for media proxying, pushed 2026-06-26).
- **License:** MIT. **Stars:** 2,990. **Last push:** 2026-06-05 (active, 112+ releases, v1.8.x).
- **Reusable for us:** gotd usage patterns at scale (chunked transfer, rate-limit handling, multi-bot session pooling); the varc range-cache idea for streaming media without full download; Go+Postgres+React architecture; explicit warnings re: Telegram API rate limits / ban risk.
- **Gaps vs us:** wrong direction entirely — it does not read existing chats, no export, no folder-per-chat, no sync of chat content.

### A2. tgfs — `TheodoreKrypton/tgfs`

- **URL:** https://github.com/TheodoreKrypton/tgfs
- **What it does:** "Telegram becomes a WebDAV server" — stores files in a **private channel**, exposes them via WebDAV so any client (rclone, Cyberduck, WinSCP, OS mounts) sees a filesystem. Chunks large files, presents them unified; optional metadata in a GitHub repo enables folder versioning; Telegram Mini App for importing existing Telegram files; video live-streaming support.
- **Stack:** Python (81.6%) + TypeScript (17.9%) frontend (originally TypeScript, rewritten to Python); Poetry, Docker.
- **License:** Apache-2.0. **Stars:** 106. **Last push:** 2026-02-25 (slow but alive).
- **Reusable for us:** WebDAV as a universal "mount" interface — lets us skip FUSE and get drive semantics on every OS via existing clients; the metadata-outside-Telegram pattern (their GitHub-repo metadata trick generalizes to "keep an index DB next to the blob store").
- **Gaps vs us:** storage direction only (one designated channel, not the user's real chats); no export of history; no per-chat folders; no GUI apps.

### A3. rclone Telegram backend

- **Official status:** feature request [rclone/rclone#5829 "[backend] add Telegram"](https://github.com/rclone/rclone/issues/5829) — opened Nov 2021, **still open as of 2026-07-12** (55 comments, recently active). No Telegram backend in official rclone. Forum thread: https://forum.rclone.org/t/support-for-telegram/28142
- **Community attempts:** [birdup000/RcloneTelegram](https://github.com/greengeckowizard/RcloneTelegram) (Go, MIT, 41★, **archived**, last push 2023-10) — bot-API-based, 2 GB limit; `mohamedkaissi/RcloneTelegram` (listed on [pkg.go.dev](https://pkg.go.dev/github.com/mohamedkaissi/RcloneTelegram), repo now 404/deleted).
- **Practical route today:** rclone → teldrive backend via the [tgdrive/rclone fork](https://github.com/tgdrive/rclone), or rclone WebDAV → tgfs. Also [tangyoha's wiki documents an rclone upload adapter](https://github.com/tangyoha/telegram_media_downloader/wiki/Rclone) for pushing downloads to other clouds.
- **Takeaway:** no maintained native rclone Telegram backend exists; everyone bridges via an intermediate server (teldrive/tgfs). Validates a server-with-standard-protocol architecture (WebDAV/S3-like) rather than a bespoke rclone backend.

---

## B) Export / download tools (our direction)

### B1. tdl — `iyear/tdl` ⭐ the reference engine

- **URL:** https://github.com/iyear/tdl · docs: https://docs.iyear.me/tdl
- **What it does:** Go CLI Telegram toolkit: bulk media download, chat export, upload, forward, account migration, extensions.
- **Stack / lib:** Go, built on **gotd** (`github.com/gotd/td v0.140.0` + `gotd/contrib` in [go.mod](https://raw.githubusercontent.com/iyear/tdl/master/go.mod)); modular `tdl/core` + `tdl/extension` Go modules.
- **License:** **AGPL-3.0** (important: viral if we link it; subprocess/CLI orchestration — as telegram-bulk-exporter does — avoids derivative-work issues). **Stars:** 7,785. **Last push:** 2026-07-10 (very active; 196 open issues).
- **Export details** ([docs](https://docs.iyear.me/tdl/guide/tools/export-messages/)): `tdl chat export -c CHAT` → JSON (`tdl-export.json`); filters by time range (`-T time`), id range (`-T id`), last-N (`-T last`), plus expression filters (`-f "Views>200 && Media.Size > 5*1024*1024"`); `--with-content` includes message text (off by default); `--all` exports non-media messages too ("useful for backups"); `--raw` dumps MTProto structs; `--topic`/`--reply` for forums/comments. **One chat per invocation.** Also `tdl chat ls` (list dialogs) and member export.
- **Download details** ([docs](https://docs.iyear.me/tdl/guide/download/)): downloads from its own export JSON **or tdesktop's JSON export**, or from `t.me` links; `--continue`/`--restart` resume; `--takeout` flag uses Telegram takeout sessions for lower flood-wait limits on bulk jobs; concurrency `-t` (threads/task) × `-l` (parallel tasks); filename templates; `-i`/`-e` extension filters; `--group` for albums; downloads from protected/restricted chats.
- **Reusable for us:** the whole engine layer — gotd + takeout + resume + flood-wait handling is exactly our downloader core. Either embed the approach (reimplement on gotd under our license) or orchestrate tdl as a subprocess (proven by telegram-bulk-exporter). Its JSON schema is a de-facto interchange format.
- **Gaps vs us:** CLI only, no GUI; no continuous sync/daemon mode; one chat per run; no folder-per-chat convention out of the box (template-able); AGPL.

### B2. telegram_media_downloader — `Dineshkarthik/telegram_media_downloader`

- **URL:** https://github.com/Dineshkarthik/telegram_media_downloader
- **What it does:** Config-driven (YAML) downloader of media from chats/channels, up to 2 GiB/file. Media types: audio, document, photo, video, video_note, voice; per-type format filters; date-range filters; **incremental** via `last_read_message_id` per chat + `ids_to_retry`; multi-chat config with `parallel_chats`; files land in per-chat dirs split by media type (`<chat_id>/photo/…`).
- **Stack / lib:** Python; **v3.0.0 migrated Pyrogram → Telethon** (Python 3.8+). **License:** MIT. **Stars:** 2,673. **Last push:** 2026-04-27 (active).
- **Reusable:** clean minimal model for incremental per-chat cursors (`last_read_message_id`) and retry lists; media-type directory layout.
- **Gaps:** no text/history export at all; no GUI (fork has web UI); folders keyed by chat *id*, not name; no real-time listener; one-shot runs (cron it yourself).

### B3. tangyoha fork — `tangyoha/telegram_media_downloader`

- **URL:** https://github.com/tangyoha/telegram_media_downloader
- **What it does:** Heavily extended fork: **web dashboard** (localhost:5000) for progress, **bot command interface**, **continuous listening for new messages** (closest thing to "sync" in this family), rclone + aligo upload adapters (push downloads onward to other clouds), custom path templates (chat title / date / type), resume, zip-before-upload, format filters, Docker.
- **Stack / lib:** Python + Flask web UI, **Pyrogram**. **License:** MIT. **Stars:** 5,375 (more than upstream). **Last push:** 2026-03-04.
- **Reusable:** proof of demand for exactly our feature set (bulk + monitor + web UI); path-prefix templating by chat title; rclone-adapter pattern for onward sync.
- **Gaps:** media only (no text export); Chinese-first docs; Flask-era architecture; no mobile/desktop apps; folder naming configurable but not a drive metaphor.

### B4. Telethon-based exporters

- **telegram-export** — `expectocode/telegram-export`, now redirect → [tnjd/telegram-export](https://github.com/tnjd/telegram-export). Python, MPL-2.0, 482★, **archived, dead since 2019-10**. Downloaded users/chats/messages/media into SQLite. Maintainers quit because "Telegram updates and schema changes make it very tedious"; they point users to tdesktop's built-in export ([PyPI page](https://pypi.org/project/telegram-export/)). Lesson: schema churn is the maintenance killer — use an actively maintained MTProto lib and store raw+normalized.
- **tg-archive** — [knadh/tg-archive](https://github.com/knadh/tg-archive). Python/Telethon, MIT, 1,158★, pushed 2026-03-01. **Periodically syncs** group messages into SQLite (only new since last sync, cron-friendly) and publishes a static-site archive (mailing-list style) with embedded media, avatars, RSS. Group-centric, one group per site; no drive/folders metaphor.
- **GeiserX/Telegram-Archive** — https://github.com/GeiserX/Telegram-Archive. Python/**Telethon**, GPL-3.0, 154★, pushed **2026-07-14** (extremely active). "Own your Telegram history": **incremental backups on cron (default 6h) + optional real-time listener** capturing new messages, **edits and deletions**; album grouping, forward metadata; media dedup via symlinks; SQLite or PostgreSQL; two Docker images (backup scheduler + viewer); **local web viewer that mimics Telegram UI**, lightbox, search, WebSocket live updates, push notifications; multi-user ACLs, audit log; **JSON export with date filters**. Media layout: `media/{chat_id}/{files}`. Limitations: no secret chats; viewer-centric not file-centric. **This is the closest architectural cousin for our "continuous sync" pillar.**
- **telegram-bulk-exporter** — [Yarusstyle-coder/telegram-bulk-exporter](https://github.com/Yarusstyle-coder/telegram-bulk-exporter). Python, license file present but non-standard per API (`NOASSERTION`; README states MIT), **0★, created 2026-07-01** (two weeks old). Architecture: **Telethon for dialogs/auth/avatars + tdl subprocess for fast bulk export** (~10× faster than Telethon alone, per README); FastAPI + HTMX/Alpine/Tailwind local web UI; SQLite state. Features: checkbox grid of **all chats** with avatars; **chunked id-range export** with per-chunk committed cursors (crash-safe resume); **"only new since last export"** incremental toggle; **per-chat "Auto" background scheduler** that enqueues incremental sync jobs when Telegram has newer messages; **SHA-256 media dedup** into `media_pool/` with hardlinks + `bytes_saved_via_links` accounting; output layout `exports/chat_<slug>_<id>/` containing `messages.json`, `messages.html`, media; proxy pool (SOCKS5/HTTP/MTProto) with auto-fastest selection; master password + TOTP gate; 392 pytest tests. Windows-first; Linux/macOS/Docker less tested; single-user; data at rest unencrypted (by design). **Closest overall feature match to our brief** — but zero traction, solo spare-time project, local web UI only.
- Others found (minor): [mo1ein/TelegramArchive](https://github.com/mo1ein/TelegramArchive) (Python, MIT, 73★, pushed 2026-02-12, chat export); [N4rr34n6/TelegramBackup](https://github.com/N4rr34n6/TelegramBackup) (AGPL-3.0, 66★, pushed 2026-05-09, forensic-style extraction preserving reactions/replies/forwards); [KeL3vRa/TelegramExporter](https://github.com/KeL3vRa/TelegramExporter) (Python, 40★, dead 2021, forensic all-chats extraction); [popstas/telegram-download-chat](https://github.com/popstas/telegram-download-chat) (Python, MIT, 171★, pushed 2026-07-08, per-chat history download with date/keyword filters); [gumblex/tg-export](https://github.com/gumblex/tg-export) (LGPL-3.0, 60★, dead 2018, telegram-cli era); [Nukesor/archivebot](https://github.com/Nukesor/archivebot) (MIT, 101★, **archived** 2024 — bot that did "full backup of all files posted in a chat and continuous backup of incoming new media"). `seuyh/Telegram-Chat-Exporter` (self-contained HTML export with media viewer) appeared in search results but the repo 404s as of 2026-07-15 — deleted or made private.

### B5. Baseline: Telegram Desktop built-in export

- **URLs:** https://telegram.org/blog/export-and-more · https://github.com/desktop-app/tdesktop
- **What it does:** Settings → Advanced → "Export Telegram data": full-account or per-chat export to **HTML or JSON**, including photos/media with size caps, contacts, sessions. C++/Qt, GPLv3-with-OpenSSL-exception.
- **Why it matters:** it's the free baseline every user has; tdl can even ingest its JSON output for media download. **Its gaps define our product:** manual one-shot (no scheduling/incremental re-export), desktop-only, single machine, no folder-per-chat drive view, slow single-stream media download (the pain point telegram-bulk-exporter explicitly cites), no API/automation.

---

## C) FUSE / drive mounts of Telegram chats

- **[nktknshn/tgmount](https://github.com/nktknshn/tgmount)** — Python/Telethon + libfuse. **Mounts Telegram dialogs/channels as a read-only VFS** (originally to play posted audio in desktop players); live updates for new posts; filters by type. Apache-2.0, 72★, unmaintained (README points to successor). Successor **[nktknshn/tgmount-ng](https://github.com/nktknshn/tgmount-ng)** — Telethon + pyfuse3, YAML config with **multiple chats organized into nested directories**, organizers by sender/forward/performer/reactions, mounts ZIPs as folders, live message add/edit/delete tracking, optional caching. Self-described "VERY ALPHA", 54★, no license file, **dead since 2023-04**. Linux-only, read-only. **Conceptually the closest thing to "chats as folders" that ever existed — and it's abandoned.**
- **[Firemoon777/tgfs](https://github.com/Firemoon777/tgfs)** — C, GPL-3.0, 198★, dead since 2018. Telegram attachments in FUSE.
- **[devdetour/TelegramFUSE](https://github.com/devdetour/TelegramFUSE)** — Python FUSE, read/write files to Telegram (storage direction), no license, 361★, dead since 2024. Author write-up: https://www.mikedegeofroy.com/blog/telegram-fs
- **[duo/telegram-fuse](https://github.com/duo/telegram-fuse)** — Rust, AGPL-3.0, 21★, dead since 2022; storage-direction FUSE modeled on onedrive-fuse.
- **[ergolyam/tgfuse](https://github.com/ergolyam/tgfuse)** — Python userbot mounting a single channel as a Linux drive, read-write if you can post; 6★ but pushed **2026-07-12** (alive, tiny).
- **Takeaway:** FUSE-mounting *chats* has been tried repeatedly, always Linux-only, always toy-scale, all abandoned except one 6-star project. The mount UX is desirable but FUSE is the wrong delivery mechanism for multi-platform; WebDAV (tgfs-style) or a synced local folder (our approach) is more portable.

---

## Requirements coverage matrix

| Requirement | tdl | Dinesh/tangyoha | tg-archive | GeiserX T-Archive | bulk-exporter | teldrive/tgfs | tgmount-ng |
|---|---|---|---|---|---|---|---|
| All chats in one run | ✗ (1/run) | ✓ (config list) | ✗ (1 group) | ✓ (whitelist) | ✓ (checkbox all) | n/a | ✓ (config) |
| Continuous sync (not one-shot) | ✗ | ~ (fork: listen mode) | ~ (cron) | ✓ (cron + realtime listener) | ✓ (per-chat auto scheduler) | n/a | ✓ (live, RO mount) |
| Folder-per-chat named after chat | ~ (template) | ~ (id-based / fork: title prefix) | ✗ | ✗ (`media/{chat_id}`) | ✓ (`chat_<slug>_<id>/`) | ✗ | ✓ (virtual) |
| Text export alongside media | ✓ (`--all --with-content` JSON) | ✗ | ✓ (SQLite→HTML) | ✓ (DB + JSON export) | ✓ (json + html) | ✗ | ✗ |
| Dialog ordering / chat-list semantics | ~ (`chat ls`) | ✗ | ✗ | ✗ | ~ (grid w/ avatars) | ✗ | ✗ |
| Resume / incremental | ✓ | ✓ (`last_read_message_id`) | ✓ | ✓ | ✓ (chunk cursors) | n/a | n/a |
| Takeout API (low flood limits) | ✓ | ✗ | ✗ | ✗ | ✓ (via tdl) | ✗ | ✗ |
| Multi-platform GUI (web+mobile+desktop) | ✗ | ✗ | ✗ | ~ (web viewer) | ~ (local web) | ~ (web) | ✗ |

**No project ticks every column.** In particular, nobody offers mobile/desktop apps, and nobody treats the chat list as an ordered drive.

---

## What to reuse (recommendations)

1. **Engine:** gotd (Go, MIT, pushed 2026-07-14, 2,285★ — https://github.com/gotd/td) as MTProto core, following tdl's patterns: takeout sessions, `--continue`-style cursor resume, threads×tasks concurrency, expression filters. If Go isn't chosen, Telethon (what 5 of the closest projects use) is the Python default. Avoid embedding tdl itself unless we accept AGPL; subprocess orchestration (telegram-bulk-exporter's approach) is the legal workaround.
2. **Sync design:** GeiserX/Telegram-Archive's dual-mode "cron incremental + realtime event listener (new/edit/delete)" and telegram-bulk-exporter's "chunked id-range export with per-chunk committed cursors" + per-chat auto-sync scheduler. Both are directly applicable blueprints.
3. **On-disk layout:** telegram-bulk-exporter's `exports/chat_<slug>_<id>/` (slug from chat title + stable id suffix to survive renames/collisions) + content-hash `media_pool` with hardlinks for dedup; Dineshkarthik's per-type subfolders and `last_read_message_id` cursor.
4. **Drive semantics without FUSE:** tgfs's WebDAV front (mountable everywhere) and teldrive's rclone-fork strategy show how to get "cloud drive" UX on all OSes without shipping kernel drivers.
5. **Rate-limit survival:** teldrive's ban warnings, tdl's takeout + flood-wait handling, bulk-exporter's proxy pool — API abuse is the #1 operational risk; takeout sessions are the sanctioned path for bulk history download.
6. **Cautionary tale:** telegram-export died from MTProto schema churn; keep the raw-layer dependency (gotd/Telethon) thin and updatable, store `--raw`-style original payloads plus normalized rows.

## Gap we fill

Continuous, account-wide, folder-per-chat mirror with text + media, dialog-ordered, delivered as consumer-grade web/mobile/desktop apps. Every existing project is either: storage-direction (teldrive/tgfs), single-chat/one-shot CLI (tdl, downloaders), viewer-centric archive (GeiserX, tg-archive), or an abandoned Linux FUSE toy (tgmount). The two closest (GeiserX Telegram-Archive, telegram-bulk-exporter) are self-hosted single-user web tools with no mobile/desktop story and no drive metaphor.

---

## All sources

- https://github.com/tgdrive/teldrive · https://github.com/tgdrive/teldrive-ui · https://github.com/tgdrive/rclone · https://github.com/tgdrive/td · https://github.com/tgdrive/varc · https://github.com/tgdrive/teldrive-mcp
- https://github.com/TheodoreKrypton/tgfs
- https://github.com/rclone/rclone/issues/5829 · https://forum.rclone.org/t/support-for-telegram/28142 · https://github.com/greengeckowizard/RcloneTelegram · https://pkg.go.dev/github.com/mohamedkaissi/RcloneTelegram
- https://github.com/iyear/tdl · https://docs.iyear.me/tdl · https://docs.iyear.me/tdl/guide/tools/export-messages/ · https://docs.iyear.me/tdl/guide/download/ · https://raw.githubusercontent.com/iyear/tdl/master/go.mod
- https://github.com/Dineshkarthik/telegram_media_downloader · https://github.com/tangyoha/telegram_media_downloader · https://github.com/tangyoha/telegram_media_downloader/wiki/Rclone
- https://github.com/tnjd/telegram-export (ex expectocode/telegram-export) · https://pypi.org/project/telegram-export/
- https://github.com/knadh/tg-archive · https://github.com/GeiserX/Telegram-Archive · https://github.com/Yarusstyle-coder/telegram-bulk-exporter · https://github.com/mo1ein/TelegramArchive · https://github.com/N4rr34n6/TelegramBackup · https://github.com/KeL3vRa/TelegramExporter · https://github.com/popstas/telegram-download-chat · https://github.com/gumblex/tg-export · https://github.com/Nukesor/archivebot
- https://telegram.org/blog/export-and-more · https://github.com/desktop-app/tdesktop
- https://github.com/nktknshn/tgmount · https://github.com/nktknshn/tgmount-ng · https://github.com/Firemoon777/tgfs · https://github.com/devdetour/TelegramFUSE · https://github.com/duo/telegram-fuse · https://github.com/ergolyam/tgfuse · https://www.mikedegeofroy.com/blog/telegram-fs
- https://github.com/gotd/td
