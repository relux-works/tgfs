//! GramDrive `DriveSource` contract — the provider-neutral boundary between
//! the drive core and any Telegram (or other) backend.
//!
//! This crate will own the asynchronous `DriveSource` trait: root/children
//! enumeration, change cursors, ranged content fetch, thumbnails, retry
//! classification, and cancellation semantics (defined by
//! TASK-260715-1j4ij3). Implementations live in separate crates — never here
//! and never behind feature flags of this crate (DEC-003/DEC-005):
//!
//! - `gramdrive-source-tdjson` (future) — local TDLib via tdjson FFI;
//! - `gramdrive-source-remote` (future) — remote gotd/td service over HTTPS;
//! - `gramdrive-testkit` — deterministic fake source for conformance tests.
//!
//! Boundary rules (enforced by `.scripts/check_crate_architecture.py`):
//! - internal dependencies: `gramdrive-model` only;
//! - no platform-specific dependencies or `cfg(target_os/windows/unix)` code;
//! - no TDLib/gotd/MTProto types in the public API.

#![forbid(unsafe_code)]

pub use gramdrive_model as model;

#[cfg(test)]
mod tests {
    #[test]
    fn re_exports_model_vocabulary() {
        let range = crate::model::ByteRange::new(0, 4).expect("valid range");
        assert_eq!(range.len(), 4);
    }
}
