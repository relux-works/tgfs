# iOS Rust and TDLib packaging

## Description
Package shared Rust core for app and extension targets while linking TDLib only into the containing application.

## Scope
XCFramework slices, UniFFI Swift, target linkage, symbols, size, and version compatibility.

## Acceptance Criteria
Extension binary has no TDLib linkage; app/core load on supported devices; artifacts are versioned, signed, and measured.
