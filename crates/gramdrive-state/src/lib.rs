//! GramDrive state store — durable local metadata in SQLite.
//!
//! This crate owns the versioned SQLite schema (TASK-260715-1ceq7h), its
//! application and its forward-only migrations (TASK-260715-18l9xz), the
//! typed repositories over it (TASK-260715-1opnb2, [`repo`]), and will grow
//! startup reconciliation (TASK-260715-21clwh) — STORY-260715-16ik2x.
//! Transactions are short and state is durable: on Apple platforms the app
//! and the File Provider extension are separate processes sharing this
//! database in WAL mode, so no in-memory state is authoritative.
//!
//! # What the schema stores
//!
//! Canonical source facts (accounts, chats, list order, the append-only
//! POL-3 message event log with its `messages` projection, attachments,
//! blobs), the provider projection (`items` — every provider-visible node
//! under its stable [`model::identity::ItemId`], canonical and appearance
//! rows alike, DEC-008/DOM-002), and the engine state that hangs off those
//! keys: transfers, cache entries and pins, change cursors, per-chat sync
//! windows, and render state. The full rationale lives in
//! `src/schema/v1.sql`, which is the schema.
//!
//! Boundary rules (enforced by `.scripts/check_crate_architecture.py`):
//! - internal dependencies: `gramdrive-model` only;
//! - no platform-specific dependencies or `cfg(target_os/windows/unix)` code
//!   (the database *location* is chosen by the embedding host, not here);
//! - no Telegram/provider types — persistence speaks the model vocabulary.

#![deny(unsafe_code)]

pub use gramdrive_model as model;

mod error;
mod migrate;
mod repair;
pub mod repo;
mod schema;
mod store;

pub use error::StateError;
pub use migrate::{BASELINE_VERSION, ChunkFn, ChunkOutcome, Migration, MigrationStep};
pub use repair::{RepairKind, RepairMarker};
pub use repo::{ReadTxn, WriteTxn};
pub use schema::SCHEMA_VERSION;
pub use store::StateStore;

#[cfg(test)]
mod tests {
    #[test]
    fn re_exports_model_vocabulary() {
        let range = crate::model::ByteRange::new(0, 4).expect("valid range");
        assert_eq!(range.len(), 4);
    }
}
