//! The staging ports: where fetched bytes live between delivery and
//! promotion (SYNC-042).
//!
//! Partial data is stored under a transfer identity, and *where* that
//! storage lives is the embedding host's decision — the core is
//! platform-neutral by architecture rule (crates/README.md), so it cannot
//! open a file. The host implements these two traits; the database knows
//! the staging area only by the opaque handle the host returns, which is
//! the same handle `gramdrive_state`'s reconciliation matches against
//! [`LocalStorage::staging_objects`](gramdrive_state::LocalStorage).
//!
//! Failure classification is the caller's contract, not a formality: a
//! [`StagingError::Full`] is the SYNC-044 disk-full class — the transfer
//! parks with progress kept until space frees — while a
//! [`StagingError::Failed`] means the staged bytes can no longer be
//! trusted, which is the integrity class: the attempt wipes and re-fetches
//! from scratch. A host that reports "out of space" as `Failed` turns a
//! recoverable pause into discarded work.

use crate::transfer::TransferFault;
use gramdrive_state::repo::TransferId;

/// Why a staging operation failed, in the engine's retry vocabulary
/// (SYNC-044).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StagingError {
    /// Local storage cannot hold the bytes. The transfer parks with its
    /// progress kept (SYNC-054 owns the actionable quota story).
    Full {
        /// The host's description of the failure; diagnostic, never
        /// contractual.
        detail: String,
    },
    /// The staging object failed in a way that leaves its bytes untrusted
    /// — a write error, a read past what was written, a vanished object.
    /// The attempt discards staged progress and re-fetches.
    Failed {
        /// The host's description of the failure; diagnostic, never
        /// contractual.
        detail: String,
    },
}

impl StagingError {
    /// The transfer-machine fault this staging failure classifies as.
    pub(crate) fn into_fault(self) -> TransferFault {
        match self {
            Self::Full { detail } => TransferFault::DiskFull { detail },
            Self::Failed { detail } => TransferFault::Integrity { detail },
        }
    }
}

impl std::fmt::Display for StagingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full { detail } => write!(f, "staging storage is full: {detail}"),
            Self::Failed { detail } => write!(f, "staging storage failed: {detail}"),
        }
    }
}

impl std::error::Error for StagingError {}

/// One open staging area — offset-addressed scratch storage for exactly
/// one transfer's bytes.
///
/// Writes land at absolute content offsets, because parallel sub-fetches
/// complete out of order (SYNC-041): the object is sparse until the ranges
/// meet. Reads serve the coordinator's reader streaming and, later, the
/// integrity/promotion layer (TASK-260715-3s6cpe); the coordinator only
/// ever reads offsets it has already written, so a read outside written
/// bytes is a host error, not a normal answer.
///
/// `Debug` is a supertrait (like [`gramdrive_state::LocalStorage`]) so the
/// engine's own state containing one stays printable in diagnostics.
pub trait Staging: Send + std::fmt::Debug {
    /// The opaque handle the database records as the transfer's
    /// `temp_ref`. Stable for the life of the staging object — the journal
    /// refuses a transfer that switches handles
    /// ([`EngineError::StagingChanged`](crate::transfer::EngineError::StagingChanged)).
    fn handle(&self) -> &str;

    /// Writes `bytes` at absolute content offset `offset`.
    ///
    /// The write must be durable enough that a later
    /// [`Staging::read_at`] of the same span returns these bytes; whether
    /// the host buffers beyond that is its own trade — promotion re-checks
    /// integrity before anything is published (SYNC-042).
    fn write_at(&mut self, offset: u64, bytes: &[u8]) -> Result<(), StagingError>;

    /// Fills `buf` from absolute content offset `offset`.
    ///
    /// Only offsets previously written through this object (or a previous
    /// open of the same handle) are ever requested.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), StagingError>;
}

/// The host's factory for staging areas, keyed by transfer.
///
/// Opening with `existing: Some(handle)` must return the same object a
/// previous open of that handle produced, bytes intact — that is what
/// makes a resume plan (`requested minus staged`, SYNC-042) mean anything.
/// With `existing: None` the host allocates a fresh area and mints its
/// handle.
pub trait StagingHost: Send {
    /// Opens the staging area for `transfer`, reusing `existing` when the
    /// journal already records a handle.
    fn open(
        &mut self,
        transfer: TransferId,
        existing: Option<&str>,
    ) -> Result<Box<dyn Staging>, StagingError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_classify_into_the_retry_taxonomy() {
        let full = StagingError::Full {
            detail: "quota".to_owned(),
        };
        assert!(matches!(
            full.clone().into_fault(),
            TransferFault::DiskFull { .. }
        ));
        assert!(full.to_string().contains("full"));

        let failed = StagingError::Failed {
            detail: "io".to_owned(),
        };
        assert!(matches!(
            failed.clone().into_fault(),
            TransferFault::Integrity { .. }
        ));
        assert!(failed.to_string().contains("failed"));
    }
}
