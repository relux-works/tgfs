//! GramDrive transfer and cache engine — the orchestration layer of the core.
//!
//! This crate owns hydration, pin/offline state, resumable ranged
//! downloads, integrity checks and cache promotion, quota accounting, and
//! LRU eviction of unpinned content (STORY-260715-2hs8cf, POL-2). It drives
//! any `DriveSource` through the provider-neutral contract and persists
//! durable transfer state through `gramdrive-state`.
//!
//! Implemented so far: the durable transfer state machine ([`transfer`],
//! TASK-260715-g4k3zm) — request/claim/progress/checkpoint and the
//! terminal gates over the journal `gramdrive-state` persists; the ranged
//! fetch coordinator ([`fetch`], TASK-260715-22fh09), which drives a
//! `DriveSource` through those claims: reader coalescing, aligned chunking,
//! bounded parallelism, locator refresh, and streaming to reader sinks; and
//! integrity verification with atomic content-addressed promotion
//! ([`cache`], TASK-260715-3s6cpe), which layers over
//! [`transfer::CompleteOutcome::Promoted`] to hash, verify, dedup, and
//! publish finished bytes into the cache store; and cache accounting, quota
//! enforcement, and LRU eviction ([`cache::Evictor`], TASK-260715-11abx8),
//! which govern how much of that cache lives — pinned and Archive-Mode
//! content quota-exempt, eviction of eligible unpinned content only, never
//! racing an open read or a live transfer (POL-2, SYNC-050..054).
//!
//! The incremental render planner ([`render_plan`], TASK-260715-22l8zy)
//! computes, from a batch of normalized changes and the renderer/schema
//! versions, which generated documents are stale and a plan to regenerate
//! exactly those against the current event watermark — the marking and planning
//! that sit in front of `gramdrive-render`'s pure renderers and
//! `gramdrive-state`'s atomic publication.
//!
//! The metadata-first backfill scheduler ([`backfill`], TASK-260715-mua1ng)
//! is the provider-neutral policy that composes the source's sans-IO history
//! machines: it orders history work by visible-item priority, paces provider
//! requests against a durable flood-wait budget, gates background metadata
//! and eager Archive-Mode media on device power/network/disk conditions, and
//! stays durable, bounded, observable, and user-pausable — never mirroring
//! media eagerly by default (POL-2, SYNC-020/021).
//!
//! Boundary rules (enforced by `.scripts/check_crate_architecture.py`):
//! - internal dependencies: `gramdrive-model`, `gramdrive-source`,
//!   `gramdrive-state`, and `gramdrive-render`;
//! - no platform-specific dependencies or `cfg(target_os/windows/unix)` code;
//! - no direct provider (TDLib/gotd) types — only the source contract.

#![deny(unsafe_code)]

pub use gramdrive_model as model;
pub use gramdrive_render as render;
pub use gramdrive_source as source;
pub use gramdrive_state as state;

pub mod backfill;
pub mod cache;
pub mod fetch;
pub mod render_pipeline;
pub mod render_plan;
pub mod transfer;

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
        let via_render = crate::render::model::ByteRange::new(0, 4).expect("valid range");
        assert_eq!(via_render, range);
    }
}
