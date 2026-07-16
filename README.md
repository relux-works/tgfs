# GramDrive

Concept: expose a user's Telegram account as a file tree — **folder per chat** (named after the chat/user, in Telegram dialog order), containing all downloaded media plus the chat history exported as text. The primary product is a native drive resembling Dropbox/Google Drive:

- desktop and mobile apps that integrate with Finder, Explorer, iOS Files, Android's system picker, and Linux mounts;
- the Telegram engine embedded per device for desktop/Android v1, with an interchangeable remote source retained for iOS cold hydration, self-hosting, or a later hosted tier;
- no web application or rich Telegram-like UI requirement for the initial product.

## Status

**Early implementation.** The technology survey, specification baseline, and complete service decomposition are committed; architecture decisions marked provisional still require their explicit decision tasks. Product code so far: the shared Rust core workspace skeleton (`crates/`, see below) — crate boundaries, dependency-direction rules, and quality gates; domain logic is still to come.

Start with the [specification index](.spec/README.md) and the [generated project plan](.planning/260715_045337_project.md). Product implementation is intentionally deferred until the plan is reviewed and approved.

Naming (DEC-019, POL-7): the public product name is **GramDrive**, and every shipped identifier is derived from the `com.reluxworks.gramdrive` namespace. `tgfs` remains the internal repository/codename only — it collides with [TheodoreKrypton/tgfs](https://github.com/TheodoreKrypton/tgfs) and must not appear in user-visible strings, marketing, or store listings. The repository is deliberately not renamed. Trademark/handle check happens before public release.

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
| `.spec/policies.md` | Accepted product policies POL-1…POL-8 (ordering, media/Archive Mode, retention, restricted content, support matrix, licensing, naming, approval gates) |
| `.research/260715-telegram-filesystem-landscape.md` | Synthesized library, API, platform, architecture, and prior-art report |
| `.research/260715-core-libraries.md` | MTProto/TDLib library landscape |
| `.research/260715-oss-clients.md` | Official/OSS Telegram clients, licenses, and API terms |
| `.research/260715-filesystem-integration.md` | File Provider / Cloud Files / SAF / FUSE survey |
| `.research/260715-prior-art.md` | Exporter, archive, Telegram-FUSE, and WebDAV prior art |
| `.research/260715-shared-core-feasibility.md` | Rust/UniFFI precedents, TDLib extension constraints, gomobile and grammers analysis |
| `docs/OPEN_QUESTIONS.md` | Open product and architecture decisions |
| `docs/TELEGRAM_API_COMPLIANCE.md` | Telegram API terms → verifiable controls, rule-to-task mapping (TGC-nn) |
| `docs/TRACEABILITY.md` | Requirement coverage matrix: every PRD/DOM/SYNC/PLAT/SEC/NFR/DEC/POL ID mapped to board elements |

## Delivery decomposition

The canonical local board is stored in `.task-board/` and must be changed through the `task-board` CLI. The current baseline contains 11 epics, 53 stories, and 142 atomic tasks; all remain unstarted.

Human-only work is isolated in the `manual-actions` epic (EPIC-260716-3vc5ay): product decisions and ADR ratification, external credentials (Telegram api_id/api_hash, Apple signing assets, Windows signing identity, test devices), and manual on-device release validation. Every other epic is designed to run autonomously in an agent loop once its `manual-actions` dependencies are done.

The project-level dependency plan has four phases:

1. manual decisions/credentials plus autonomous product-foundation analysis;
2. shared Rust core;
3. local TDLib source and the optional remote tier;
4. native drive integrations plus cross-platform quality, security, and release work.

Detailed generated plans for every epic are in [`.planning/`](.planning/). The remote tier is decomposed to preserve interface and sizing clarity, but remains optional and does not authorize hosted-service implementation.

## Tools

Conventions:

- `.spec/` — product and architecture source of truth.
- `crates/` — the shared Rust core workspace (`crates/README.md` documents layers, dependency direction, and feature policy).
- `.research/` — permanent research archive.
- `.task-board/` and `.planning/` — project decomposition and generated execution plans.
- `.scripts/` — reusable repo utilities.
- `.temp/` — ignored local agent/runtime artifacts only.
- Research documents cite primary-source URLs inline; verify against them before relying on a claim.

Available utilities:

| Tool | Purpose | Run | Output |
|---|---|---|---|
| `cargo` (Rust 1.91+, edition 2024) | Build and test the shared core workspace | `cargo build --workspace` / `cargo test --workspace` (repo root); per-crate commands in each `crates/*/README.md` | Binaries/test results under `target/` (gitignored) |
| `.scripts/check_crate_architecture.py` | Enforces `crates/README.md`: dependency direction, no cycles, no platform leakage in core crates, testkit dev-only, per-crate README sections | `python3 .scripts/check_crate_architecture.py` (repo root; stdlib only, needs `cargo` on PATH) | Exit 0 + summary line, or exit 1 with itemized errors (CI-suitable) |
| `cargo-deny` (installed via `brew install cargo-deny`) | POL-6 license gate: permissive-only dependency licenses, config in `deny.toml` | `cargo deny check licenses` (repo root) | `licenses ok`, or non-zero exit with the offending dependency tree |
| `make` | Aggregated repo checks | `make check` (arch + licenses + traceability + build + test); individual targets in `Makefile` | Fails on first broken gate |
| `.scripts/validate_traceability.py` | Validates `docs/TRACEABILITY.md` against `.spec/` and `.task-board/`: every requirement mapped exactly once, no orphan board elements, no stale requirement references on the board | `python3 .scripts/validate_traceability.py` (repo root; stdlib only) | Exit 0 + summary line, or exit 1 with itemized errors (CI-suitable) |
