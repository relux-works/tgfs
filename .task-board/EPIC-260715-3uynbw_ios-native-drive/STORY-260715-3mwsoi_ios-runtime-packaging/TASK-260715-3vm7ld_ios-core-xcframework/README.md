# Package Rust core XCFramework

## Description
Build device/simulator slices and generated Swift API suitable for app and memory-constrained extension.

## Scope
Feature split so extension excludes unnecessary source/runtime functionality.

## Acceptance Criteria
Both targets compile and load; extension feature set is minimal; artifact/version mismatch tests fail safely.
