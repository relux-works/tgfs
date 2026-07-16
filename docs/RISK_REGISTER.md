# Risk Register

Last updated: 2026-07-17

| ID | Risk | Likelihood | Impact | Mitigation / decision gate |
|---|---|---:|---:|---|
| R-001 | iOS cannot cold-hydrate local-first content while the containing app is unavailable. | High | High | Treat as explicit release gate: open-app UX, remote source, or separately proven minimal fetcher. |
| R-002 | TDLib/Rust/UniFFI packaging, binary size, async cancellation, or multi-process behavior differs materially by platform. | Medium | High | Complete shared-core plus Apple/Android packaging spikes before broad implementation. |
| R-003 | CfAPI callback/state complexity exceeds available Rust wrapper quality. | High | High | Own the wrapper over `windows-rs`; vertical-slice restart/cancel/delete/read-only tests early. |
| R-004 | Telegram rate limits, file-reference expiry, CDN behavior, or account observation make large histories slow/unreliable. | High | High | Bounded scheduler, durable cursors, flood-wait handling, refresh/retry taxonomy, synthetic and test-account load fixtures. |
| R-005 | TDLib normal history crawl is too slow for very large local-first accounts. | Medium | Medium | Metadata-first placeholders; optional desktop Takeout importer or remote tier after measured spike. |
| R-006 | Telegram ordering changes cause disruptive folder renames. | High | Medium | Stable identity plus selectable stable-name/`order.json` versus numeric-prefix mode. |
| R-007 | Cross-platform filename/case/normalization differences break parity. | High | Medium | Strict common sanitizer, collision fixture corpus, path-independent IDs. |
| R-008 | Generated exports become huge or expensive to regenerate. | Medium | Medium | Monthly partitions, input watermarks, deterministic incremental rendering, bounded pages/ranges. |
| R-009 | Local cache exposes sensitive chat content through logs, temp files, indexing, or backups. | Medium | High | Security spec, container permissions, redaction tests, indexing policy, secure cleanup and threat model. |
| R-010 | Hosted tier creates unacceptable Telegram-key custody and privacy liability. | Medium | Critical | Keep optional; require separate threat model, legal/privacy program, tenant isolation, key management, deletion and incident response before implementation. |
| R-011 | Telegram API/terms or client behavior changes. | Medium | High | Isolate source adapters, schema/version monitoring, compliance review, explicit degraded/unsupported state. |
| R-012 | GPL/AGPL reference code is copied into a proprietary product. | Low | High | Reference-only rule, dependency/license scanning, legal review before reuse. |
| R-013 | Provider/database migrations orphan stable IDs or placeholders. | Medium | High | Versioned identity scheme, migration fixtures, restart/upgrade tests, repair/reconciliation command. |
| R-014 | Read-only intent is bypassed by OS/client behavior. | Medium | Medium | Do not advertise capabilities; stable errors; integration tests for create/write/rename/move/delete attempts. |
| R-015 | Project name conflicts with existing `TheodoreKrypton/tgfs`. | High | Medium | Mitigated by DEC-019/POL-7: public name is GramDrive (`com.reluxworks.gramdrive.*`); `tgfs` is retained as the private repo/codename only and is never user-visible. Residual: formal trademark and handle/domain acquisition check before public release. |

Risk owners are assigned in task-board during implementation planning. Critical/high risks must have explicit spike or control tasks before their dependent implementation can enter development.
