//! GramDrive renderers — deterministic JSON, NDJSON, and Markdown projections
//! of chat state and history.
//!
//! This crate owns the lossless `messages.ndjson` renderer ([`ndjson`]) and the
//! human-readable monthly Markdown renderer ([`markdown`]). Both renderers are
//! projections of one shared record set ([`record`]): rendering is a pure
//! function of canonical records, so the same input always produces
//! byte-identical output and generated files are reproducible views, never a
//! second source of truth (DOM-006).
//!
//! The [`civil`] module is the one instant-to-calendar computation both the
//! Markdown day grouping and the incremental render planner (the engine,
//! STORY-260715-1oq9jg) share, so the month a change is planned into is always
//! the month the renderer would group it under.
//!
//! Boundary rules (enforced by `.scripts/check_crate_architecture.py`):
//! - internal dependencies: `gramdrive-model` only;
//! - no platform-specific dependencies or `cfg(target_os/windows/unix)` code;
//! - no I/O policy — callers decide where rendered bytes are written.

#![forbid(unsafe_code)]

pub use gramdrive_model as model;

mod json;
mod record;

pub mod chat_json;
pub mod civil;
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
