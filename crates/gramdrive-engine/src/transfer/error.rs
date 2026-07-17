//! Failure vocabulary of the transfer machine.

use gramdrive_model::ByteRange;
use gramdrive_state::StateError;

/// Why a transfer machine operation refused or failed.
///
/// Structured for the NFR-030 discipline: a category a caller can act on,
/// never a panic. The named variants are the machine's own gates; anything
/// the state layer detects passes through as [`EngineError::State`] with its
/// category intact (including [`StateError::InvalidTransition`], which is
/// how a stale claim shows up when the durable row moved underneath it).
#[derive(Debug)]
pub enum EngineError {
    /// The state store refused or failed the underlying operation.
    State(StateError),
    /// The item cannot be hydrated at all — a directory, a tombstoned row,
    /// POL-4 restricted or unavailable content, or a file with no content
    /// version to pin (SYNC-042 requires the pin before the first byte).
    NotHydratable {
        /// Which precondition failed.
        reason: &'static str,
    },
    /// A requested range extends past the item's known extent — a caller
    /// bug the source would reject as `InvalidRequest` after a wasted
    /// round trip.
    RangeBeyondExtent {
        /// The offending exclusive end offset.
        end: u64,
        /// The extent the projection records.
        extent: u64,
    },
    /// The promotion gate refused: bytes the transfer promised are not
    /// staged, and incomplete content is never published (SYNC-042,
    /// NFR-012). The transfer is unchanged and still live.
    IncompleteContent {
        /// Exactly the bytes still missing.
        missing: Vec<ByteRange>,
    },
    /// A whole-object transfer reached promotion while the projection still
    /// records no size, so completeness cannot be proven. Fail-closed: the
    /// transfer stays live until the extent is known (a metadata refresh
    /// records it) rather than promoting on hope.
    UnknownExtent,
    /// Recorded progress would shrink. Staged bytes are durable; a report
    /// that un-stages them describes data loss and is refused (the caller
    /// that really means "start over" fails the attempt instead — see
    /// integrity handling in [`crate::transfer`]).
    ProgressRegression,
    /// Progress named a different staging area than the one the transfer
    /// already claims. One transfer owns one staging handle for its whole
    /// life; switching handles would orphan the bytes under the first one.
    StagingChanged,
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::State(error) => write!(f, "state store: {error}"),
            Self::NotHydratable { reason } => write!(f, "item is not hydratable: {reason}"),
            Self::RangeBeyondExtent { end, extent } => write!(
                f,
                "requested range ends at {end}, past the item extent {extent}"
            ),
            Self::IncompleteContent { missing } => {
                write!(f, "content incomplete: {} range(s) missing", missing.len())
            }
            Self::UnknownExtent => {
                write!(f, "whole-object completeness unprovable: extent unknown")
            }
            Self::ProgressRegression => {
                write!(f, "recorded progress would shrink durable staged ranges")
            }
            Self::StagingChanged => {
                write!(f, "transfer already claims a different staging handle")
            }
        }
    }
}

impl std::error::Error for EngineError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::State(error) => Some(error),
            Self::NotHydratable { .. }
            | Self::RangeBeyondExtent { .. }
            | Self::IncompleteContent { .. }
            | Self::UnknownExtent
            | Self::ProgressRegression
            | Self::StagingChanged => None,
        }
    }
}

impl From<StateError> for EngineError {
    fn from(error: StateError) -> Self {
        Self::State(error)
    }
}
