# Define Rust workspace and crate boundaries

## Description
Create the Rust workspace architecture for model, source contract, state, render, transfer/cache, FFI, and test support.

## Scope
Cargo manifests, dependency direction rules, feature policy, and architecture checks.

## Acceptance Criteria
No dependency cycles or platform leakage; each crate has documented ownership and test command; workspace compiles on reference host.
