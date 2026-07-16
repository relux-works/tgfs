//! GramDrive test support — deterministic fixtures shared across the core.
//!
//! This crate will own the deterministic fake `DriveSource`
//! (TASK-260715-3uft8j), the source conformance suite (TASK-260715-3e8q4m),
//! and shared fixture trees, including the cross-platform filename fixtures
//! required by PLAT-021.
//!
//! Boundary rules (enforced by `.scripts/check_crate_architecture.py`):
//! - internal dependencies: `gramdrive-model`, `gramdrive-source`;
//! - product crates may use this crate **only** as a `dev-dependency`;
//!   it never ships inside a product artifact;
//! - no platform-specific dependencies or `cfg(target_os/windows/unix)` code.

#![forbid(unsafe_code)]

pub use gramdrive_model as model;
pub use gramdrive_source as source;

#[cfg(test)]
mod tests {
    #[test]
    fn re_exports_model_and_source() {
        let range = crate::model::ByteRange::new(0, 4).expect("valid range");
        let via_source = crate::source::model::ByteRange::new(0, 4).expect("valid range");
        assert_eq!(range, via_source);
    }
}
