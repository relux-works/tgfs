//! GramDrive state store — durable local metadata in SQLite.
//!
//! This crate will own the SQLite schema and migrations, repositories over
//! items/messages/cursors/pins, and startup reconciliation
//! (STORY-260715-16ik2x). Transactions are short and state is durable:
//! on Apple platforms the app and the File Provider extension are separate
//! processes sharing this database, so no in-memory state is authoritative.
//!
//! Boundary rules (enforced by `.scripts/check_crate_architecture.py`):
//! - internal dependencies: `gramdrive-model` only;
//! - no platform-specific dependencies or `cfg(target_os/windows/unix)` code
//!   (the database *location* is chosen by the embedding host, not here);
//! - no Telegram/provider types — persistence speaks the model vocabulary.

#![deny(unsafe_code)]

pub use gramdrive_model as model;

#[cfg(test)]
mod tests {
    #[test]
    fn re_exports_model_vocabulary() {
        let range = crate::model::ByteRange::new(0, 4).expect("valid range");
        assert_eq!(range.len(), 4);
    }
}
