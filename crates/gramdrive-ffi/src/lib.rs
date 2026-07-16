//! GramDrive FFI boundary — the only crate native consumers link.
//!
//! This crate will own the UniFFI-exposed API surface: provider-neutral
//! asynchronous operations, records, errors, cancellation, and progress for
//! Swift (File Provider hosts) and Kotlin (DocumentsProvider). The UniFFI
//! wiring, UDL/proc-macro choice, and generation pipeline are defined by
//! TASK-260715-265gqq; artifact packaging by TASK-260715-3akqs8. Windows and
//! Linux hosts bypass the generated bindings and use this crate as a plain
//! Rust dependency.
//!
//! Boundary rules (enforced by `.scripts/check_crate_architecture.py`):
//! - internal dependencies: any core crate (this is the top of the graph);
//!   nothing inside the workspace may depend on this crate;
//! - the exported API stays provider-neutral: no Telegram/TDLib/gotd types
//!   and no OS-specific types cross this boundary (DEC-003);
//! - no platform-specific dependencies or `cfg(target_os/windows/unix)` code.

#![deny(unsafe_code)]

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
