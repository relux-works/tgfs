//! GramDrive domain model — the shared vocabulary of the drive core.
//!
//! This crate owns provider-neutral domain types: item identity, the virtual
//! `chat -> folder -> files` tree, naming/sanitization policy, versions,
//! change cursors, and byte ranges. Every other core crate depends on this
//! one; this crate depends on nothing inside the workspace.
//!
//! Boundary rules (enforced by `.scripts/check_crate_architecture.py`):
//! - no internal dependencies;
//! - no platform-specific dependencies or `cfg(target_os/windows/unix)` code;
//! - no Telegram/TDLib/gotd types — sources adapt to this vocabulary, never
//!   the other way around (DEC-003).

#![forbid(unsafe_code)]

pub mod identity;

/// A half-open byte range `[start, end)` within a content object.
///
/// Used by ranged content fetch and hydration. A range is never empty:
/// construction fails unless `end > start`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ByteRange {
    start: u64,
    end: u64,
}

/// Error returned when a [`ByteRange`] would be empty or inverted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidByteRange {
    /// Requested start offset.
    pub start: u64,
    /// Requested exclusive end offset.
    pub end: u64,
}

impl std::fmt::Display for InvalidByteRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "invalid byte range [{}, {}): end must be greater than start",
            self.start, self.end
        )
    }
}

impl std::error::Error for InvalidByteRange {}

impl ByteRange {
    /// Creates the half-open range `[start, end)`.
    pub fn new(start: u64, end: u64) -> Result<Self, InvalidByteRange> {
        if end > start {
            Ok(Self { start, end })
        } else {
            Err(InvalidByteRange { start, end })
        }
    }

    /// Inclusive start offset.
    pub fn start(&self) -> u64 {
        self.start
    }

    /// Exclusive end offset.
    pub fn end(&self) -> u64 {
        self.end
    }

    /// Number of bytes covered; always non-zero.
    // A ByteRange is never empty by construction, so `is_empty` would be
    // a constant `false` and is deliberately not provided.
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> u64 {
        self.end - self.start
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructs_half_open_range() {
        let range = ByteRange::new(10, 25).expect("valid range");
        assert_eq!(range.start(), 10);
        assert_eq!(range.end(), 25);
        assert_eq!(range.len(), 15);
    }

    #[test]
    fn rejects_empty_range() {
        assert_eq!(
            ByteRange::new(5, 5),
            Err(InvalidByteRange { start: 5, end: 5 })
        );
    }

    #[test]
    fn rejects_inverted_range() {
        assert_eq!(
            ByteRange::new(9, 3),
            Err(InvalidByteRange { start: 9, end: 3 })
        );
    }

    #[test]
    fn error_message_names_the_offsets() {
        let err = ByteRange::new(9, 3).unwrap_err();
        assert_eq!(
            err.to_string(),
            "invalid byte range [9, 3): end must be greater than start"
        );
    }
}
