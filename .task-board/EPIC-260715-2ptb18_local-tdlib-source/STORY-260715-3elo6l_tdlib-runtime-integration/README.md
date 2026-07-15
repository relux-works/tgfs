# TDLib runtime and FFI integration

## Description
Package TDLib and implement safe tdjson client lifecycle, request correlation, ordered update receipt, and shutdown.

## Scope
Rust wrapper, build/link configuration, database paths/keys, threads, allocators, and logging.

## Acceptance Criteria
Repeated create/authorize/close passes leak/lifecycle tests; responses correlate correctly; updates apply in order; platform artifacts link.
