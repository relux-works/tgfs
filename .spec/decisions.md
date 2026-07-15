# Architecture Decision Log

Status: active
Last updated: 2026-07-15

This compact log records decisions that constrain decomposition. Detailed evidence lives in `.research/`; unresolved choices live in `docs/OPEN_QUESTIONS.md` and board decision tasks.

| ID | Status | Decision | Rationale / consequence |
|---|---|---|---|
| **DEC-001** | Accepted | Share the drive/sync engine in Rust. | Rust fits native linking, correctness-heavy state machines, Windows/Linux direct use, and UniFFI Swift/Kotlin bindings. |
| **DEC-002** | Accepted | Keep UI and provider entry points native. | OS providers and lifecycle are platform-specific; native Swift/Kotlin/WinUI layers avoid cross-platform UI/runtime impedance. |
| **DEC-003** | Accepted | Define a provider-neutral `DriveSource` contract. | The core must not depend directly on TDLib or gotd types. Fake, local TDLib, and remote HTTP sources share conformance tests. |
| **DEC-004** | Accepted for V1 | Prefer local TDLib on desktop and Android. | Delivers zero-infrastructure consumer UX; dataless placeholders avoid full per-device mirrors. |
| **DEC-005** | Provisional | Preserve an optional remote gotd/td source. | Needed for Takeout/canonical archive, self-hosting/SaaS, and possibly iOS cold hydration. Not mandatory for desktop/Android V1. |
| **DEC-006** | Accepted | Do not run TDLib in the iOS File Provider extension. | Verified 20 MB process limit plus TDLib/native/provider overhead leaves inadequate engineering margin. |
| **DEC-007** | Accepted | V1 is read-only with respect to Telegram. | Filesystem writes/moves/deletes do not map safely or unambiguously to Telegram operations. |
| **DEC-008** | Accepted | Stable item IDs are path/title/order independent. | Native providers persist IDs across rename, move, restart, and URI grants. |
| **DEC-009** | Accepted | Structured records are canonical; NDJSON/Markdown are deterministic views. | Enables regeneration, schema migration, search, and safe handling of edits/deletes. |
| **DEC-010** | Accepted | Use native provider APIs: File Provider, CfAPI, DocumentsProvider, FUSE. | These are the supported Dropbox-like integration points; web and WebDAV do not provide equivalent OS behavior. |
| **DEC-011** | Provisional | Ship macOS then Windows as the first vertical slices. | They exercise the two hardest desktop provider boundaries while reusing the same core/fixtures. |
| **DEC-012** | Open release gate | Select iOS cold-hydration strategy before iOS release. | Local-first cannot guarantee opening an unmaterialized file from Files while the app is unavailable. |

## Decision update procedure

When a decision changes, retain the old row, mark it superseded, add the replacement decision, update affected specifications, and revise board dependencies/acceptance criteria through `task-board`.
