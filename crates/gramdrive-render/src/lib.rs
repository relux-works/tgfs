//! GramDrive renderers — deterministic NDJSON and Markdown projections of
//! chat history.
//!
//! This crate will own the lossless `messages.ndjson` renderer, the
//! human-readable Markdown renderer, and the incremental render planner
//! (STORY-260715-1oq9jg). Rendering is a pure function of canonical records:
//! the same input always produces byte-identical output, so generated files
//! are reproducible views, never a second source of truth.
//!
//! Boundary rules (enforced by `.scripts/check_crate_architecture.py`):
//! - internal dependencies: `gramdrive-model` only;
//! - no platform-specific dependencies or `cfg(target_os/windows/unix)` code;
//! - no I/O policy — callers decide where rendered bytes are written.

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
