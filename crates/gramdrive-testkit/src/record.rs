//! Interaction recording — what was asked, and how it ended.
//!
//! A fake that only returns scripted answers proves the caller can read.
//! The recording is what lets a test prove the harder half: that the engine
//! asked for the page size it claims to, that a dropped task actually
//! reached the source as cancellation instead of running to completion
//! unobserved, and that a fetch cut short delivered exactly the bytes it
//! says it did.
//!
//! # Calls are recorded when made, not when polled
//!
//! A `DriveSource` method returns a future; the body of that future does
//! not run until someone polls it. Recording at call time rather than at
//! first poll means `source.children(..)` shows up in the log even if the
//! caller never polls the future — which is itself a bug worth seeing, and
//! it appears here as a [`Call`] whose [`Outcome`] is
//! [`Cancelled`](Outcome::Cancelled) with nothing delivered.
//!
//! # Cancellation is observed by dropping, because that is what it is
//!
//! Every in-flight call holds a guard inside its future. Completing settles
//! it; dropping the future drops the guard unsettled, and the guard records
//! [`Outcome::Cancelled`] with the bytes delivered so far. There is no
//! cancellation *flag* for the fake to forget to set — the recording is
//! driven by the same drop the contract defines cancellation as.
//!
//! # Clearing the log cannot rewrite the calls that outlive it
//!
//! A guard settles by position, and clearing the log makes every position
//! available again — so a call still in flight across a clear would settle
//! whatever entry inherited its index, silently stamping an unrelated
//! call's outcome onto a stranger. The log therefore carries an epoch that
//! [`Recorder::clear`] bumps and each guard remembers. A guard from an
//! earlier epoch settles nothing: its entry was discarded on purpose, and
//! writing anything at all would be a fabrication. This is the one failure
//! mode a fixture whose product is evidence must not have.

use std::sync::{Arc, Mutex};

use gramdrive_model::cursor::ChangeCursor;
use gramdrive_model::identity::ItemId;
use gramdrive_source::{FetchRequest, PageRequest, SourceError, ThumbnailSpec};

use crate::fault::Operation;

/// One `DriveSource` call, with the arguments it carried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    /// [`DriveSource::root`](gramdrive_source::DriveSource::root).
    Root,
    /// [`DriveSource::children`](gramdrive_source::DriveSource::children).
    Children {
        /// The parent whose children were requested.
        parent: ItemId,
        /// The page request as the caller sent it — continuation token and
        /// `max_items` included, so a test can assert the paging the
        /// caller actually performed.
        request: PageRequest,
    },
    /// [`DriveSource::latest_cursor`](gramdrive_source::DriveSource::latest_cursor).
    LatestCursor,
    /// [`DriveSource::changes`](gramdrive_source::DriveSource::changes).
    Changes {
        /// The cursor presented by the caller.
        cursor: ChangeCursor,
    },
    /// [`DriveSource::fetch`](gramdrive_source::DriveSource::fetch).
    Fetch {
        /// The request: item, pinned version, and range.
        request: FetchRequest,
    },
    /// [`DriveSource::thumbnail`](gramdrive_source::DriveSource::thumbnail).
    Thumbnail {
        /// The item a thumbnail was requested for.
        item: ItemId,
        /// The bounding box requested.
        spec: ThumbnailSpec,
    },
}

impl Call {
    /// Which operation this call invoked — the same vocabulary faults are
    /// triggered on ([`Operation`]).
    pub fn operation(&self) -> Operation {
        match self {
            Self::Root => Operation::Root,
            Self::Children { .. } => Operation::Children,
            Self::LatestCursor => Operation::LatestCursor,
            Self::Changes { .. } => Operation::Changes,
            Self::Fetch { .. } => Operation::Fetch,
            Self::Thumbnail { .. } => Operation::Thumbnail,
        }
    }

    /// The item this call targets, when it targets one: the parent for
    /// `children`, the item for `fetch` and `thumbnail`. `None` for the
    /// account-wide operations.
    pub fn item(&self) -> Option<&ItemId> {
        match self {
            Self::Children { parent, .. } => Some(parent),
            Self::Fetch { request } => Some(&request.item),
            Self::Thumbnail { item, .. } => Some(item),
            Self::Root | Self::LatestCursor | Self::Changes { .. } => None,
        }
    }
}

/// How a recorded call ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Still in flight: the future exists and has neither resolved nor
    /// been dropped. Only observable while holding the future — a
    /// completed test never sees it.
    Pending,
    /// Resolved successfully.
    Ok,
    /// Resolved with a failure — scripted or contractual.
    Failed {
        /// What it failed with.
        error: SourceError,
        /// Bytes handed to the sink before the failure surfaced. Always
        /// `0` outside `fetch`, and non-zero exactly where a failure
        /// interrupted a delivery already under way — a
        /// [`VersionRace`](crate::Effect::VersionRace) that conflicted
        /// mid-range, or a sink that asked to stop.
        delivered: u64,
    },
    /// The future was dropped before resolving: cancellation as the
    /// contract defines it (SYNC-005, SYNC-043).
    Cancelled {
        /// Bytes handed to the sink before the drop. Always `0` outside
        /// `fetch`. This is the side-effect evidence a cancellation test
        /// needs: it distinguishes a source that stopped promptly from one
        /// that delivered the whole range and then noticed.
        delivered: u64,
    },
}

impl Outcome {
    /// Whether this call resolved successfully.
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok)
    }

    /// Whether this call was cancelled by a dropped future.
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled { .. })
    }

    /// The error this call failed with, if it failed.
    pub fn error(&self) -> Option<&SourceError> {
        match self {
            Self::Failed { error, .. } => Some(error),
            _ => None,
        }
    }
}

/// One recorded interaction: a call and its fate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interaction {
    /// Position in the call order, from zero. Assigned when the call is
    /// made, so the sequence reflects call order even when the outcomes
    /// settle out of order — which is what concurrent callers do.
    ///
    /// Positional within the current log:
    /// [`clear_interactions`](crate::FakeSource::clear_interactions)
    /// restarts it at zero, so `seq` numbers the calls a test is looking
    /// at rather than every call the source has ever seen.
    pub seq: usize,
    /// The call and its arguments.
    pub call: Call,
    /// How it ended.
    pub outcome: Outcome,
}

/// The log, and the epoch that makes clearing it safe.
#[derive(Debug, Default)]
struct Log {
    entries: Vec<Interaction>,
    /// Bumped by every [`Recorder::clear`]. Guards remember the epoch they
    /// began in and settle only into that one, so a clear cannot be
    /// rewritten by a call that outlived it. See the module docs.
    epoch: u64,
}

/// The shared, append-only interaction log.
///
/// Cloning shares the log — every clone records into the same place, which
/// is what lets the guard inside a future write the outcome the
/// [`FakeSource`](crate::FakeSource) will later report.
#[derive(Debug, Clone, Default)]
pub(crate) struct Recorder {
    log: Arc<Mutex<Log>>,
}

impl Recorder {
    /// An empty log.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Records a call as in-flight and returns the guard that will settle
    /// it. Drop the guard without settling and the call is recorded as
    /// cancelled.
    pub(crate) fn begin(&self, call: Call) -> CallGuard {
        let mut log = lock(&self.log);
        let seq = log.entries.len();
        let epoch = log.epoch;
        log.entries.push(Interaction {
            seq,
            call,
            outcome: Outcome::Pending,
        });
        drop(log);
        CallGuard {
            recorder: self.clone(),
            epoch,
            seq,
            delivered: 0,
            settled: false,
        }
    }

    /// A snapshot of the log so far.
    pub(crate) fn snapshot(&self) -> Vec<Interaction> {
        lock(&self.log).entries.clone()
    }

    /// Drops every recorded interaction and opens a new epoch.
    pub(crate) fn clear(&self) {
        let mut log = lock(&self.log);
        log.entries.clear();
        log.epoch = log.epoch.saturating_add(1);
    }

    fn settle(&self, epoch: u64, seq: usize, outcome: Outcome) {
        let mut log = lock(&self.log);
        // A guard from an earlier epoch has no entry to settle: it was
        // cleared, and `seq` now names somebody else's call.
        if log.epoch != epoch {
            return;
        }
        if let Some(entry) = log.entries.get_mut(seq) {
            entry.outcome = outcome;
        }
    }
}

/// Settles one in-flight call, by completion or by drop.
#[derive(Debug)]
pub(crate) struct CallGuard {
    recorder: Recorder,
    epoch: u64,
    seq: usize,
    delivered: u64,
    settled: bool,
}

impl CallGuard {
    /// Adds to the byte count the outcome will report.
    pub(crate) fn record_delivered(&mut self, bytes: u64) {
        self.delivered = self.delivered.saturating_add(bytes);
    }

    /// Settles the call with its result and hands the result back.
    pub(crate) fn finish<T>(mut self, result: Result<T, SourceError>) -> Result<T, SourceError> {
        let outcome = match &result {
            Ok(_) => Outcome::Ok,
            Err(error) => Outcome::Failed {
                error: error.clone(),
                delivered: self.delivered,
            },
        };
        self.settled = true;
        self.recorder.settle(self.epoch, self.seq, outcome);
        result
    }
}

impl Drop for CallGuard {
    fn drop(&mut self) {
        if !self.settled {
            self.recorder.settle(
                self.epoch,
                self.seq,
                Outcome::Cancelled {
                    delivered: self.delivered,
                },
            );
        }
    }
}

/// Locks a mutex, recovering from poisoning.
///
/// A poisoned mutex here means a test panicked while holding the log — that
/// test has already failed, and the recorded data is a `Vec` that no
/// panicking path leaves logically torn. Propagating the poison would turn
/// one failing test into a cascade of unrelated failures, and unwrapping
/// would trip the workspace's `unwrap_used` deny for no benefit.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU32;

    fn page_request() -> PageRequest {
        PageRequest::first(NonZeroU32::new(10).unwrap())
    }

    #[test]
    fn completing_a_call_records_success() {
        let recorder = Recorder::new();
        let guard = recorder.begin(Call::Root);
        let result: Result<(), SourceError> = guard.finish(Ok(()));
        assert!(result.is_ok());

        let log = recorder.snapshot();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].seq, 0);
        assert_eq!(log[0].call, Call::Root);
        assert_eq!(log[0].outcome, Outcome::Ok);
    }

    #[test]
    fn failing_a_call_records_the_error() {
        let recorder = Recorder::new();
        let guard = recorder.begin(Call::LatestCursor);
        let error = SourceError::Unavailable {
            detail: "offline".to_owned(),
        };
        let result: Result<(), SourceError> = guard.finish(Err(error.clone()));
        assert!(result.is_err());

        let log = recorder.snapshot();
        assert_eq!(
            log[0].outcome,
            Outcome::Failed {
                error: error.clone(),
                delivered: 0,
            }
        );
        assert_eq!(log[0].outcome.error(), Some(&error));
        assert!(!log[0].outcome.is_ok());
    }

    #[test]
    fn a_failure_records_the_bytes_it_had_already_delivered() {
        // The side-effect evidence for an interrupted delivery: a version
        // race that conflicts mid-range moved bytes before it failed, and
        // the record has to say so without the test consulting the sink.
        let recorder = Recorder::new();
        let mut guard = recorder.begin(Call::Root);
        guard.record_delivered(8);
        let error = SourceError::VersionConflict {
            current: None,
            detail: "changed underneath".to_owned(),
        };
        let _: Result<(), SourceError> = guard.finish(Err(error.clone()));

        assert_eq!(
            recorder.snapshot()[0].outcome,
            Outcome::Failed {
                error,
                delivered: 8,
            }
        );
    }

    #[test]
    fn dropping_a_guard_records_cancellation_with_delivered_bytes() {
        let recorder = Recorder::new();
        let mut guard = recorder.begin(Call::Root);
        guard.record_delivered(120);
        guard.record_delivered(8);
        drop(guard);

        let log = recorder.snapshot();
        assert_eq!(log[0].outcome, Outcome::Cancelled { delivered: 128 });
        assert!(log[0].outcome.is_cancelled());
    }

    #[test]
    fn an_unsettled_call_reads_as_pending_while_in_flight() {
        let recorder = Recorder::new();
        let guard = recorder.begin(Call::Root);
        assert_eq!(recorder.snapshot()[0].outcome, Outcome::Pending);
        drop(guard);
    }

    #[test]
    fn sequence_numbers_follow_call_order_not_settle_order() {
        let recorder = Recorder::new();
        let first = recorder.begin(Call::Root);
        let second = recorder.begin(Call::LatestCursor);
        // Settle in reverse: concurrent callers do exactly this.
        let _ = second.finish::<()>(Ok(()));
        let _ = first.finish::<()>(Ok(()));

        let log = recorder.snapshot();
        assert_eq!(log[0].seq, 0);
        assert_eq!(log[0].call, Call::Root);
        assert_eq!(log[1].seq, 1);
        assert_eq!(log[1].call, Call::LatestCursor);
    }

    #[test]
    fn calls_expose_operation_and_target_item() {
        let item = crate::fixture::account_root_id(crate::fixture::scope());
        let call = Call::Children {
            parent: item.clone(),
            request: page_request(),
        };
        assert_eq!(call.operation(), Operation::Children);
        assert_eq!(call.item(), Some(&item));
        assert_eq!(Call::Root.operation(), Operation::Root);
        assert_eq!(Call::Root.item(), None);
        assert_eq!(Call::LatestCursor.item(), None);
    }

    #[test]
    fn clearing_drops_the_log() {
        let recorder = Recorder::new();
        let _ = recorder.begin(Call::Root).finish::<()>(Ok(()));
        assert_eq!(recorder.snapshot().len(), 1);
        recorder.clear();
        assert!(recorder.snapshot().is_empty());
        assert_eq!(
            recorder.begin(Call::Root).seq,
            0,
            "seq numbers the calls after the clear, from zero"
        );
    }

    #[test]
    fn a_call_cleared_while_in_flight_cannot_settle_a_later_call() {
        // The index a cleared call held is handed to the next one. Without
        // an epoch the stale guard settles that entry instead, stamping a
        // dropped future's `Cancelled` onto a call that succeeded — a
        // plausible-looking lie, which is the worst kind here.
        let recorder = Recorder::new();
        let stale = recorder.begin(Call::Root);

        recorder.clear();

        let _ = recorder.begin(Call::LatestCursor).finish::<()>(Ok(()));
        drop(stale);

        let log = recorder.snapshot();
        assert_eq!(log.len(), 1, "the cleared call does not come back");
        assert_eq!(log[0].call, Call::LatestCursor);
        assert_eq!(
            log[0].outcome,
            Outcome::Ok,
            "the surviving call keeps the outcome it actually had"
        );
    }

    #[test]
    fn a_call_cleared_while_in_flight_settles_nothing_into_an_empty_log() {
        // The other half: with nothing to overwrite, the stale outcome must
        // be dropped rather than resurrect its entry.
        let recorder = Recorder::new();
        let stale = recorder.begin(Call::Root);
        recorder.clear();
        drop(stale);

        assert!(
            recorder.snapshot().is_empty(),
            "a cleared call stays cleared"
        );
    }

    #[test]
    fn calls_begun_after_a_clear_settle_normally() {
        // The epoch must not break the ordinary path it protects.
        let recorder = Recorder::new();
        let _ = recorder.begin(Call::Root).finish::<()>(Ok(()));
        recorder.clear();

        let first = recorder.begin(Call::Root);
        let second = recorder.begin(Call::LatestCursor);
        drop(second);
        let _ = first.finish::<()>(Ok(()));

        let log = recorder.snapshot();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].outcome, Outcome::Ok);
        assert!(log[1].outcome.is_cancelled());
    }

    #[test]
    fn a_poisoned_lock_is_recovered_rather_than_propagated() {
        let recorder = Recorder::new();
        let poisoner = recorder.clone();
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.begin(Call::Root);
            panic!("poison the log mutex");
        })
        .join();

        // The log survives, and the panicking thread's guard recorded its
        // own cancellation on unwind.
        let log = recorder.snapshot();
        assert_eq!(log.len(), 1);
        assert!(log[0].outcome.is_cancelled());
    }
}
