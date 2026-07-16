# Product Policies

Status: accepted
Last updated: 2026-07-17
Decided by: product owner (interactive session, 2026-07-17). Decision log rows: DEC-013…DEC-020 in `decisions.md`.

## POL-1. Dialog ordering on disk (DEC-013)

Folder names are **stable**: `<Display Name> — @<username>` (or display name only), no numeric order prefixes. Exact Telegram dialog order is exposed:

- inside the companion app UI (canonical order from the engine chat lists);
- as `order.json` metadata at each list root (Main / Archive / folder), regenerated on reorder events.

Folder renames happen only when the chat itself is renamed in Telegram. A numeric-prefix mode is explicitly **out of scope for v1** (revisit post-v1 as an optional projection mode).

## POL-2. Media cache and Archive Mode (DEC-014)

- Default state everywhere: **dataless placeholders**, content hydrates on open (Dropbox semantics).
- Default cache quota: **10 GB**, configurable; LRU eviction of unpinned content only.
- **Pin** = explicit "available offline"; pinned content is quota-exempt but counted and surfaced in the app.
- **Archive Mode** — the "download everything" product promise as an explicit opt-in:
  - toggle globally or per chat;
  - equivalent to pin-all + eager backfill of that scope, quota-exempt;
  - the app must show projected disk usage and warn on low disk space before enabling.
- Thumbnails are always eager (small, improves browsing).

## POL-3. Edit/delete retention (DEC-015)

Per-account mode selected at account setup, changeable later:

- **Mirror (default):** the archive reflects current Telegram state. Observed deletions purge content and rendered views; edits replace prior revisions. Internal event log keeps only minimal tombstone markers (id, timestamp) for sync correctness, no content.
- **Audit:** everything the archive has observed is retained — edits as revisions, deletions as content-preserving tombstones, all visible in the app and in `messages.ndjson`. An explicit purge tool exists.

In both modes the canonical store is the append-only message event log; the mode governs content retention/purging and rendered projections. History that predates first sync is unrecoverable in either mode (Telegram does not expose past revisions).

## POL-4. Restricted content (DEC-016)

- **Protected-content chats** (Telegram no-save flag): the chat and its structure are visible; text is exported only where Telegram permits; media items appear as **unavailable placeholders** with an explicit "restricted by Telegram" state and are never fetched into the archive. This follows Telegram's content-protection requirements; violating them risks API revocation.
- **View-once / self-destructing media:** never persisted, shown as unavailable.
- **Secret chats:** out of scope for v1 (device-bound, no cloud history).
- Engine must respect `can_be_saved` / protected-content flags from TDLib per message.

## POL-5. Support matrix v1 (DEC-017)

| Platform | Minimum | Architecture | Status |
|---|---|---|---|
| macOS | 14.0 (Sonoma) | arm64 (Apple Silicon) only | v1 target |
| Windows / Android / iOS / Linux | TBD | TBD | defined when the platform enters active scope |

Intel macOS builds are explicitly out of scope for v1.

## POL-6. Product licensing (DEC-018)

The product is **proprietary / closed-source**.

- Dependencies: permissive licenses only (MIT, Apache-2.0, BSD, BSL-1.0, Zlib, ISC).
- GPL/AGPL/LGPL-static code (tdesktop, tdl, MadelineProto, official Telegram clients) is **reference-only** — never linked, vendored, or translated verbatim.
- CI license scanning (SBOM) enforces this; any exception requires an explicit owner-approved decision row.

## POL-7. Public name (DEC-019)

Public product name: **GramDrive**.

- Bundle/package identifier prefix: `com.reluxworks.gramdrive.*`.
- Repository and internal codename may remain `tgfs`; all user-visible surfaces, marketing, and store listings use GramDrive.
- Collision check 2026-07-17: no exact GramDrive product/repo found (nearest: drivegram, teldrive, TeleDrive — different names, Telegram-as-storage niche). Formal trademark and handle (@-username, domain) acquisition check is required before public release.
- Telegram ToS naming rules apply: no "Telegram" in the name, no official logo.

## POL-8. Human approval gates (DEC-020)

Single mandatory human gate: **public release** (release-readiness review sign-off by the owner).

Everything else — implementation, tests, docs, research, code review — runs in the agentic producer → reviewer cycle without human stops. Two standing exceptions that always escalate to the owner regardless of gate policy:

- changing an Accepted decision row in `decisions.md` (per the decision update procedure);
- actions with Telegram ToS/account-safety risk beyond behaviors already approved in these policies.
