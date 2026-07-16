//! GramDrive transfer and cache engine — the orchestration layer of the core.
//!
//! This crate will own hydration, pin/offline state, resumable ranged
//! downloads, integrity checks and cache promotion, quota accounting, and
//! LRU eviction of unpinned content (STORY-260715-2hs8cf, POL-2). It drives
//! any `DriveSource` through the provider-neutral contract and persists
//! durable transfer state through `gramdrive-state`.
//!
//! Boundary rules (enforced by `.scripts/check_crate_architecture.py`):
//! - internal dependencies: `gramdrive-model`, `gramdrive-source`,
//!   `gramdrive-state`, and `gramdrive-render` (allowed, not yet used);
//! - no platform-specific dependencies or `cfg(target_os/windows/unix)` code;
//! - no direct provider (TDLib/gotd) types — only the source contract.

#![deny(unsafe_code)]

pub use gramdrive_model as model;
pub use gramdrive_source as source;
pub use gramdrive_state as state;

#[cfg(test)]
mod tests {
    #[test]
    fn re_exports_lower_layers() {
        let range = crate::model::ByteRange::new(0, 4).expect("valid range");
        assert_eq!(range.len(), 4);
        let via_source = crate::source::model::ByteRange::new(0, 4).expect("valid range");
        assert_eq!(via_source, range);
        let via_state = crate::state::model::ByteRange::new(0, 4).expect("valid range");
        assert_eq!(via_state, range);
    }
}
