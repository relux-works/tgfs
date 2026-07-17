//! GramDrive renderers — deterministic NDJSON and Markdown projections of
//! chat history.
//!
//! This crate owns the lossless `messages.ndjson` renderer ([`ndjson`]) and the
//! human-readable monthly Markdown renderer ([`markdown`]); the incremental
//! render planner (STORY-260715-1oq9jg) is still to come. Both renderers are
//! projections of one shared record set ([`record`]): rendering is a pure
//! function of canonical records, so the same input always produces
//! byte-identical output and generated files are reproducible views, never a
//! second source of truth (DOM-006).
//!
//! Boundary rules (enforced by `.scripts/check_crate_architecture.py`):
//! - internal dependencies: `gramdrive-model` only;
//! - no platform-specific dependencies or `cfg(target_os/windows/unix)` code;
//! - no I/O policy — callers decide where rendered bytes are written.

#![forbid(unsafe_code)]

pub use gramdrive_model as model;

mod json;
mod record;

pub mod markdown;
pub mod ndjson;

#[cfg(test)]
mod tests {
    #[test]
    fn re_exports_model_vocabulary() {
        let range = crate::model::ByteRange::new(0, 4).expect("valid range");
        assert_eq!(range.len(), 4);
    }
}
