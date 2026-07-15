# Define source contract types and errors

## Description
Implement item/page/cursor/version/range/capability/progress/error types without backend leakage.

## Scope
Rust API and UniFFI-safe representation where exposed.

## Acceptance Criteria
Types encode invalid-state prevention, are serializable/versioned where durable, and cover specified failure taxonomy.
