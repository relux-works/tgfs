# Unresolved Questions

Open decisions from the research phase. Details and trade-offs: `.research/260715-telegram-filesystem-landscape.md`. Accepted decisions live in `.spec/decisions.md` + `.spec/policies.md`.

## Product

1. **Remote tier:** local-first is the v1 preference for desktop/Android. Is the remote source needed initially for iOS cold hydration, and should it be self-hosted only or eventually hosted SaaS? SaaS means account-equivalent Telegram-key custody and plaintext chat access.
2. **Deferred UI scope:** after the native drive is stable, is any Telegram-style chat UI needed, or only search/export management? Web is explicitly out of the initial scope.

## Architecture

3. **Engine placement:** local-first per-device TDLib is the v1 default for desktop/Android; the same Rust source trait preserves a gotd/td remote implementation. Confirm whether any non-iOS platform needs the remote source before v1.
4. **iOS cold hydration (local-first variant):** accept "open the app to download" UX, build a standalone lightweight Rust MTProto fetcher for the extension (grammers + CDN support work; Telegram-iOS's temp-context pattern proves feasibility), or route iOS through the remote backend? Decision gate: DEC-012, blocked by the iOS spikes.
5. **Export/backfill worker (service variant):** gotd/td (Go, one language with the daemon) vs Telethon (Python, first-class takeout support). Also: is TDLib's normal-API crawl acceptable for huge accounts in the local-first variant, or ship an optional desktop-only takeout backfill tool alongside?
6. **Shared client core:** Rust + UniFFI is now the recommended direction. Confirm packaging, binary size, cancellation, multi-process SQLite access, and Swift/Kotlin async behavior with an Apple/Android spike.
7. **Windows provider binding:** prefer a Rust CfAPI host using `windows-rs` or a vetted wrapper (`cloud-filter` crate is 0.0.x/one-maintainer — plan to own or fork that layer) so it can call the shared core directly. Confirm callback safety and placeholder lifecycle in a spike.

## Compliance

8. **Takeout UX (remote tier only):** `TAKEOUT_INIT_DELAY_X` can postpone export and returns the required wait in seconds. How should onboarding explain and resume this security delay?

## Resolved (2026-07-17)

Moved to `.spec/policies.md` / `.spec/decisions.md`:

- Dialog ordering on disk → **DEC-013** (stable names + `order.json`, no prefixes in v1).
- Media policy / quotas / eager-vs-on-demand → **DEC-014** (placeholders + opt-in Archive Mode, 10 GB LRU default).
- Deletions/edits policy → **DEC-015** (per-account Mirror default / Audit option).
- Protected content, view-once, secret chats → **DEC-016** (unavailable placeholders; secret chats out of scope v1).
- Support matrix → **DEC-017** (macOS 14+, arm64 only for v1).
- Product licensing → **DEC-018** (proprietary, permissive deps only).
- Public name → **DEC-019** (GramDrive, `com.reluxworks.gramdrive.*`).
- Human approval gates → **DEC-020** (single gate: public release).
