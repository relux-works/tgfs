//! Ranged content fetch: request, delivery, progress, thumbnails
//! (SYNC-040..046; TASK-260715-1j4ij3).
//!
//! # The delivery contract
//!
//! A fetch is pinned: [`FetchRequest`] names the item, the
//! [`ContentVersion`] the caller observed, and a [`ByteRange`]. The source
//! delivers exactly the requested bytes — in offset order, contiguous from
//! `range.start()`, ending exactly at `range.end()` — as [`ContentChunk`]s
//! into the caller's [`ContentSink`], then resolves. It may download larger
//! aligned blocks internally (SYNC-041); what reaches the sink is still
//! exactly the range. A range the item cannot satisfy is
//! [`SourceError::InvalidRequest`](crate::SourceError::InvalidRequest);
//! content that changed away from the pinned version is
//! [`SourceError::VersionConflict`](crate::SourceError::VersionConflict) —
//! bytes of version A are never passed off as version B (SYNC-042,
//! `.spec/domain-model.md` § Versioning).
//!
//! # Cancellation (SYNC-043, NFR-025)
//!
//! Two paths, both prompt: dropping the returned future at an await point
//! (what a cancelled binding task does), and the sink returning
//! [`SinkControl::Stop`], after which the source ceases network and disk
//! work and resolves with
//! [`SourceError::Cancelled`](crate::SourceError::Cancelled). Either way
//! the caller's partial state must remain resumable or safely disposable —
//! partial content lives under a transfer identity and is promoted only
//! after verification (SYNC-042); that promotion is the engine's job, not
//! the source's.
//!
//! # Progress is accounted, not reported (NFR-033 observability)
//!
//! The source does not push progress snapshots; delivery itself is the
//! progress signal. [`FetchProgress`] folds chunks into verified
//! accounting: it rejects out-of-order, overlapping, or over-delivering
//! chunks, so a source that violates the delivery contract is caught at
//! the first bad chunk — by the engine in production and by the
//! conformance suite in tests — rather than corrupting range accounting
//! downstream (SYNC-046).

use gramdrive_model::ByteRange;
use gramdrive_model::identity::ItemId;
use gramdrive_model::version::ContentVersion;

/// One ranged content fetch, pinned to a content version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchRequest {
    /// The file item whose bytes are requested.
    pub item: ItemId,
    /// The content version the caller observed; the fetch is valid only
    /// for it (DOM-003, SYNC-042).
    pub version: ContentVersion,
    /// The bytes requested — half-open, never empty by construction.
    pub range: ByteRange,
}

/// A borrowed, non-empty run of delivered bytes at an absolute offset.
///
/// Non-empty and overflow-free by construction: [`ContentChunk::end`]
/// cannot wrap, so range arithmetic downstream needs no checked paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContentChunk<'a> {
    offset: u64,
    bytes: &'a [u8],
}

impl<'a> ContentChunk<'a> {
    /// Wraps `bytes` delivered at `offset`, rejecting the two states the
    /// type promises away: an empty chunk and an offset+length that would
    /// overflow `u64`.
    pub fn new(offset: u64, bytes: &'a [u8]) -> Result<Self, InvalidChunk> {
        if bytes.is_empty() {
            return Err(InvalidChunk::Empty);
        }
        let len = bytes.len() as u64;
        if offset.checked_add(len).is_none() {
            return Err(InvalidChunk::OffsetOverflow { offset, len });
        }
        Ok(Self { offset, bytes })
    }

    /// Absolute offset of the first byte within the content object.
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// The delivered bytes; never empty.
    pub fn bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// Number of bytes in this chunk; always non-zero.
    // Non-empty by construction, so `is_empty` would be constant `false`
    // and is deliberately not provided (same call as ByteRange::len).
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    /// Exclusive end offset. Cannot overflow by construction.
    pub fn end(&self) -> u64 {
        self.offset + self.len()
    }
}

/// Why bytes and an offset cannot form a [`ContentChunk`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidChunk {
    /// The byte slice is empty; delivering nothing is not a delivery.
    Empty,
    /// `offset + len` exceeds `u64::MAX`.
    OffsetOverflow {
        /// The requested offset.
        offset: u64,
        /// The chunk length.
        len: u64,
    },
}

impl std::fmt::Display for InvalidChunk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str("content chunk is empty"),
            Self::OffsetOverflow { offset, len } => {
                write!(
                    f,
                    "content chunk at offset {offset} with length {len} overflows u64"
                )
            }
        }
    }
}

impl std::error::Error for InvalidChunk {}

/// The sink's verdict after accepting a chunk.
#[must_use = "ignoring the verdict lets a fetch run on after the sink asked to stop"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SinkControl {
    /// Deliver the next chunk.
    Continue,
    /// Stop promptly: cease network and disk work and resolve the fetch
    /// with `SourceError::Cancelled` (SYNC-043).
    Stop,
}

/// Where fetched bytes go — implemented by the caller, driven by the
/// source.
///
/// Calls arrive sequentially from the fetch operation's execution context,
/// in delivery order. Implementations must return promptly (they sit on
/// the transfer path, NFR-025); durable writes may buffer, and integrity
/// verification happens after delivery, not inside the sink (SYNC-042).
pub trait ContentSink: Send {
    /// Accepts the next chunk and decides whether delivery continues.
    fn accept(&mut self, chunk: ContentChunk<'_>) -> SinkControl;
}

/// Verified per-fetch delivery accounting.
///
/// Fold every delivered chunk through [`FetchProgress::record`]; the fold
/// enforces the delivery contract — contiguous from the range start, never
/// past the range end — so `delivered` can only describe bytes that arrived
/// in a valid order. Completion is exactly `delivered == expected`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FetchProgress {
    range: ByteRange,
    delivered: u64,
}

impl FetchProgress {
    /// Fresh accounting for one fetch of `range`.
    pub fn new(range: ByteRange) -> Self {
        Self {
            range,
            delivered: 0,
        }
    }

    /// Records one delivered chunk, rejecting any delivery that violates
    /// the contract: a gap, an overlap, a wrong start, or bytes past the
    /// requested end.
    pub fn record(&mut self, chunk: &ContentChunk<'_>) -> Result<(), DeliveryViolation> {
        let expected_offset = self.range.start() + self.delivered;
        if chunk.offset() != expected_offset {
            return Err(DeliveryViolation::NonContiguous {
                expected_offset,
                found_offset: chunk.offset(),
            });
        }
        if chunk.end() > self.range.end() {
            return Err(DeliveryViolation::Overrun {
                range_end: self.range.end(),
                chunk_end: chunk.end(),
            });
        }
        self.delivered += chunk.len();
        Ok(())
    }

    /// Bytes delivered so far; monotonically non-decreasing.
    pub fn delivered(&self) -> u64 {
        self.delivered
    }

    /// Total bytes this fetch must deliver — the range length.
    pub fn expected(&self) -> u64 {
        self.range.len()
    }

    /// Bytes still owed.
    pub fn remaining(&self) -> u64 {
        self.expected() - self.delivered
    }

    /// Whether the full range has been delivered.
    pub fn is_complete(&self) -> bool {
        self.delivered == self.expected()
    }
}

/// A source delivering outside the requested range's contract.
///
/// Reaching this error means the *source* is broken, not the request: the
/// engine must abort the fetch and treat the transfer as failed rather than
/// account the bytes (SYNC-046).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryViolation {
    /// The chunk does not start where delivery stands — a gap, an overlap,
    /// or a wrong first offset.
    NonContiguous {
        /// The offset the contract requires next.
        expected_offset: u64,
        /// The offset the chunk carried.
        found_offset: u64,
    },
    /// The chunk runs past the requested range's end.
    Overrun {
        /// The exclusive end of the requested range.
        range_end: u64,
        /// The chunk's exclusive end.
        chunk_end: u64,
    },
}

impl std::fmt::Display for DeliveryViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NonContiguous {
                expected_offset,
                found_offset,
            } => write!(
                f,
                "non-contiguous delivery: expected offset {expected_offset}, got {found_offset}"
            ),
            Self::Overrun {
                range_end,
                chunk_end,
            } => write!(
                f,
                "delivery overrun: range ends at {range_end}, chunk ends at {chunk_end}"
            ),
        }
    }
}

impl std::error::Error for DeliveryViolation {}

/// Requested thumbnail bounding box, in pixels.
///
/// Never zero-sized by construction. The source returns a thumbnail that
/// fits within the box when it has one; exact dimensions are the source's
/// choice (PLAT-AND-004 "thumbnails where useful").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ThumbnailSpec {
    /// Maximum width in pixels.
    pub max_width_px: std::num::NonZeroU32,
    /// Maximum height in pixels.
    pub max_height_px: std::num::NonZeroU32,
}

/// One delivered thumbnail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thumbnail {
    mime_type: String,
    bytes: Vec<u8>,
}

impl Thumbnail {
    /// Wraps encoded image bytes with their MIME type, rejecting empty
    /// values ("no thumbnail" is `Option::None`, never an empty body).
    pub fn new(mime_type: impl Into<String>, bytes: Vec<u8>) -> Result<Self, InvalidThumbnail> {
        let mime_type = mime_type.into();
        if mime_type.is_empty() {
            return Err(InvalidThumbnail::EmptyMimeType);
        }
        if bytes.is_empty() {
            return Err(InvalidThumbnail::EmptyBytes);
        }
        Ok(Self { mime_type, bytes })
    }

    /// MIME type of the encoded image.
    pub fn mime_type(&self) -> &str {
        &self.mime_type
    }

    /// The encoded image bytes; never empty.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// Why values cannot form a [`Thumbnail`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidThumbnail {
    /// The MIME type is empty.
    EmptyMimeType,
    /// The image body is empty.
    EmptyBytes,
}

impl std::fmt::Display for InvalidThumbnail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyMimeType => f.write_str("thumbnail MIME type is empty"),
            Self::EmptyBytes => f.write_str("thumbnail bytes are empty"),
        }
    }
}

impl std::error::Error for InvalidThumbnail {}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(start: u64, end: u64) -> ByteRange {
        ByteRange::new(start, end).unwrap()
    }

    #[test]
    fn chunk_rejects_empty_and_overflowing() {
        assert_eq!(ContentChunk::new(0, &[]).unwrap_err(), InvalidChunk::Empty);
        assert_eq!(
            ContentChunk::new(u64::MAX, &[1]).unwrap_err(),
            InvalidChunk::OffsetOverflow {
                offset: u64::MAX,
                len: 1
            }
        );
    }

    #[test]
    fn chunk_reports_geometry() {
        let chunk = ContentChunk::new(10, &[1, 2, 3]).unwrap();
        assert_eq!(chunk.offset(), 10);
        assert_eq!(chunk.len(), 3);
        assert_eq!(chunk.end(), 13);
        assert_eq!(chunk.bytes(), &[1, 2, 3]);
    }

    #[test]
    fn progress_accounts_a_contiguous_delivery() {
        let mut progress = FetchProgress::new(range(100, 110));
        assert_eq!(progress.expected(), 10);
        assert!(!progress.is_complete());

        progress
            .record(&ContentChunk::new(100, &[0; 4]).unwrap())
            .unwrap();
        assert_eq!(progress.delivered(), 4);
        assert_eq!(progress.remaining(), 6);

        progress
            .record(&ContentChunk::new(104, &[0; 6]).unwrap())
            .unwrap();
        assert!(progress.is_complete());
        assert_eq!(progress.remaining(), 0);
    }

    #[test]
    fn progress_rejects_wrong_first_offset() {
        let mut progress = FetchProgress::new(range(100, 110));
        let err = progress
            .record(&ContentChunk::new(0, &[0; 4]).unwrap())
            .unwrap_err();
        assert_eq!(
            err,
            DeliveryViolation::NonContiguous {
                expected_offset: 100,
                found_offset: 0
            }
        );
        assert_eq!(progress.delivered(), 0, "rejected chunks are not counted");
    }

    #[test]
    fn progress_rejects_gaps_and_overlaps() {
        let mut progress = FetchProgress::new(range(0, 100));
        progress
            .record(&ContentChunk::new(0, &[0; 10]).unwrap())
            .unwrap();
        // Gap: skips bytes 10..20.
        assert!(matches!(
            progress.record(&ContentChunk::new(20, &[0; 10]).unwrap()),
            Err(DeliveryViolation::NonContiguous {
                expected_offset: 10,
                found_offset: 20
            })
        ));
        // Overlap: re-delivers bytes 5..15.
        assert!(matches!(
            progress.record(&ContentChunk::new(5, &[0; 10]).unwrap()),
            Err(DeliveryViolation::NonContiguous { .. })
        ));
    }

    #[test]
    fn progress_rejects_overrun() {
        let mut progress = FetchProgress::new(range(0, 8));
        let err = progress
            .record(&ContentChunk::new(0, &[0; 9]).unwrap())
            .unwrap_err();
        assert_eq!(
            err,
            DeliveryViolation::Overrun {
                range_end: 8,
                chunk_end: 9
            }
        );
    }

    #[test]
    fn thumbnail_rejects_empty_parts() {
        assert_eq!(
            Thumbnail::new("", vec![1]).unwrap_err(),
            InvalidThumbnail::EmptyMimeType
        );
        assert_eq!(
            Thumbnail::new("image/jpeg", Vec::new()).unwrap_err(),
            InvalidThumbnail::EmptyBytes
        );
        let thumb = Thumbnail::new("image/jpeg", vec![0xff, 0xd8]).unwrap();
        assert_eq!(thumb.mime_type(), "image/jpeg");
        assert_eq!(thumb.bytes(), &[0xff, 0xd8]);
    }

    #[test]
    fn violation_messages_carry_offsets() {
        assert_eq!(
            DeliveryViolation::NonContiguous {
                expected_offset: 10,
                found_offset: 20
            }
            .to_string(),
            "non-contiguous delivery: expected offset 10, got 20"
        );
        assert_eq!(
            DeliveryViolation::Overrun {
                range_end: 8,
                chunk_end: 9
            }
            .to_string(),
            "delivery overrun: range ends at 8, chunk ends at 9"
        );
    }
}
