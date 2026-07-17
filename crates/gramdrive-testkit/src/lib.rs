//! GramDrive test support — deterministic fixtures shared across the core.
//!
//! This crate owns the deterministic fake `DriveSource`
//! (TASK-260715-3uft8j) and the one source conformance suite
//! (TASK-260715-3e8q4m), and will own shared fixture trees, including the
//! cross-platform filename fixtures required by PLAT-021.
//!
//! # What is here
//!
//! - [`conformance`] — the backend-agnostic suite every `DriveSource`
//!   implementation must pass (SYNC-002, NFR-002), and the fake's harness
//!   for it;
//! - [`SourceScript`] / [`ScriptBuilder`] — a whole backend written down:
//!   a base tree, the bytes behind each file, the change batches that move
//!   it forward, and the faults that interrupt any of it ([`script`]);
//! - [`FakeSource`] — the machine that plays a script against the real
//!   contract ([`fake`]);
//! - [`Fault`] / [`Occurrence`] / [`Effect`] — scripted delays, failures,
//!   and version races ([`fault`]);
//! - [`Interaction`] / [`Call`] / [`Outcome`] — what was asked, and how it
//!   ended, cancellation included ([`record`]);
//! - [`RecordingSink`] — a `ContentSink` that verifies the delivery
//!   contract while collecting bytes ([`sink`]);
//! - [`exec`] — a single-threaded executor with no runtime behind it;
//! - [`fixture`] — identity and item constructors for writing scripts.
//!
//! # Determinism is the whole product
//!
//! Every fake is a lie about a real system; the question is which lie is
//! useful. This one drops exactly one property of a real backend — that
//! things happen on their own — and keeps the rest. Nothing here reads a
//! clock, spawns a thread, or draws entropy from anywhere but the script's
//! seed. The backend changes when [`FakeSource::advance`] is called and at
//! no other time; a "delay" is a stated number of yields rather than a
//! duration ([`fault`]); chunk boundaries come from the seed.
//!
//! What that buys is worth the trade: a test can pose a version race, a
//! flood wait, a mid-fetch cancellation, or a rejected page token, hit the
//! same code path on every run, and read back exactly what the source was
//! asked. What it gives up is asking this fake what happens under load — it
//! has no load, and a fixture that pretended otherwise would be a flake
//! with a seed field.
//!
//! ```
//! use gramdrive_testkit::{FakeSource, RecordingSink, SourceScript, exec, fixture};
//! use gramdrive_testkit::model::ByteRange;
//! use gramdrive_testkit::model::version::ContentVersion;
//! use gramdrive_testkit::source::{DirectoryKind, DriveSource, FetchRequest, FileKind};
//!
//! let scope = fixture::scope();
//! let root = fixture::account_root_id(scope);
//! let chat = fixture::chat_id(scope, 100);
//! let photo = fixture::attachment_id(scope, 100, 5, 0);
//!
//! let script = SourceScript::builder(scope)
//!     .item(fixture::directory(root.clone(), None, "Account", "m1", DirectoryKind::Root)?)
//!     .item(fixture::directory(chat.clone(), Some(root), "Team", "m2", DirectoryKind::Chat)?)
//!     .item(fixture::file(photo.clone(), chat, "photo.jpg", "m3", "c1", 11, FileKind::Attachment)?)
//!     .content(&photo, ContentVersion::new("c1")?, *b"hello world")
//!     .build()?;
//!
//! let source = FakeSource::new(script);
//! let range = ByteRange::new(0, 11)?;
//! let mut sink = RecordingSink::new(range);
//! exec::drive(source.fetch(
//!     FetchRequest { item: photo, version: ContentVersion::new("c1")?, range },
//!     &mut sink,
//! ))?;
//!
//! assert_eq!(sink.bytes(), b"hello world");
//! assert_eq!(sink.violation(), None, "the fake honors the delivery contract");
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! Boundary rules (enforced by `.scripts/check_crate_architecture.py`):
//! - internal dependencies: `gramdrive-model`, `gramdrive-source`;
//! - product crates may use this crate **only** as a `dev-dependency`;
//!   it never ships inside a product artifact;
//! - no platform-specific dependencies or `cfg(target_os/windows/unix)` code.

#![forbid(unsafe_code)]

mod rng;
mod tree;

pub mod conformance;
pub mod exec;
pub mod fake;
pub mod fault;
pub mod fixture;
pub mod record;
pub mod script;
pub mod sink;

pub use gramdrive_model as model;
pub use gramdrive_source as source;

pub use fake::FakeSource;
pub use fault::{Effect, Fault, Occurrence, Operation};
pub use record::{Call, Interaction, Outcome};
pub use script::{
    ChunkPlan, DEFAULT_MAX_CHUNK_BYTES, DEFAULT_SEED, ScriptBuilder, ScriptError, SourceScript,
};
pub use sink::RecordingSink;

#[cfg(test)]
mod tests {
    #[test]
    fn re_exports_model_and_source() {
        let range = crate::model::ByteRange::new(0, 4).expect("valid range");
        let via_source = crate::source::model::ByteRange::new(0, 4).expect("valid range");
        assert_eq!(range, via_source);
    }
}
