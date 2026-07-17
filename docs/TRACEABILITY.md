# Requirement Coverage Matrix

Status: baseline
Last updated: 2026-07-17
Owner task: TASK-260715-1czb40 (requirement-coverage-matrix)
Validation: `python3 .scripts/validate_traceability.py` (run from repo root; CI-suitable, exits non-zero on any missing or orphan reference)

<!-- deferred-epics: EPIC-260715-1mlv5j EPIC-260715-y0fshx EPIC-260715-1hnglv EPIC-260715-3uynbw EPIC-260715-2y4q0r -->

This matrix maps every requirement identifier committed in `.spec/` (PRD, DOM, SYNC, PLAT, SEC, NFR, DEC, POL) to the board elements that implement or validate it. It is the canonical requirement-to-work mapping; board READMEs may additionally cite requirement ranges as scope hints, but coverage is asserted here.

## Dispositions

- **active** — committed V1 scope on the macOS-first path (DEC-017 / POL-5): shared core, local TDLib source, macOS drive, quality/security/release, and human-gate work.
- **deferred-platform** — decomposed on the board but deferred until the platform (Windows / Android / Linux / iOS) enters active scope per DEC-017 / POL-5. Board elements exist inside deferred platform epics.
- **deferred-optional** — optional remote tier per DEC-005; not required for local-first release.
- **future** — recorded to protect interfaces; not an implementation commitment; intentionally has no board element.

Deferred epics: EPIC-260715-1mlv5j (windows-native-drive), EPIC-260715-y0fshx (android-native-drive), EPIC-260715-1hnglv (linux-native-drive), EPIC-260715-3uynbw (ios-native-drive), EPIC-260715-2y4q0r (optional-remote-tier).

## Summary

| Disposition | Count |
|---|---|
| active | 166 |
| deferred-platform | 24 |
| deferred-optional | 10 |
| future | 1 |
| **Total** | **201** |

By namespace: PRD 30, DOM 13, SYNC 41, PLAT 32, SEC 28, NFR 29, DEC 20, POL 8.

## Matrix

Multiple board elements on one row are justified in Notes (typical split: engine implementation vs. source adapter vs. platform surface vs. validation harness, or decision record vs. implementing task).

### PRD — product and functional requirements

| Requirement | Tier | Disposition | Board elements | Notes |
|---|---|---|---|---|
| PRD-001 | V1 | active | STORY-260715-2nqkhl, TASK-260715-3s44pc | Session/account lifecycle (engine) + per-account provider domain registration keeps the multi-account design path. |
| PRD-002 | V1 | active | STORY-260715-2nqkhl, TASK-260715-13pxnu | Shared authorization state machine + macOS companion UI; auth never enters filesystem callbacks (PLAT-MAC-002 boundary). |
| PRD-003 | V1 | active | TASK-260715-wjaux5, TASK-260715-kxzfy7 | Removal workflow implementation + secure-cleanup verification. |
| PRD-004 | V1 | active | TASK-260715-3b9w8x, TASK-260715-162fdj | Stable error taxonomy + health/progress surfacing of auth/flood/offline/storage states. |
| PRD-010 | V1 | active | STORY-260715-17kl0q, TASK-260715-3tjduq | Telegram list/folder sync (source) + virtual tree projection (core). Custom folders: TASK-260715-54nopz. |
| PRD-011 | V1 | active | TASK-260715-1qz1g5, TASK-260715-1ffbkg | Path-independent stable IDs + filesystem-safe display names. |
| PRD-012 | V1 | active | TASK-260715-1jmsdp | Ordering projection per POL-1 (stable names + order.json; numeric prefixes out of v1). |
| PRD-013 | V1 | active | TASK-260715-3tjduq, TASK-260715-54nopz | Multi-appearance tree without duplicating canonical records + folder membership appearances. |
| PRD-014 | V1 | active | TASK-260715-1ffbkg | Deterministic collision and reserved-name handling. |
| PRD-020 | V1 | active | TASK-260715-2tq5sk | Versioned lossless NDJSON renderer. |
| PRD-021 | V1 | active | TASK-260715-hmmiay | Bounded monthly Markdown renderer. |
| PRD-022 | V1 | active | TASK-260715-1ynmct | Message normalization; README cites PRD-022 explicitly. |
| PRD-023 | V1 | active | STORY-260715-1oq9jg | Deterministic derived views; canonical store per DEC-009/DOM-006. |
| PRD-024 | V1 | active | STORY-260715-1oq9jg | Renderer fixtures include unavailable/deleted/protected/unsupported content states. |
| PRD-030 | V1 | active | TASK-260715-23arcu | Attachment metadata across all supported media kinds. |
| PRD-031 | V1 | active | STORY-260715-2hs8cf, TASK-260715-1onbmf | Shared hydration/transfer engine + TDLib download adapter (cancel/resume). |
| PRD-032 | V1 | active | TASK-260715-23arcu, TASK-260715-1ffbkg | Original filename/MIME/size preservation + deterministic safe filename derivation. |
| PRD-033 | V1 | active | TASK-260715-3s6cpe | Blob dedup with provenance preservation and distinct virtual items. |
| PRD-034 | V1 | active | TASK-260715-23arcu, TASK-260715-3prhsi | Saveability/protection capability mapping + accepted restricted-content policy (POL-4/DEC-016). |
| PRD-040 | V1 | active | TASK-260715-11abx8, TASK-260715-13pxnu | Quota/accounting engine + companion-app exposure of cache use. |
| PRD-041 | V1 | active | TASK-260715-g4k3zm, TASK-260715-11abx8 | Transfer state machine (dataless/hydrating/failed) + cache states (materialized/pinned/stale/evictable). |
| PRD-042 | V1 | active | TASK-260715-11abx8, TASK-260715-3s461k | System-eviction reconciliation in core + macOS pin/eviction reconciliation surface. |
| PRD-043 | V1 | active | TASK-260715-g4k3zm, TASK-260715-3s6cpe | Durable resumable transfers + atomic promotion (no partial publication). |
| PRD-050 | V1 | active | TASK-260715-162fdj, TASK-260715-13pxnu | Shared health/progress model + native status presentation. |
| PRD-051 | V1 | active | TASK-260715-1nuhxj, TASK-260715-4im48n | Diagnostic export + redaction policy/tests. |
| PRD-052 | V1 | active | TASK-260715-1nuhxj, TASK-260715-21clwh | User-triggered repair entrypoint + reconciliation that avoids redownloading unchanged content. |
| PRD-060 | Optional | deferred-optional | STORY-260715-2adhov, STORY-260715-1kv6pi | Remote Rust source client + provider-oriented drive API implement the same normalized contract. |
| PRD-061 | Optional | deferred-optional | STORY-260715-ntuixj, STORY-260715-2e8ivr, STORY-260715-1kv6pi | Takeout backfill/incremental ingest + canonical metadata/blob storage + range-addressable delivery. |
| PRD-062 | Optional | deferred-optional | TASK-260715-1zqf0k | Revocable per-device product tokens; Telegram keys stay service-side. |
| PRD-063 | Optional | deferred-platform | TASK-260715-1k4hmd, TASK-260715-180uh6 | iOS cold hydration implementation conditional on the ADR (DEC-012/PLAT-IOS-004); also depends on optional tier if remote option is selected. |

### DOM — domain-model invariants

| Requirement | Tier | Disposition | Board elements | Notes |
|---|---|---|---|---|
| DOM-001 | V1 | active | TASK-260715-1qz1g5 | Stable opaque ItemId independent of title/order/path. |
| DOM-002 | V1 | active | TASK-260715-1qz1g5, TASK-260715-3tjduq | Canonical vs. appearance identity split: key scheme + tree projection. |
| DOM-003 | V1 | active | TASK-260715-1j4ij3 | Metadata/content Version types in the source contract; persisted via STORY-260715-16ik2x. |
| DOM-004 | V1 | active | TASK-260715-1j4ij3, TASK-260715-1opnb2 | Opaque durable ChangeCursor type + atomic cursor persistence scoped to account/contract version. |
| DOM-005 | V1 | active | TASK-260715-1qz1g5 | AC explicitly forbids path/title dependence. |
| DOM-006 | V1 | active | STORY-260715-1oq9jg | Generated text derived from structured records + renderer/schema versions. |
| DOM-007 | V1 | active | TASK-260715-23arcu, TASK-260715-1onbmf | Locator/file-reference stored as refreshable metadata + refresh invisible to item identity. |
| DOM-008 | V1 | active | TASK-260715-11abx8 | Cache state never mutates source state; also guarded by NFR-014 read-only validation. |
| DOM-020 | V1 | active | TASK-260715-1qz1g5 | Opaque IDs stable across database rebuilds (AC: determinism/round-trip). |
| DOM-021 | V1 | active | TASK-260715-1qz1g5 | Canonical key composition (account, peer, message, attachment index, namespace version). |
| DOM-022 | V1 | active | TASK-260715-3tjduq, TASK-260715-54nopz | Appearance IDs include view identity; view moves create/remove appearances only. |
| DOM-023 | V1 | active | TASK-260715-1qz1g5 | Generated-file identity includes partition/format/schema family, never current title. |
| DOM-024 | V1 | active | TASK-260715-1qz1g5, TASK-260715-i3mp9x | Shared ItemId namespace + macOS provider item mapping. Deferred counterparts: TASK-260715-2sbyuy (Windows FILE_IDENTITY), TASK-260715-1ynoya (Android document IDs), TASK-260715-1za16i (Linux inodes). |

### SYNC — synchronization and filesystem semantics

| Requirement | Tier | Disposition | Board elements | Notes |
|---|---|---|---|---|
| SYNC-001 | V1 | active | STORY-260715-255sa3 | Story scope cites SYNC-001..005. |
| SYNC-002 | V1 | active | TASK-260715-3e8q4m, TASK-260715-3uft8j | Conformance suite + deterministic fake source it runs against. |
| SYNC-003 | V1 | active | TASK-260715-3e8q4m | Pagination repeatability; duplicate/missing children are contract failures. |
| SYNC-004 | V1 | active | TASK-260715-1opnb2, TASK-260715-3e8q4m | Durable cursor persistence + conformance tests for restart survival and mismatch rejection. |
| SYNC-005 | V1 | active | STORY-260715-255sa3, TASK-260715-rhcnhc | Bounded-deadline/cancellation contract semantics + macOS enumerator deadline compliance (v1 adapter). |
| SYNC-010 | V1 | active | TASK-260715-3tjduq | Virtual layout over shared canonical records/blobs. |
| SYNC-011 | V1 | active | TASK-260715-1jmsdp | Stable-name mode with order metadata per POL-1. |
| SYNC-012 | V1 | active | TASK-260715-1ffbkg | Collision suffixes deterministic from stable identity. |
| SYNC-013 | V1 | active | TASK-260715-1ffbkg | Strictest-target name sanitization budget. |
| SYNC-020 | V1 | active | TASK-260715-30amrq | Metadata-first initial discovery without eager media download. |
| SYNC-021 | V1 | active | TASK-260715-26dnp6 | Resumable idempotent per-chat history crawl. |
| SYNC-022 | V1 | active | TASK-260715-10p5zp | Source-order updates with transactional checkpoints. |
| SYNC-023 | V1 | active | TASK-260715-10p5zp | Gap recovery before durable cursor advance. |
| SYNC-024 | V1 | active | TASK-260715-37nhe5, TASK-260715-22l8zy | Edit application policy + affected generated-document version planning. |
| SYNC-025 | V1 | active | TASK-260715-37nhe5 | Deletion handling per POL-3 (Mirror/Audit); eviction remains distinct. |
| SYNC-026 | V1 | active | TASK-260715-1c8fea, TASK-260715-54nopz | Chat metadata/list updates + folder membership updates preserve canonical identity. |
| SYNC-030 | V1 | active | TASK-260715-2tq5sk | Explicit schema version, deterministic field/record order. |
| SYNC-031 | V1 | active | TASK-260715-hmmiay | Bounded deterministic timezone-explicit Markdown partitions. |
| SYNC-032 | V1 | active | TASK-260715-2tq5sk, TASK-260715-hmmiay | Both renderers use stable attachment links and explicit missing-content states. |
| SYNC-033 | V1 | active | TASK-260715-22l8zy | Atomic publication of regenerated documents. |
| SYNC-034 | V1 | active | STORY-260715-1oq9jg, TASK-260715-26eoqx | Renderer fixture requirements + shared synthetic fixture corpus that supplies the edge cases. |
| SYNC-040 | V1 | active | STORY-260715-2hs8cf, TASK-260715-30amrq | Dataless-default engine behavior + discovery that never eagerly hydrates. |
| SYNC-041 | V1 | active | TASK-260715-22fh09 | Byte-range fetch with backend chunk alignment. |
| SYNC-042 | V1 | active | TASK-260715-3s6cpe, TASK-260715-g4k3zm | Transfer-identity storage of partial data + atomic verified promotion. |
| SYNC-043 | V1 | active | TASK-260715-22fh09 | Prompt cancellation leaving resumable/disposable state. |
| SYNC-044 | V1 | active | TASK-260715-3b9w8x, TASK-260715-22fh09 | Retry taxonomy definition + application inside the fetch coordinator. |
| SYNC-045 | V1 | active | TASK-260715-1onbmf | File-reference refresh never changes provider identity (AC-explicit). |
| SYNC-046 | V1 | active | TASK-260715-22fh09 | Safe coalescing of concurrent same-item/version requests. |
| SYNC-050 | V1 | active | TASK-260715-11abx8 | Separate accounting for blobs/partials/generated/thumbnails/metadata. |
| SYNC-051 | V1 | active | TASK-260715-11abx8 | Pinned content protected from eviction by default. |
| SYNC-052 | V1 | active | TASK-260715-11abx8 | LRU eviction of eligible verified content only. |
| SYNC-053 | V1 | active | TASK-260715-11abx8, TASK-260715-3s461k | Core reconciliation of provider/system eviction + macOS reconciliation surface. |
| SYNC-054 | V1 | active | TASK-260715-11abx8 | Durable quota changes with actionable plan/status. |
| SYNC-060 | V1 | active | TASK-260715-1j4ij3, TASK-260715-i3mp9x | Capability model excludes writes + macOS adapter leaks no write capability. Deferred counterparts: TASK-260715-29m7as (Windows), TASK-260715-1lkypp (Linux), PLAT-AND-002 (Android). |
| SYNC-061 | V1 | active | TASK-260715-3b9w8x, TASK-260715-i3mp9x | Stable read-only error category (core) + v1 adapter surface. Deferred adapter enforcement: TASK-260715-29m7as, TASK-260715-1lkypp. |
| SYNC-062 | V1 | active | TASK-260715-13pxnu | Companion actions distinguish remove-local-copy / unpin / view removal; Telegram delete does not exist in V1. |
| SYNC-063 | Future | future | — | Write support is explicitly not committed; no board element by design (DEC-007). Requires a separately approved specification first. |
| SYNC-070 | V1 | active | TASK-260715-21clwh | Startup reconciliation of transfers/versions/registrations/cache. |
| SYNC-071 | V1 | active | TASK-260715-21clwh, TASK-260715-1nuhxj | Projection rebuild (core repair pass) + user-triggered repair/diagnostics entrypoint. |
| SYNC-072 | V1 | active | TASK-260715-18l9xz | Resumable crash-safe database/renderer migrations. |
| SYNC-073 | V1 | active | STORY-260715-16ik2x | Cursors/identity independent of wall clock; source timestamps explicit in state store semantics. |

### PLAT — platform requirements

| Requirement | Tier | Disposition | Board elements | Notes |
|---|---|---|---|---|
| PLAT-001 | V1 | active | STORY-260715-255sa3, TASK-260715-11qg88 | Provider-neutral contract keeps Telegram rules in core; native harnesses validate adapters stay translation-only. Realized per platform epic (macOS active; others deferred). |
| PLAT-002 | V1 | active | STORY-260715-14k4l9, STORY-260715-14n7wp | macOS adapter capabilities (v1). Deferred counterparts: STORY-260715-vf3ihw/1cmz06 (Windows), STORY-260715-31thz2/34xy5y (Android), STORY-260715-2sff6a/jgjoxz (Linux), STORY-260715-1hwxpw/r0cz40 (iOS). |
| PLAT-003 | V1 | active | TASK-260715-13pxnu | macOS minimal shell (v1). Deferred: TASK-260715-u3d734 (Windows), TASK-260715-5wcv0c (Android), TASK-260715-1bu7k2 (Linux), STORY-260715-2vq8mp (iOS). |
| PLAT-004 | V1 | active | STORY-260715-2ca0k9 | macOS packaging/signing/uninstall acceptance (v1). Deferred: STORY-260715-2al90q, STORY-260715-2w0l79, STORY-260715-1nc0nr, TASK-260715-eike3u. |
| PLAT-MAC-001 | V1 | active | STORY-260715-2pe5sa | Replicated File Provider extension + supported domain APIs. |
| PLAT-MAC-002 | V1 | active | STORY-260715-33oacu, TASK-260715-1yx9ly | TDLib hosted in companion app/agent; extension stays thin. |
| PLAT-MAC-003 | V1 | active | TASK-260715-gnsa2s | App Group durable shared state; separate-process assumption. |
| PLAT-MAC-004 | V1 | active | STORY-260715-14k4l9, STORY-260715-14n7wp, TASK-260715-gnat2x | Enumeration/change signaling + dataless/fetch/pinning + domain removal; three surfaces of one requirement. |
| PLAT-MAC-005 | V1 | active | TASK-260715-13pxnu, TASK-260715-1yx9ly | Menu-bar/settings shell + launch/background lifecycle of the TDLib host. |
| PLAT-IOS-001 | V1 | deferred-platform | STORY-260715-1hwxpw, STORY-260715-2vq8mp | Replicated extension + containing Swift app. |
| PLAT-IOS-002 | V1 | deferred-platform | STORY-260715-3mwsoi, TASK-260715-2r00ho | TDLib linked only into containing app; link-map proof; 20 MB budget as hard design constraint. |
| PLAT-IOS-003 | V1 | deferred-platform | TASK-260715-1rr041 | App Group protocol for metadata/queues/materialized content only. |
| PLAT-IOS-004 | Decision gate | deferred-platform | STORY-260715-18rqmq, TASK-260715-180uh6 | Cold-hydration spikes + ADR ratification (human gate; DEC-012). |
| PLAT-IOS-005 | V1 | deferred-platform | TASK-260715-3ja6sb | Authorization/2FA only in the containing application. |
| PLAT-IOS-006 | V1 | deferred-platform | TASK-260715-1zydwj, TASK-260715-usv9cz | Baseline measurement harness + release memory/jetsam test suite. |
| PLAT-WIN-001 | V1 | deferred-platform | STORY-260715-vf3ihw, STORY-260715-1cmz06 | Sync root/placeholders lifecycle + hydration/pin/in-sync callbacks. |
| PLAT-WIN-002 | V1 | deferred-platform | STORY-260715-2ltccr | Rust host over windows/windows-sys with owned/audited wrapper. |
| PLAT-WIN-003 | V1 | deferred-platform | TASK-260715-2sbyuy | Opaque CfAPI file identity mapped to stable shared ItemId. |
| PLAT-WIN-004 | V1 | deferred-platform | STORY-260715-1cmz06, TASK-260715-mfjal6 | Cancellation/range/read-only/restart behavior + callback dispatch semantics. |
| PLAT-WIN-005 | V1 | deferred-platform | STORY-260715-2al90q, TASK-260715-26wzmn | Packaging/registration/upgrade + clean sync-root removal. |
| PLAT-AND-001 | V1 | deferred-platform | STORY-260715-31thz2 | Kotlin DocumentsProvider with per-account root policy. |
| PLAT-AND-002 | V1 | deferred-platform | TASK-260715-1ynoya | Stable document IDs and capability flags without write/delete/move/rename. |
| PLAT-AND-003 | V1 | deferred-platform | STORY-260715-1815k1 | UniFFI/JNI runtime integration; TDLib in app/provider process with measured lifecycle. |
| PLAT-AND-004 | V1 | deferred-platform | STORY-260715-34xy5y, TASK-260715-1ynoya | Streaming/thumbnails/recreation/cancellation + persisted provider state and multi-reader queries. |
| PLAT-AND-005 | V1 | deferred-platform | TASK-260715-2odowl, TASK-260715-1vx5e4 | Keystore-backed credential protection (shared secret-store task) + background execution constraints. |
| PLAT-LNX-001 | V1 | deferred-platform | TASK-260715-3gzi1v, TASK-260715-2zmgpo | fuser adapter + long-running user service/daemon. |
| PLAT-LNX-002 | V1 | deferred-platform | TASK-260715-1za16i | Durable/reconstructible inode mapping without path identity. |
| PLAT-LNX-003 | V1 | deferred-platform | STORY-260715-2sff6a, STORY-260715-jgjoxz | lookup/readdir/getattr/statfs/xattr + open/read/release/interruption/read-only errors. |
| PLAT-LNX-004 | V1 | deferred-platform | TASK-260715-eike3u | Reference-distribution packaging and FUSE prerequisites (manual acceptance track). |
| PLAT-020 | V1 | active | TASK-260715-11qg88, TASK-260715-26eoqx | Same fixture tree through every adapter: harnesses + corpus. |
| PLAT-021 | V1 | active | TASK-260715-1ffbkg, TASK-260715-26eoqx | Cross-platform filename fixture behavior + fixture corpus coverage. |
| PLAT-022 | V1 | active | TASK-260715-1j4ij3, TASK-260715-32gjo8 | Capability types make non-normalizable behavior explicit + user docs disclose per-platform limitations. |

### SEC — security and privacy

| Requirement | Tier | Disposition | Board elements | Notes |
|---|---|---|---|---|
| SEC-001 | V1 | active | TASK-260715-3faqmr, TASK-260716-1iypv4 | CI secret scanning + credentials provisioned outside the repository (also TASK-260716-1jswke for signing assets). |
| SEC-002 | V1 | active | TASK-260715-2odowl | Platform secure-store abstractions (Keychain/Keystore/DPAPI/Secret Service). |
| SEC-003 | V1 | active | TASK-260715-1hdnuy, TASK-260715-2odowl | TDLib key configuration policy + runtime secret retrieval, never plaintext config. |
| SEC-004 | V1 | active | TASK-260715-wjaux5, TASK-260715-kxzfy7 | Documented cleanup sequence implementation + per-platform verification. |
| SEC-005 | Optional | deferred-optional | TASK-260715-1zqf0k | Scoped revocable device credentials; Telegram keys never leave service. |
| SEC-010 | V1 | active | TASK-260715-1nohav | Threat model defines FDE vs. application-level encryption per platform. |
| SEC-011 | V1 | active | TASK-260715-1nohav | Least-privilege permissions/containers as threat-model mitigations; Linux specifics in deferred TASK-260715-2zmgpo. |
| SEC-012 | V1 | active | TASK-260715-g4k3zm, TASK-260715-21clwh | Private transfer-scoped temporary files + interruption cleanup/recovery. |
| SEC-013 | V1 | active | TASK-260715-3nl3mu, TASK-260715-1nohav | Thumbnail privacy handling + same-treatment analysis for generated text. |
| SEC-014 | V1 | active | TASK-260715-1nohav | Clipboard/notifications/crash/indexing exposure review is threat-model scope. |
| SEC-020 | V1 | active | TASK-260715-4im48n | Default log redaction of content/identity/auth/URLs. |
| SEC-021 | V1 | active | TASK-260715-1nuhxj, TASK-260715-4im48n | Explicit-action diagnostic export + stable redacted identifiers. |
| SEC-022 | V1 | active | TASK-260715-4im48n | Crash/analytics never receive content or secrets; opt-in policy. |
| SEC-023 | V1 | active | TASK-260715-4im48n | Local audit of security events without secret values. |
| SEC-030 | V1 | active | TASK-260716-1iypv4, TASK-260715-pyqm1k | Product api_id/api_hash provisioning + Telegram ToS/branding compliance checklist. |
| SEC-031 | V1 | active | TASK-260715-mua1ng, TASK-260715-22fh09 | Bounded backfill scheduling honoring flood waits + retry/backoff taxonomy in fetch path. |
| SEC-032 | V1 | active | TASK-260715-23arcu | can_be_saved / protected-content / view-once capability enforcement per POL-4. |
| SEC-033 | V1 | active | TASK-260715-3s6cpe | Size/range/hash/version validation before publication. |
| SEC-034 | V1 | active | TASK-260715-1ffbkg | Untrusted-name sanitization: traversal, device names, control characters (export injection also covered by SYNC-032 renderers). |
| SEC-040 | Optional | deferred-optional | TASK-260715-3re6t2 | Dedicated remote-tier threat model before service implementation. |
| SEC-041 | Optional | deferred-optional | STORY-260715-3bs3wv | Tenant isolation and per-request authorization. |
| SEC-042 | Optional | deferred-optional | TASK-260715-3dluhs | Service credential encryption, rotation, operator access controls. |
| SEC-043 | Optional | deferred-optional | TASK-260715-2q9n2b, TASK-260715-1zqf0k | Retention/deletion/backup-deletion jobs + device-token revocation. |
| SEC-044 | Optional | deferred-optional | TASK-260715-3vekwt, STORY-260715-3bs3wv | Range authorization/link expiry on content endpoints + TLS/replay-resistant auth/rate limits. |
| SEC-050 | V1 | active | TASK-260715-32gjo8 | User-facing docs promise only accessible/saveable content. |
| SEC-051 | V1 | active | TASK-260715-1nxcst | Standing product constraint (no AI/ML training on content); no dedicated implementation task — enforced as a release-readiness review gate. |
| SEC-052 | V1 | active | TASK-260715-32gjo8, TASK-260715-1nohav | Index/Spotlight exposure documentation + threat-model analysis of OS indexing/backups. |
| SEC-053 | V1 | active | TASK-260715-2weglw, TASK-260715-152wjq | Accepted license/reuse policy (POL-6) + SBOM/license CI enforcement; legal review before branding/hosted operation. |

### NFR — non-functional requirements and release gates

| Requirement | Tier | Disposition | Board elements | Notes |
|---|---|---|---|---|
| NFR-001 | V1 | active | STORY-260715-2ufyq8 | Deterministic unit/property testing is per-task DoD across EPIC-260715-1poogc; the test-infrastructure story owns the shared tooling. |
| NFR-002 | V1 | active | TASK-260715-3e8q4m, TASK-260715-3uft8j | One conformance suite + the deterministic fake it certifies against. |
| NFR-003 | V1 | active | TASK-260715-11qg88 | Common fixture suite + native integration harnesses per adapter. |
| NFR-004 | V1 | active | TASK-260715-1zqwbz | Crash/restart fault-injection at controlled checkpoints. |
| NFR-005 | V1 | active | TASK-260715-26eoqx | Synthetic-only fixture corpus covering the specified edge categories. |
| NFR-010 | V1 | active | TASK-260715-1opnb2 | Idempotent replay at repository level; validated further by TASK-260715-1zqwbz. |
| NFR-011 | V1 | active | STORY-260715-1oq9jg | Byte-identical rendering is the story acceptance criterion. |
| NFR-012 | V1 | active | TASK-260715-3s6cpe | Fail-closed integrity; partial/stale content never published as valid. |
| NFR-013 | V1 | active | TASK-260715-18l9xz | Transactional/resumable migrations with documented rollback expectations. |
| NFR-014 | V1 | active | TASK-260715-i3mp9x, TASK-260715-11qg88 | No write/delete capability in v1 adapter + harness validation of read-only behavior; deferred adapters carry equivalents (TASK-260715-29m7as, TASK-260715-1lkypp). |
| NFR-020 | V1 | active | TASK-260715-e90vvr | First-page enumeration benchmark against the 200 ms p95 budget. |
| NFR-021 | V1 | active | TASK-260715-e90vvr | Enumeration memory bounded by page/working-set size. |
| NFR-022 | V1 | active | TASK-260715-22fh09, TASK-260715-2gkvoz | Streaming-to-disk implementation + load validation without whole-file buffering. |
| NFR-023 | V1 | deferred-platform | TASK-260715-1zydwj, TASK-260715-usv9cz | iOS extension memory budget: baseline measurement + release gate harness. |
| NFR-024 | V1 | active | TASK-260715-3nvmmu | Desktop/Android memory/startup/idle measurements during TDLib spike. |
| NFR-025 | V1 | active | STORY-260715-255sa3, TASK-260715-rhcnhc | Contract-level bounded operations + macOS callback deadline compliance. |
| NFR-030 | V1 | active | TASK-260715-3b9w8x | Structured stable error categories with actionable states. |
| NFR-031 | V1 | active | TASK-260715-g4k3zm | Transfer/sync progress survives restart where source supports resume. |
| NFR-032 | V1 | active | TASK-260715-162fdj | Health payload: cursor, last success, transfers, cache pressure, registration, redacted failures. |
| NFR-033 | V1 | active | TASK-260715-22fh09, TASK-260715-162fdj | Bounded retries in fetch coordination + observability of retry/flood-wait state. |
| NFR-034 | V1 | active | TASK-260715-1nuhxj, TASK-260715-1zqwbz | Repair operation + corruption/missing-file fixture coverage. |
| NFR-040 | V1 | active | TASK-260715-3ox001 | Minimum OS/architecture matrix recorded (POL-5/DEC-017) before implementation. |
| NFR-041 | V1 | active | TASK-260715-18l9xz, TASK-260715-1qz1g5 | SQLite/serialized/export schema migration tests + provider identity version compatibility. |
| NFR-042 | V1 | active | TASK-260715-1j4ij3 | Additive-tolerant, major-version-rejecting contract types. |
| NFR-043 | V1 | active | TASK-260715-1ffbkg | Fixture-driven cross-platform naming stability. |
| NFR-050 | V1 | active | TASK-260715-3faqmr | Formatting/lint/tests/licenses/secret scanning on every change. |
| NFR-051 | V1 | active | TASK-260715-3pwu1o | Native integration tests on signed/test environments gate platform release. |
| NFR-052 | V1 | active | TASK-260715-3bhbkv | Reproducible attributable signed artifacts + SBOM. |
| NFR-053 | V1 | active | TASK-260715-3bhbkv | Release artifacts free of sample credentials/dev sessions (AC-explicit). |

### DEC — architecture decisions

| Requirement | Tier | Disposition | Board elements | Notes |
|---|---|---|---|---|
| DEC-001 | Accepted | active | EPIC-260715-1poogc | Shared Rust drive/sync engine. |
| DEC-002 | Accepted | active | EPIC-260715-3i9uyp | Native UI/provider entry points; v1 realization is macOS. Deferred realizations: EPIC-260715-1mlv5j, EPIC-260715-y0fshx, EPIC-260715-3uynbw, EPIC-260715-1hnglv. |
| DEC-003 | Accepted | active | STORY-260715-255sa3 | Provider-neutral DriveSource contract with shared conformance. |
| DEC-004 | Accepted | active | EPIC-260715-2ptb18 | Local TDLib preferred for desktop (Android epic deferred). |
| DEC-005 | Provisional | deferred-optional | EPIC-260715-2y4q0r | Optional remote gotd/td source preserved. |
| DEC-006 | Accepted | deferred-platform | STORY-260715-3mwsoi, TASK-260715-2r00ho | No TDLib in iOS extension; link-map proof of absence. |
| DEC-007 | Accepted | active | TASK-260715-i3mp9x, TASK-260715-11qg88 | Read-only V1 enforced at capability surface + validated by harnesses (same coverage as NFR-014). |
| DEC-008 | Accepted | active | TASK-260715-1qz1g5 | Path/title/order-independent stable IDs. |
| DEC-009 | Accepted | active | STORY-260715-1oq9jg, TASK-260715-1ceq7h | Deterministic derived views + canonical structured store schema. |
| DEC-010 | Accepted | active | EPIC-260715-3i9uyp | Native provider APIs (File Provider v1); CfAPI/DocumentsProvider/FUSE in deferred epics. |
| DEC-011 | Provisional | active | EPIC-260715-3i9uyp, EPIC-260715-1mlv5j | macOS-then-Windows vertical slices; board dependencies encode the order, Windows itself deferred until it enters scope (POL-5). |
| DEC-012 | Open release gate | deferred-platform | STORY-260715-18rqmq, TASK-260715-180uh6 | iOS cold-hydration strategy selection before iOS release. |
| DEC-013 | Accepted | active | TASK-260715-2cl112, TASK-260715-1jmsdp | Decision record (done) + implementing ordering-projection task (POL-1). |
| DEC-014 | Accepted | active | TASK-260715-240bpy, TASK-260715-11abx8 | Decision record (done) + cache/quota/pinning engine implementing 10 GB LRU + Archive Mode semantics (POL-2). |
| DEC-015 | Accepted | active | TASK-260715-287x8t, TASK-260715-37nhe5 | Decision record (done) + edit/delete retention mapping (POL-3). |
| DEC-016 | Accepted | active | TASK-260715-3prhsi, TASK-260715-23arcu | Decision record (done) + protected-content capability enforcement (POL-4). |
| DEC-017 | Accepted | active | TASK-260715-3ox001, TASK-260715-3pwu1o | Decision record (done) + CI support-matrix enforcement (POL-5). |
| DEC-018 | Accepted | active | TASK-260715-2weglw, TASK-260715-152wjq | Decision record (done) + SBOM/license scanning enforcement (POL-6). |
| DEC-019 | Accepted | active | TASK-260715-7pdgft, TASK-260717-3dvved | Decision record (done) + GramDrive name application across specs/docs/identifiers (POL-7). |
| DEC-020 | Accepted | active | TASK-260715-3rhlh6, TASK-260715-1nxcst | Decision record (done) + the release-readiness review that operates the single human gate (POL-8). |
| DEC-021 | Accepted | active | TASK-260715-265gqq, TASK-260715-152wjq | Named MPL-2.0 (uniffi*) / Unicode-3.0 (unicode-ident) exceptions enforced as per-crate `[licenses.exceptions]` entries in deny.toml at the UniFFI boundary + license scanning in core CI (narrows POL-6/DEC-018). |

### POL — product policies

POL rows are the detailed form of DEC-013..DEC-020; they map to the same enforcing elements plus policy-specific implementation tasks. DEC-021 has no POL row of its own — it is a named exception inside POL-6.

| Requirement | Tier | Disposition | Board elements | Notes |
|---|---|---|---|---|
| POL-1 | Accepted | active | TASK-260715-1jmsdp, TASK-260715-2cl112 | Stable names + order.json projection (details of DEC-013). |
| POL-2 | Accepted | active | TASK-260715-11abx8, TASK-260715-mua1ng | Quota/pin/eviction engine + eager-backfill scheduling for Archive Mode (details of DEC-014; decision record TASK-260715-240bpy). |
| POL-3 | Accepted | active | TASK-260715-37nhe5, TASK-260715-287x8t | Mirror/Audit retention mapping + decision record (details of DEC-015). |
| POL-4 | Accepted | active | TASK-260715-23arcu, TASK-260715-3nl3mu | Protected/view-once capability enforcement incl. restriction-aware thumbnails (details of DEC-016; decision record TASK-260715-3prhsi). |
| POL-5 | Accepted | active | TASK-260715-3ox001, TASK-260715-3pwu1o | macOS 14+ arm64 support matrix + CI jobs pinned to it (details of DEC-017). |
| POL-6 | Accepted | active | TASK-260715-152wjq, TASK-260715-3faqmr | License/SBOM enforcement in dependency controls and core CI (details of DEC-018; named exceptions per DEC-021; decision record TASK-260715-2weglw). |
| POL-7 | Accepted | active | TASK-260717-3dvved, TASK-260715-7pdgft | GramDrive application task + decision record (details of DEC-019). |
| POL-8 | Accepted | active | TASK-260715-1nxcst, TASK-260715-3rhlh6 | Release-gate review operation + approval-boundary definition (details of DEC-020). |

## Known tensions and observations

1. **product.md success gate vs. POL-5.** `product.md` ("Product success gates") declares V1 product-complete only when at least macOS **and Windows** pass the native gates, while DEC-017/POL-5 commit only macOS 14+ arm64 for v1 and defer Windows. The board follows POL-5 (Windows epic deferred). One of the two texts should be reconciled; tracked in `docs/OPEN_QUESTIONS.md`.
2. **SEC-051 has no implementing task.** It is a standing negative constraint (never train AI/ML on Telegram content); mapped to the release-readiness review gate rather than an implementation task. Acceptable, but any future analytics/telemetry work must re-check it.
3. **Board scope hints are ranges.** Several board READMEs cite ranges ("SYNC-040 through SYNC-054"); this matrix is the per-ID authority. The validation script also verifies that every requirement ID mentioned in board READMEs exists in `.spec/`.
