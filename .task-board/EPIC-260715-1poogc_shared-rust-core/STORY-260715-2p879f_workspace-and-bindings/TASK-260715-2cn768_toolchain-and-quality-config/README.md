# Pin Rust toolchain and quality configuration

## Description
Define rustfmt, clippy, deny/audit/license, test, and build profiles.

## Scope
Repository-local developer and CI configuration without product behavior.

## Acceptance Criteria
Commands are documented, deterministic, and fail on formatting, denied lints, forbidden licenses, or known critical vulnerabilities according to policy.
