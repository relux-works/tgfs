# Security and Privacy Specification

Status: planning baseline
Last updated: 2026-07-15

## Security boundary

Telegram user sessions are account-equivalent credentials. Local-first keeps them per device. The optional remote tier centralizes them and therefore has a materially larger custody, breach, and operator-access risk.

## Credential requirements

- **SEC-001 (V1):** Never commit `api_id`, `api_hash`, phone numbers, auth keys, session databases, tokens, or user content to source control.
- **SEC-002 (V1):** Protect platform credential-encryption keys with Keychain/Secure Enclave facilities where appropriate, Android Keystore, Windows credential/DPAPI facilities, and Linux Secret Service or a documented encrypted fallback.
- **SEC-003 (V1):** TDLib database encryption keys and product credentials are retrieved at runtime and are not stored in plaintext configuration.
- **SEC-004 (V1):** Logout/account removal has a documented secure cleanup sequence for credentials, session/database files, provider registrations, partial transfers, and cached content.
- **SEC-005 (Optional tier):** Remote access uses short/scoped or revocable per-device product credentials; Telegram auth keys never leave the service boundary.

## Local data

- **SEC-010 (V1):** Define which metadata and content rely on full-disk encryption versus application-level encryption per platform.
- **SEC-011 (V1):** Use least-privilege filesystem permissions and application containers.
- **SEC-012 (V1):** Temporary/partial files are private, named without sensitive chat titles where practical, and removed or recovered after interruption.
- **SEC-013 (V1):** Thumbnails and generated text receive the same privacy treatment as original content.
- **SEC-014 (V1):** Clipboard, notifications, crash reports, and OS indexing exposure are explicitly reviewed and configurable where required.

## Logging and diagnostics

- **SEC-020 (V1):** Logs exclude message text, filenames when avoidable, phone numbers, usernames, auth material, raw Telegram payloads, and signed download URLs by default.
- **SEC-021 (V1):** Diagnostic exports use stable redacted identifiers and require explicit user action.
- **SEC-022 (V1):** Crash/analytics systems never receive Telegram content or secrets; analytics are opt-in where legally/product appropriate.
- **SEC-023 (V1):** Security-sensitive events are auditable locally without logging secret values.

## Protocol and abuse controls

- **SEC-030 (V1):** Use a product-specific Telegram `api_id`/`api_hash` and comply with Telegram API terms and branding requirements.
- **SEC-031 (V1):** Bound request concurrency; handle flood waits, exponential/backoff policy, takeout security delays, and account restrictions without retry storms.
- **SEC-032 (V1):** Respect protected-content, self-destruct/view-once, and `can_be_saved` restrictions.
- **SEC-033 (V1):** Validate remote item metadata, sizes, ranges, hashes, and versions before filesystem publication.
- **SEC-034 (V1):** Treat filenames and message-derived strings as untrusted input; prevent traversal, device-name abuse, control-character injection, and archive/export injection.

## Optional remote tier

- **SEC-040 (Optional tier):** Perform a dedicated threat model before service implementation or hosted deployment.
- **SEC-041 (Optional tier):** Separate tenants/accounts cryptographically and logically; enforce authorization on every metadata/content request.
- **SEC-042 (Optional tier):** Encrypt service-side credentials with a managed key strategy and define operator-access controls, rotation, backup, and incident response.
- **SEC-043 (Optional tier):** Support account export/deletion, data retention, backup deletion, and device-token revocation.
- **SEC-044 (Optional tier):** Use TLS, replay-resistant authentication, range-request authorization, rate limits, and download-link expiration.

## Privacy and compliance

- **SEC-050 (V1):** Product messaging promises only content currently accessible and saveable through the authorized account/source.
- **SEC-051 (V1):** Do not use aggregated Telegram content to train or develop AI/ML systems.
- **SEC-052 (V1):** Document local index/Spotlight/search exposure and allow users to understand what the OS may index.
- **SEC-053 (V1):** Complete legal review before copying GPL/AGPL implementation code, shipping branding, or operating a hosted service.

## Required security artifacts before release

1. Local-first threat model.
2. Credential and logout data-flow review per platform.
3. Logging/redaction test fixtures.
4. Dependency/license/SBOM process.
5. Security contact and vulnerability handling policy.
6. Remote-tier threat model and privacy program before that tier ships.
