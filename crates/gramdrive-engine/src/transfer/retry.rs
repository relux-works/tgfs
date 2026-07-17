//! Retry policy and the engine-side fault classification (SYNC-044,
//! NFR-033).
//!
//! `gramdrive-source` classifies *source* failures and derives what a retry
//! would need ([`SourceError::retry_advice`]); this module owns what the
//! engine actually does about a failed attempt — the bounds, the backoff,
//! and the two local failure classes SYNC-044 names that no source can
//! report (disk full, integrity). The mapping is one exhaustive match, so a
//! new source category cannot ship without a stated engine reaction.
//!
//! The policy reads no clock and no entropy: backoff is a pure function of
//! the persisted retry count, and `now` is always the caller's timestamp
//! (SYNC-073). Determinism here is what makes the machine's tests — and its
//! crash-recovery story — exact rather than probabilistic.

use gramdrive_source::SourceError;
use gramdrive_state::repo::FailureCategory;

/// Bounds and backoff for retryable transfer failures (NFR-033).
///
/// The budget is a count of *failed attempts that may return to the queue*:
/// once the journal's persisted `retry_count` reaches it, the next failure
/// is terminal regardless of category. Parked outcomes (see
/// [`FaultPlan::Park`]) do not consume budget — they leave the queue until
/// an external precondition changes, so they cannot loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Failed attempts allowed back into the queue before the next failure
    /// becomes terminal.
    pub retry_budget: u32,
    /// Backoff before the first retry, in milliseconds; doubles per failed
    /// attempt.
    pub base_backoff_ms: i64,
    /// Ceiling the doubling saturates at, in milliseconds.
    pub max_backoff_ms: i64,
}

impl Default for RetryPolicy {
    /// Five bounded attempts, one second doubling to five minutes — a
    /// starting point for hosts that state no policy, not a tuning claim.
    fn default() -> Self {
        Self {
            retry_budget: 5,
            base_backoff_ms: 1_000,
            max_backoff_ms: 300_000,
        }
    }
}

impl RetryPolicy {
    /// The backoff before the retry that follows `retries_used` failed
    /// attempts: `base * 2^retries_used`, saturating at the ceiling.
    ///
    /// Deterministic on purpose — no jitter. Jitter exists to decorrelate
    /// independent clients hammering one backend, and the place to add it is
    /// the fetch coordinator that owns scheduling (TASK-260715-22fh09), not
    /// the durable machine whose behavior tests replay exactly.
    pub fn backoff_ms(&self, retries_used: u32) -> i64 {
        let doublings = retries_used.min(31);
        self.base_backoff_ms
            .max(0)
            .saturating_mul(1_i64 << doublings)
            .min(self.max_backoff_ms.max(0))
    }
}

/// One failed transfer attempt, in the engine's vocabulary.
///
/// The `detail` strings of the local variants follow the source-error
/// discipline: diagnostic text for logs, never contractual, never persisted
/// — the journal records only the [`FailureCategory`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferFault {
    /// The source failed the fetch (SYNC-044's source-reported classes).
    Source(SourceError),
    /// Local storage could not hold the staged bytes (SYNC-044 local
    /// class; SYNC-054 owns the actionable quota story).
    DiskFull {
        /// Diagnostic detail; not contractual.
        detail: String,
    },
    /// Staged bytes failed verification (SYNC-044 local class, NFR-012):
    /// whatever was staged cannot be trusted, so the attempt starts over.
    Integrity {
        /// Diagnostic detail; not contractual.
        detail: String,
    },
}

/// What the machine does about one classified fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FaultPlan {
    /// Back to the queue with backoff, subject to the retry budget.
    Retry {
        /// The category the journal records.
        category: FailureCategory,
        /// A source-mandated minimum wait (flood wait), which the backoff
        /// must honor even when the policy's own schedule is shorter
        /// (NFR-033, SEC-031).
        source_minimum_ms: Option<i64>,
        /// Whether the staged bytes are invalid and must be discarded
        /// before the retry (integrity failures: re-fetch from scratch).
        wipe_progress: bool,
    },
    /// Suspend with progress kept: a retry cannot succeed until an external
    /// precondition changes (reauthorization, freed disk), so the transfer
    /// leaves the queue instead of polling it. The host resumes suspended
    /// transfers when conditions change; if the precondition still fails,
    /// the next attempt parks again — convergent, and budget-free.
    Park {
        /// The category reported to the caller (the row keeps its last
        /// recorded category; see the module docs of [`crate::transfer`]).
        category: FailureCategory,
    },
    /// Terminal: no retry of this transfer can succeed.
    Final {
        /// The category the journal records.
        category: FailureCategory,
    },
    /// The source observed a cancellation (SYNC-043). The machine resolves
    /// it against the durable cancel flag: acknowledged as cancelled when
    /// the flag is up, parked as a local stop when it is not.
    CancelObserved,
    /// The pinned content version is gone (SYNC-042): partial data is
    /// invalid and the transfer ends; fresh demand re-requests at the
    /// current version.
    Invalidate,
}

/// Maps a fault to the engine's reaction — exhaustive, so a category and
/// its handling cannot drift apart.
pub(crate) fn classify(fault: &TransferFault) -> FaultPlan {
    match fault {
        TransferFault::Source(error) => match error {
            SourceError::InvalidRequest { .. } => FaultPlan::Final {
                category: FailureCategory::InvalidRequest,
            },
            SourceError::NotFound { .. } => FaultPlan::Final {
                category: FailureCategory::NotFound,
            },
            SourceError::Restricted { .. } => FaultPlan::Final {
                category: FailureCategory::Restricted,
            },
            // A content fetch has no cursor to reject; a source that
            // answers one with CursorRejected is misbehaving, and the
            // no-blind-retry rule for unclassifiable failures applies.
            SourceError::Internal { .. } | SourceError::CursorRejected { .. } => FaultPlan::Final {
                category: FailureCategory::Internal,
            },
            SourceError::AuthRequired { .. } => FaultPlan::Park {
                category: FailureCategory::AuthRequired,
            },
            SourceError::RateLimited { retry_after, .. } => FaultPlan::Retry {
                category: FailureCategory::RateLimited,
                source_minimum_ms: retry_after
                    .map(|wait| i64::try_from(wait.as_millis()).unwrap_or(i64::MAX)),
                wipe_progress: false,
            },
            SourceError::Unavailable { .. } => FaultPlan::Retry {
                category: FailureCategory::Unavailable,
                source_minimum_ms: None,
                wipe_progress: false,
            },
            // The locator refresh happens on the next attempt; identity
            // never changes with it (SYNC-045).
            SourceError::StaleReference { .. } => FaultPlan::Retry {
                category: FailureCategory::StaleReference,
                source_minimum_ms: None,
                wipe_progress: false,
            },
            SourceError::VersionConflict { .. } => FaultPlan::Invalidate,
            SourceError::Cancelled { .. } => FaultPlan::CancelObserved,
        },
        TransferFault::DiskFull { .. } => FaultPlan::Park {
            category: FailureCategory::DiskFull,
        },
        TransferFault::Integrity { .. } => FaultPlan::Retry {
            category: FailureCategory::Integrity,
            source_minimum_ms: None,
            wipe_progress: true,
        },
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn backoff_doubles_and_saturates() {
        let policy = RetryPolicy {
            retry_budget: 5,
            base_backoff_ms: 1_000,
            max_backoff_ms: 5_000,
        };
        assert_eq!(policy.backoff_ms(0), 1_000);
        assert_eq!(policy.backoff_ms(1), 2_000);
        assert_eq!(policy.backoff_ms(2), 4_000);
        assert_eq!(policy.backoff_ms(3), 5_000, "ceiling");
        assert_eq!(policy.backoff_ms(u32::MAX), 5_000, "no overflow at depth");

        let unbounded = RetryPolicy {
            retry_budget: 5,
            base_backoff_ms: i64::MAX,
            max_backoff_ms: i64::MAX,
        };
        assert_eq!(unbounded.backoff_ms(40), i64::MAX, "saturates, not wraps");
    }

    #[test]
    fn classification_covers_every_fault() {
        fn detail() -> String {
            "diagnostic".to_owned()
        }
        let cases: Vec<(TransferFault, FaultPlan)> = vec![
            (
                TransferFault::Source(SourceError::InvalidRequest { detail: detail() }),
                FaultPlan::Final {
                    category: FailureCategory::InvalidRequest,
                },
            ),
            (
                TransferFault::Source(SourceError::NotFound { detail: detail() }),
                FaultPlan::Final {
                    category: FailureCategory::NotFound,
                },
            ),
            (
                TransferFault::Source(SourceError::Restricted { detail: detail() }),
                FaultPlan::Final {
                    category: FailureCategory::Restricted,
                },
            ),
            (
                TransferFault::Source(SourceError::Internal { detail: detail() }),
                FaultPlan::Final {
                    category: FailureCategory::Internal,
                },
            ),
            (
                TransferFault::Source(SourceError::CursorRejected { detail: detail() }),
                FaultPlan::Final {
                    category: FailureCategory::Internal,
                },
            ),
            (
                TransferFault::Source(SourceError::AuthRequired { detail: detail() }),
                FaultPlan::Park {
                    category: FailureCategory::AuthRequired,
                },
            ),
            (
                TransferFault::Source(SourceError::RateLimited {
                    retry_after: Some(Duration::from_millis(30_000)),
                    detail: detail(),
                }),
                FaultPlan::Retry {
                    category: FailureCategory::RateLimited,
                    source_minimum_ms: Some(30_000),
                    wipe_progress: false,
                },
            ),
            (
                TransferFault::Source(SourceError::RateLimited {
                    retry_after: None,
                    detail: detail(),
                }),
                FaultPlan::Retry {
                    category: FailureCategory::RateLimited,
                    source_minimum_ms: None,
                    wipe_progress: false,
                },
            ),
            (
                TransferFault::Source(SourceError::Unavailable { detail: detail() }),
                FaultPlan::Retry {
                    category: FailureCategory::Unavailable,
                    source_minimum_ms: None,
                    wipe_progress: false,
                },
            ),
            (
                TransferFault::Source(SourceError::StaleReference { detail: detail() }),
                FaultPlan::Retry {
                    category: FailureCategory::StaleReference,
                    source_minimum_ms: None,
                    wipe_progress: false,
                },
            ),
            (
                TransferFault::Source(SourceError::VersionConflict {
                    current: None,
                    detail: detail(),
                }),
                FaultPlan::Invalidate,
            ),
            (
                TransferFault::Source(SourceError::Cancelled { detail: detail() }),
                FaultPlan::CancelObserved,
            ),
            (
                TransferFault::DiskFull { detail: detail() },
                FaultPlan::Park {
                    category: FailureCategory::DiskFull,
                },
            ),
            (
                TransferFault::Integrity { detail: detail() },
                FaultPlan::Retry {
                    category: FailureCategory::Integrity,
                    source_minimum_ms: None,
                    wipe_progress: true,
                },
            ),
        ];
        for (fault, expected) in cases {
            assert_eq!(classify(&fault), expected, "for {fault:?}");
        }
    }
}
