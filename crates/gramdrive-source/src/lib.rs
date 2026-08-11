//! GramDrive `DriveSource` contract — the provider-neutral boundary between
//! the drive core and any Telegram (or other) backend (DEC-003;
//! TASK-260715-1j4ij3).
//!
//! This crate owns the contract, not any implementation:
//!
//! - [`DriveSource`] — the asynchronous, dyn-compatible trait: root and
//!   children enumeration, change feed, ranged content fetch, thumbnails
//!   ([`source`]);
//! - [`SourceItem`] and its structural vocabulary — directory/file split,
//!   availability, derived read-only capabilities ([`item`]);
//! - [`ItemPage`]/[`PageToken`] snapshot paging and the
//!   [`ChangePage`]/[`ItemChange`] feed ([`page`]);
//! - [`FetchRequest`], chunked delivery into a [`ContentSink`], verified
//!   [`FetchProgress`] accounting, thumbnails ([`fetch`]);
//! - [`SourceError`] — the normalized failure taxonomy — and its derived
//!   [`RetryAdvice`] classification ([`error`]).
//!
//! The durable vocabulary the contract is written in — [`ItemId`] stable
//! identities, [`MetadataVersion`]/[`ContentVersion`], the serialized,
//! versioned [`ChangeCursor`], [`ByteRange`], [`Capabilities`] — lives in
//! [`gramdrive_model`] (layer 0) and is re-exported here as [`model`]:
//! the state store persists cursors and versions without depending on this
//! crate.
//!
//! Implementations live in separate crates — never here and never behind
//! feature flags of this crate (DEC-003/DEC-005):
//!
//! - `gramdrive-source-tdjson` (future) — local TDLib via tdjson FFI;
//! - `gramdrive-source-remote` (future) — remote gotd/td service over HTTPS;
//! - `gramdrive-testkit` — deterministic fake source for conformance tests
//!   (TASK-260715-3uft8j).
//!
//! Every implementation must pass the one conformance suite (SYNC-002,
//! NFR-002; TASK-260715-3e8q4m). Exposure through `gramdrive-ffi` maps
//! these types onto UniFFI records/enums/callback interfaces in that crate
//! (the boundary's own error and progress types already follow this
//! pattern); everything here is deliberately UniFFI-representable — owned
//! data, integer milliseconds for times, strings for opaque tokens, no
//! borrowed or OS types in any exposed struct.
//!
//! Boundary rules (enforced by `.scripts/check_crate_architecture.py`):
//! - internal dependencies: `gramdrive-model` only;
//! - no platform-specific dependencies or `cfg(target_os/windows/unix)` code;
//! - no TDLib/gotd/MTProto types in the public API.
//!
//! [`ItemId`]: gramdrive_model::identity::ItemId
//! [`MetadataVersion`]: gramdrive_model::version::MetadataVersion
//! [`ContentVersion`]: gramdrive_model::version::ContentVersion
//! [`ChangeCursor`]: gramdrive_model::cursor::ChangeCursor
//! [`ByteRange`]: gramdrive_model::ByteRange
//! [`Capabilities`]: gramdrive_model::tree::Capabilities

#![forbid(unsafe_code)]

pub mod error;
pub mod fetch;
pub mod item;
pub mod page;
pub mod source;

pub use gramdrive_model as model;

pub use error::{RetryAdvice, SourceError};
pub use fetch::{
    ContentChunk, ContentSink, DeliveryViolation, FetchProgress, FetchRequest, InvalidChunk,
    InvalidThumbnail, SinkControl, Thumbnail, ThumbnailSpec,
};
pub use item::{ContentAvailability, DirectoryKind, FileFacts, FileKind, ItemContent, SourceItem};
pub use page::{
    ChangePage, InvalidPageToken, ItemChange, ItemPage, MAX_PAGE_TOKEN_BYTES, PageRequest,
    PageToken,
};
pub use source::{ContentSource, DriveSource, SourceFuture};

#[cfg(test)]
mod tests {
    #[test]
    fn re_exports_model_vocabulary() {
        let range = crate::model::ByteRange::new(0, 4).expect("valid range");
        assert_eq!(range.len(), 4);
        let version = crate::model::version::ContentVersion::new("v1").expect("valid token");
        assert_eq!(version.as_str(), "v1");
    }
}
