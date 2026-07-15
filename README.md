# TGFS — Telegram File System

Concept: expose a user's Telegram account as a file tree — **folder per chat** (named after the chat/user, in Telegram dialog order), containing all downloaded media plus the chat history exported as text. The primary product is a native drive resembling Dropbox/Google Drive:

- desktop and mobile apps that integrate with Finder, Explorer, iOS Files, Android's system picker, and Linux mounts;
- the Telegram engine embedded per device for desktop/Android v1, with an interchangeable remote source retained for iOS cold hydration, self-hosting, or a later hosted tier;
- no web application or rich Telegram-like UI requirement for the initial product.

## Status

**Research phase.** No code yet. The technology survey is complete; architecture and stack are proposed but not finally decided.

Working name note: "tgfs" collides with the existing [TheodoreKrypton/tgfs](https://github.com/TheodoreKrypton/tgfs) (opposite direction — Telegram as backing storage over WebDAV). Rename before going public.

## Current direction (from research, 2026-07-15)

- **Shared client core:** Rust — virtual tree, local SQLite/cache, change cursors, hydration, range downloads, retry/recovery, offline state, naming and generated files. Swift/Kotlin bindings through UniFFI; direct use on Windows/Linux. Verified precedents: Element X (matrix-rust-sdk), Dropbox Nucleus, Firefox (`.research/260715-shared-core-feasibility.md`).
- **Telegram source — behind one provider-neutral Rust trait, two interchangeable implementations:**
  - *Local-first:* [TDLib](https://github.com/tdlib/td) (BSL-1.0) embedded per device via tdjson FFI — zero infrastructure, Dropbox UX on desktop/Android; on iOS TDLib fits only in the main app (FP extension ~20 MB cap), and TDLib has no takeout API (normal-API backfill with flood-wait pacing).
  - *Remote:* Go + [gotd/td](https://github.com/gotd/td) (MIT) — optional for takeout backfill, one canonical archive, and iOS cold hydration; requires an always-on instance (self-hosted or SaaS with auth-key custody). Patterns from [iyear/tdl](https://github.com/iyear/tdl) (study only — AGPL); Telethon (Codeberg, MIT) is the takeout-worker alternative.
- **OS integration:** thin native adapters: File Provider replicated extensions (iOS/macOS, Swift), Cloud Files API (Windows, Rust), DocumentsProvider/SAF (Android, Kotlin), FUSE (Linux, Rust). Read-only first.
- **UI:** native and minimal; web and rich chat UI deferred.
- **Storage:** SQLite/PostgreSQL canonical metadata + content-addressed blob store; text export as NDJSON (lossless) + Markdown (human-readable).
- Treat official Telegram clients as **reference implementations**. A commercial GPL fork is legally possible, but a proprietary/closed-source fork is generally incompatible with their copyleft obligations and needs a deliberate licensing review.

## Research artifacts

| Artifact | Contents |
|---|---|
| `.spec/architecture.md` | Native-drive architecture with the shared Rust core and interchangeable local/remote sources |
| `.research/260715-telegram-filesystem-landscape.md` | Synthesized library, API, platform, architecture, and prior-art report |
| `.research/260715-core-libraries.md` | MTProto/TDLib library landscape |
| `.research/260715-oss-clients.md` | Official/OSS Telegram clients, licenses, and API terms |
| `.research/260715-filesystem-integration.md` | File Provider / Cloud Files / SAF / FUSE survey |
| `.research/260715-prior-art.md` | Exporter, archive, Telegram-FUSE, and WebDAV prior art |
| `.research/260715-shared-core-feasibility.md` | Rust/UniFFI precedents, TDLib extension constraints, gomobile and grammers analysis |
| `docs/OPEN_QUESTIONS.md` | Open product and architecture decisions |

## Tools

No project build tooling yet (research phase). Conventions:

- `.spec/` — product and architecture source of truth.
- `.research/` — permanent research archive.
- `.task-board/` and `.planning/` — project decomposition and generated execution plans.
- `.temp/` — ignored local agent/runtime artifacts only.
- Research documents cite primary-source URLs inline; verify against them before relying on a claim.
