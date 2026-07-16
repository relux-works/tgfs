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
| **DEC-013** | Accepted | Stable folder names; Telegram order via `order.json` + app UI, no numeric prefixes in v1. | Zero rename churn and stable paths; exact order remains available programmatically. Details: `policies.md` POL-1. |
| **DEC-014** | Accepted | Placeholders + on-demand hydration by default (10 GB LRU quota); opt-in Archive Mode (global/per-chat pin-all eager mirror). | Dropbox semantics by default, "download everything" preserved as explicit opt-in. Details: POL-2. |
| **DEC-015** | Accepted | Edit/delete retention is per-account selectable: Mirror (default) or Audit; canonical store is the append-only event log. | Serves both privacy-mirroring and archival users. Details: POL-3. |
| **DEC-016** | Accepted | Protected content shown as unavailable placeholders, never fetched; view-once never persisted; secret chats out of scope v1. | Telegram ToS compliance; visible-but-honest beats hidden chats. Details: POL-4. |
| **DEC-017** | Accepted | v1 support matrix: macOS 14+ (Sonoma), arm64 only; other platforms defined on entering scope. | Cheapest CI/test matrix; Intel out of scope. Details: POL-5. |
| **DEC-018** | Accepted | Product is proprietary; permissive-license dependencies only; GPL/AGPL strictly reference-only, enforced via SBOM scanning in CI. | Keeps monetization options open; open-sourcing later remains possible. Details: POL-6. |
| **DEC-019** | Accepted | Public name: **GramDrive** (`com.reluxworks.gramdrive.*`); repo/codename may stay tgfs. | tgfs and tgfiles both collide with existing projects/services; GramDrive verified collision-free 2026-07-17; trademark/handle check before public release. Details: POL-7. |
| **DEC-020** | Accepted | Single human approval gate: public release. Decision-row changes and new ToS-risk behaviors always escalate to the owner. | Maximizes agent-loop autonomy while keeping account-safety and release control human-owned. Details: POL-8. |

## Decision update procedure

When a decision changes, retain the old row, mark it superseded, add the replacement decision, update affected specifications, and revise board dependencies/acceptance criteria through `task-board`.
