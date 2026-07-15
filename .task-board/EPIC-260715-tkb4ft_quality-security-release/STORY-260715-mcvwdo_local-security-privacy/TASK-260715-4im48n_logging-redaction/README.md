# Implement logging and diagnostic redaction policy

## Description
Create structured redacted identifiers, safe error contexts, diagnostic export, and tests preventing content/secret leakage.

## Scope
Rust, TDLib/gotd logs, Swift/Kotlin/Windows/Linux layers, crash analytics.

## Acceptance Criteria
Fixtures prove credentials, phone/usernames, chat text, filenames, raw payloads, and signed URLs are absent by default.
