# Workspace and language bindings

## Description
Establish the Rust workspace, crate boundaries, supported toolchain, UniFFI surface, and build artifacts.

## Scope
Core crates, feature flags, generated Swift/Kotlin bindings, C ABI boundaries, and developer commands.

## Acceptance Criteria
Workspace builds reproducibly for initial targets; public FFI types are versioned and minimal; sample Swift/Kotlin consumers pass smoke tests.
