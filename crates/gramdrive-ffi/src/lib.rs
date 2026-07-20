//! GramDrive FFI boundary — the only crate native consumers link.
//!
//! The UniFFI-exposed contract lives in [`api`] — provider-neutral
//! asynchronous operations, records, errors, cancellation, and progress for
//! Swift (File Provider hosts) and Kotlin (DocumentsProvider) — and in
//! [`shared_state`], the multi-process shared durable state surface
//! (layout, role-based open, snapshot reads, change probing, corruption
//! recovery). Binding style, generation pipeline, threading/async model,
//! callback dispatch rules, and the versioning policy are documented in
//! this crate's README.md. Artifact packaging is owned by TASK-260715-3akqs8. Windows
//! and Linux hosts bypass the generated bindings and use this crate as a
//! plain Rust dependency.
//!
//! Boundary rules (enforced by `.scripts/check_crate_architecture.py`):
//! - internal dependencies: any core crate (this is the top of the graph);
//!   nothing inside the workspace may depend on this crate;
//! - the exported API stays provider-neutral: no Telegram/TDLib/gotd types
//!   and no OS-specific types cross this boundary (DEC-003);
//! - no platform-specific dependencies or `cfg(target_os/windows/unix)` code.

#![deny(unsafe_code)]

// Emits the `extern "C"` scaffolding the generated Swift/Kotlin bindings
// call into, under the `gramdrive` namespace (POL-7: shipped identifiers
// carry the product name, not the codename).
uniffi::setup_scaffolding!("gramdrive");

pub mod api;
pub mod auth;
pub mod removal;
pub mod shared_state;

pub use gramdrive_engine as engine;
pub use gramdrive_model as model;

#[cfg(test)]
mod tests {
    #[test]
    fn re_exports_engine_and_model() {
        let range = crate::model::ByteRange::new(0, 4).expect("valid range");
        let via_engine = crate::engine::model::ByteRange::new(0, 4).expect("valid range");
        assert_eq!(range, via_engine);
    }
}
