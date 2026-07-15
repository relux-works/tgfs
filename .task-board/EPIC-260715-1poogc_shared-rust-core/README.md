# Shared Rust drive core

## Description
Implement the provider-neutral domain, synchronization, cache, rendering, and binding engine reused by every platform.

## Scope
Rust workspace, UniFFI API, model/state/transfer/rendering modules, conformance fixtures, and platform-neutral diagnostics.

## Acceptance Criteria
Core satisfies DOM/SYNC requirements, passes fake and real source conformance suites, contains no OS-provider or Telegram-specific policy leakage, and builds for all selected targets.
