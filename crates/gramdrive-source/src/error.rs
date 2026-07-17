//! The source failure taxonomy and its retry classification (SYNC-044,
//! NFR-033, PRD-004; TASK-260715-1j4ij3).
//!
//! Every `DriveSource` operation fails with a [`SourceError`] and nothing
//! else: the categories are the contract, backend-specific failures are
//! normalized into them by the adapter that observes them (DEC-003 — no
//! TDLib/gotd error type crosses this boundary). The `detail` strings are
//! diagnostic text for logs — human-readable, redacted, never contractual;
//! consumers must not parse them.
//!
//! # Retry classification is derived, not stored
//!
//! `.spec/architecture.md` makes retry classification part of the contract.
//! [`SourceError::retry_advice`] derives the classification from the
//! category in one exhaustive match, so a category and its retry behavior
//! cannot drift apart — there is no second field for an adapter to fill in
//! wrong. Retry loops themselves stay bounded and observable on the engine
//! side (NFR-033); the advice only says what a retry would need.
//!
//! # What is deliberately absent
//!
//! SYNC-044 also names *disk full* and *integrity failure* among the retry
//! classes. Those are not source failures: disk full is local storage
//! (state/cache, `gramdrive-state`), and integrity is verified by the
//! transfer engine after bytes arrive (SYNC-042, NFR-012). Keeping them out
//! of this enum keeps every variant something a backend can actually
//! report; the full cross-layer taxonomy is TASK-260715-3b9w8x.

use std::time::Duration;

use gramdrive_model::version::ContentVersion;

/// Why a `DriveSource` operation failed.
///
/// Variant coverage against the specified failure classes: authorization →
/// [`AuthRequired`]; flood wait/backoff → [`RateLimited`]; restricted or
/// protected content → [`Restricted`]; expired/unavailable file reference →
/// [`StaleReference`]; transient network / unreachable backend →
/// [`Unavailable`]; cancellation → [`Cancelled`]; source deletion →
/// [`NotFound`]; version race during fetch → [`VersionConflict`]; rejected
/// change cursor or page token → [`CursorRejected`] (SYNC-004).
///
/// [`AuthRequired`]: SourceError::AuthRequired
/// [`RateLimited`]: SourceError::RateLimited
/// [`Restricted`]: SourceError::Restricted
/// [`StaleReference`]: SourceError::StaleReference
/// [`Unavailable`]: SourceError::Unavailable
/// [`Cancelled`]: SourceError::Cancelled
/// [`NotFound`]: SourceError::NotFound
/// [`VersionConflict`]: SourceError::VersionConflict
/// [`CursorRejected`]: SourceError::CursorRejected
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceError {
    /// The request itself violates the contract (a range beyond the item's
    /// extent, a fetch against a directory). Retrying the identical call
    /// cannot succeed; the bug is in the caller.
    InvalidRequest {
        /// Diagnostic detail; not contractual.
        detail: String,
    },
    /// The item does not exist — never did, or was deleted at the source
    /// (the *source deletion* class of SYNC-044; SYNC-025 decides whether
    /// the drive tombstones or removes).
    NotFound {
        /// Diagnostic detail; not contractual.
        detail: String,
    },
    /// The source has no usable authorization; the user must (re)authorize
    /// in the host app before any retry can succeed (PRD-004).
    AuthRequired {
        /// Diagnostic detail; not contractual.
        detail: String,
    },
    /// The source is throttling — a Telegram flood wait or equivalent.
    /// Honor `retry_after` when the backend supplied one; flood waits must
    /// never become tight retry loops (NFR-033, SEC-031).
    RateLimited {
        /// Backend-provided minimum backoff, when it supplied one.
        retry_after: Option<Duration>,
        /// Diagnostic detail; not contractual.
        detail: String,
    },
    /// The content is protected or unsupported at the source — Telegram's
    /// no-save flag or an object class the source cannot serve (POL-4).
    /// The item stays visible as an explicit placeholder; its bytes are
    /// never fetched, and no retry changes that.
    Restricted {
        /// Diagnostic detail; not contractual.
        detail: String,
    },
    /// The source's locator for the content expired — Telegram's
    /// `FILE_REFERENCE_EXPIRED` class. References are refreshable metadata,
    /// never identity (DOM-007): the adapter refreshes and the caller
    /// retries; item identity must not change (SYNC-045).
    StaleReference {
        /// Diagnostic detail; not contractual.
        detail: String,
    },
    /// The pinned content version is no longer what the source serves —
    /// the content changed mid-operation. Bytes fetched for version A must
    /// never be published as version B; the caller restarts against the
    /// current version (`.spec/domain-model.md` § Versioning).
    VersionConflict {
        /// The version the source now serves, when it knows it.
        current: Option<ContentVersion>,
        /// Diagnostic detail; not contractual.
        detail: String,
    },
    /// A continuation anchor — change cursor or page token — that this
    /// source cannot serve: wrong account/namespace scope, expired, or
    /// malformed. The explicit rejection SYNC-004 requires; recovery is a
    /// fresh baseline, never a silent partial answer.
    CursorRejected {
        /// Diagnostic detail; not contractual.
        detail: String,
    },
    /// The source cannot be reached right now — the *transient network*
    /// class: network down, backend unreachable, connection lost mid-call.
    /// Retrying later with backoff is reasonable (PRD-004).
    Unavailable {
        /// Diagnostic detail; not contractual.
        detail: String,
    },
    /// The operation was cancelled — the returned future was dropped at a
    /// cancellation point, or the content sink asked to stop (SYNC-043).
    Cancelled {
        /// Diagnostic detail; not contractual.
        detail: String,
    },
    /// A source-internal failure that fits no category above. A bug or an
    /// unclassified backend condition; report and log, do not blind-retry.
    Internal {
        /// Diagnostic detail; not contractual.
        detail: String,
    },
}

impl SourceError {
    /// What a retry of the failed request would need — derived from the
    /// category in one place so classification cannot drift (SYNC-044).
    pub fn retry_advice(&self) -> RetryAdvice {
        match self {
            Self::InvalidRequest { .. }
            | Self::NotFound { .. }
            | Self::Restricted { .. }
            | Self::Cancelled { .. }
            | Self::Internal { .. } => RetryAdvice::Never,
            Self::RateLimited { retry_after, .. } => RetryAdvice::AfterBackoff {
                minimum: *retry_after,
            },
            Self::Unavailable { .. } => RetryAdvice::AfterBackoff { minimum: None },
            Self::AuthRequired { .. } => RetryAdvice::AfterReauth,
            Self::StaleReference { .. } | Self::VersionConflict { .. } => RetryAdvice::AfterRefresh,
            Self::CursorRejected { .. } => RetryAdvice::AfterRebaseline,
        }
    }
}

impl std::fmt::Display for SourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRequest { detail } => write!(f, "invalid request: {detail}"),
            Self::NotFound { detail } => write!(f, "not found: {detail}"),
            Self::AuthRequired { detail } => write!(f, "authorization required: {detail}"),
            Self::RateLimited {
                retry_after,
                detail,
            } => match retry_after {
                Some(wait) => {
                    write!(
                        f,
                        "rate limited (retry after {} ms): {detail}",
                        wait.as_millis()
                    )
                }
                None => write!(f, "rate limited: {detail}"),
            },
            Self::Restricted { detail } => write!(f, "restricted content: {detail}"),
            Self::StaleReference { detail } => write!(f, "stale content reference: {detail}"),
            Self::VersionConflict { detail, .. } => {
                write!(f, "content version conflict: {detail}")
            }
            Self::CursorRejected { detail } => write!(f, "cursor rejected: {detail}"),
            Self::Unavailable { detail } => write!(f, "source unavailable: {detail}"),
            Self::Cancelled { detail } => write!(f, "cancelled: {detail}"),
            Self::Internal { detail } => write!(f, "internal source error: {detail}"),
        }
    }
}

impl std::error::Error for SourceError {}

/// What a retry needs in order to have a chance of succeeding.
///
/// The engine owns retry *policy* (bounds, jitter, budgets — NFR-033); this
/// type only classifies. Every variant states the precondition a retry
/// depends on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryAdvice {
    /// No retry of the identical request can succeed.
    Never,
    /// Retry the identical request after backing off.
    AfterBackoff {
        /// Source-mandated minimum wait, when the backend stated one
        /// (flood waits). `None` means the caller picks its own schedule.
        minimum: Option<Duration>,
    },
    /// Retry only after the user (re)authorizes the account.
    AfterReauth,
    /// Refresh the source-side facts first — content reference or current
    /// version — then retry with the refreshed request (DOM-007).
    AfterRefresh,
    /// Discard the rejected anchor and re-baseline: fresh enumeration,
    /// fresh cursor (SYNC-004).
    AfterRebaseline,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detail() -> String {
        "diagnostic".to_owned()
    }

    #[test]
    fn retry_classification_covers_every_category() {
        let cases: Vec<(SourceError, RetryAdvice)> = vec![
            (
                SourceError::InvalidRequest { detail: detail() },
                RetryAdvice::Never,
            ),
            (
                SourceError::NotFound { detail: detail() },
                RetryAdvice::Never,
            ),
            (
                SourceError::AuthRequired { detail: detail() },
                RetryAdvice::AfterReauth,
            ),
            (
                SourceError::RateLimited {
                    retry_after: Some(Duration::from_millis(1500)),
                    detail: detail(),
                },
                RetryAdvice::AfterBackoff {
                    minimum: Some(Duration::from_millis(1500)),
                },
            ),
            (
                SourceError::RateLimited {
                    retry_after: None,
                    detail: detail(),
                },
                RetryAdvice::AfterBackoff { minimum: None },
            ),
            (
                SourceError::Restricted { detail: detail() },
                RetryAdvice::Never,
            ),
            (
                SourceError::StaleReference { detail: detail() },
                RetryAdvice::AfterRefresh,
            ),
            (
                SourceError::VersionConflict {
                    current: None,
                    detail: detail(),
                },
                RetryAdvice::AfterRefresh,
            ),
            (
                SourceError::CursorRejected { detail: detail() },
                RetryAdvice::AfterRebaseline,
            ),
            (
                SourceError::Unavailable { detail: detail() },
                RetryAdvice::AfterBackoff { minimum: None },
            ),
            (
                SourceError::Cancelled { detail: detail() },
                RetryAdvice::Never,
            ),
            (
                SourceError::Internal { detail: detail() },
                RetryAdvice::Never,
            ),
        ];
        for (error, expected) in cases {
            assert_eq!(error.retry_advice(), expected, "for {error:?}");
        }
    }

    #[test]
    fn display_names_category_and_detail() {
        let err = SourceError::RateLimited {
            retry_after: Some(Duration::from_millis(2000)),
            detail: "FLOOD_WAIT_2".to_owned(),
        };
        assert_eq!(
            err.to_string(),
            "rate limited (retry after 2000 ms): FLOOD_WAIT_2"
        );
        let err = SourceError::StaleReference {
            detail: "reference expired".to_owned(),
        };
        assert_eq!(
            err.to_string(),
            "stale content reference: reference expired"
        );
        let err = SourceError::VersionConflict {
            current: None,
            detail: "edited during fetch".to_owned(),
        };
        assert_eq!(
            err.to_string(),
            "content version conflict: edited during fetch"
        );
    }
}
