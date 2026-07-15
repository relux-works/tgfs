# Unresolved Questions

Open decisions from the research phase (2026-07-15). Details and trade-offs: `.research/260715-telegram-filesystem-landscape.md`.

## Product

1. **Remote tier:** local-first is the v1 preference for desktop/Android. Is the remote source needed initially for iOS cold hydration, and should it be self-hosted only or eventually hosted SaaS? SaaS means account-equivalent Telegram-key custody and plaintext chat access.
2. **Deferred UI scope:** after the native drive is stable, is any Telegram-style chat UI needed, or only search/export management? Web is explicitly out of the initial scope.
3. **Secret chats:** out of scope? They are device-bound and cannot be reconstructed from the cloud.
4. **Deletions/edits policy:** mirror current Telegram state, or keep an audit log with tombstones and revisions?
5. **Media policy:** maximum file size, storage quota, eager download vs on-demand hydration defaults.
6. **Project name:** "tgfs" collides with existing TheodoreKrypton/tgfs. Rename?

## Architecture

7. **Dialog ordering on disk:** numeric folder prefixes (exact Telegram order, but frequent renames) vs stable names + `order.json` (exact order only inside product UI). Possibly a user-selectable mode.
8. **Engine placement:** local-first per-device TDLib is the v1 default for desktop/Android; the same Rust source trait preserves a gotd/td remote implementation. Confirm whether any non-iOS platform needs the remote source before v1.
9. **iOS cold hydration (local-first variant):** accept "open the app to download" UX, build a standalone lightweight Rust MTProto fetcher for the extension (grammers + CDN support work; Telegram-iOS's temp-context pattern proves feasibility), or route iOS through the remote backend?
10. **Export/backfill worker (service variant):** gotd/td (Go, one language with the daemon) vs Telethon (Python, first-class takeout support). Also: is TDLib's normal-API crawl acceptable for huge accounts in the local-first variant, or ship an optional desktop-only takeout backfill tool alongside?
11. **Shared client core:** Rust + UniFFI is now the recommended direction. Confirm packaging, binary size, cancellation, multi-process SQLite access, and Swift/Kotlin async behavior with an Apple/Android spike.
12. **Windows provider binding:** prefer a Rust CfAPI host using `windows-rs` or a vetted wrapper (`cloud-filter` crate is 0.0.x/one-maintainer — plan to own or fork that layer) so it can call the shared core directly. Confirm callback safety and placeholder lifecycle in a spike.
13. **Licensing model of the product itself:** open-source (which license) or proprietary? Constrains whether AGPL dependencies (tdl, MadelineProto) could ever be linked (currently: avoid).

## Compliance

14. **Protected-content chats:** Telegram terms require respecting no-save flags; product must decide UX for chats it deliberately will not export.
15. **Takeout UX:** `TAKEOUT_INIT_DELAY_X` can postpone export and returns the required wait in seconds. How should onboarding explain and resume this security delay?
