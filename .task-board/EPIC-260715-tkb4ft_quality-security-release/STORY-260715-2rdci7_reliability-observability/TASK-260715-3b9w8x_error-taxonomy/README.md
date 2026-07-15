# Define and implement stable error taxonomy

## Description
Normalize source, network, auth, Telegram, storage, integrity, version, cancellation, provider, and unsupported errors.

## Scope
Rust API, UniFFI mapping, native UX, and remote protocol.

## Acceptance Criteria
Every known failure maps to stable category/action; unknowns degrade safely; round-trip tests preserve meaning.
