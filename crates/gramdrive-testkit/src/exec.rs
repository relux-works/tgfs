//! A deterministic single-threaded executor for driving source futures.
//!
//! The `DriveSource` contract is asynchronous, so testing it needs
//! *something* to poll futures — but not a runtime. A real runtime would
//! bring back everything this crate exists to remove: a thread pool whose
//! interleavings vary run to run, timers that tie a test's outcome to the
//! host's clock, and a dependency the testkit's consumers would inherit.
//! The loop here polls on the calling thread, in one order, forever
//! reproducible.
//!
//! # Waking is the caller's contract, not a formality
//!
//! [`poll_n`] and [`try_drive`] pass [`Waker::noop`], so a future that
//! parks expecting to be woken by a timer or an I/O reactor never
//! progresses here — it burns the poll budget instead. That is a deliberate
//! filter. [`FakeSource`](crate::FakeSource) yields by waking itself before
//! returning `Pending` (the `yield_now` pattern), which is exactly what
//! makes it drivable both by this loop and by a real runtime like the
//! engine's tokio.
//!
//! # Cancellation testing
//!
//! Dropping a future *is* cancellation (SYNC-005, SYNC-043). [`poll_n`]
//! exists to reach a chosen point and stop: poll a fetch a few times, drop
//! it mid-delivery, then read
//! [`FakeSource::interactions`](crate::FakeSource::interactions) to see the
//! [`Outcome::Cancelled`](crate::Outcome::Cancelled) the source recorded
//! and how many bytes it had delivered.
//!
//! ```
//! # use gramdrive_testkit::exec;
//! let value = exec::drive(async { 1 + 1 });
//! assert_eq!(value, 2);
//! ```

use std::future::Future;
use std::pin::{Pin, pin};
use std::task::{Context, Poll, Waker};

/// Poll budget [`drive`] allows before declaring a future stuck.
///
/// Far above any scripted fixture — the fake's poll count is bounded by its
/// scripted delays plus one poll per delivered chunk — and low enough to
/// fail in under a second rather than hang a suite.
pub const DEFAULT_POLL_BUDGET: usize = 1_000_000;

/// A future did not complete within its poll budget.
///
/// With [`Waker::noop`] driving, this means the future is waiting on
/// something this executor cannot supply — a real timer or reactor — or a
/// scripted delay outran the budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetExhausted {
    /// The budget that was consumed.
    pub budget: usize,
}

impl std::fmt::Display for BudgetExhausted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "future did not complete within {} polls; it is waiting on a waker \
             this executor never fires (a real timer or reactor), or its scripted \
             delay exceeds the budget",
            self.budget
        )
    }
}

impl std::error::Error for BudgetExhausted {}

/// Polls `future` to completion on the current thread.
///
/// Use [`try_drive`] to handle a stuck future rather than fail on it.
///
/// # Panics
///
/// If the future does not complete within [`DEFAULT_POLL_BUDGET`] polls.
// The workspace denies `panic` because a panic in the *core* is an aborted
// File Provider extension or a lost error category (NFR-030). None of that
// reasoning reaches here: this crate is a dev-dependency by architecture
// rule and never links into a product artifact, so this panic can only ever
// surface as a failing test — which, per clippy.toml's own note on the
// test-only exemptions, is just a failing test. The alternative is every
// caller unwrapping a `Result` whose error means "the fixture is broken",
// which buys no safety and costs every call site.
#[allow(clippy::panic)]
pub fn drive<F: Future>(future: F) -> F::Output {
    match try_drive(future, DEFAULT_POLL_BUDGET) {
        Ok(output) => output,
        Err(exhausted) => panic!("{exhausted}"),
    }
}

/// Polls `future` to completion, giving up after `budget` polls.
///
/// The non-panicking [`drive`]: returns [`BudgetExhausted`] instead of
/// failing the test, for callers that want to assert a future is *not*
/// ready.
pub fn try_drive<F: Future>(future: F, budget: usize) -> Result<F::Output, BudgetExhausted> {
    let mut future = pin!(future);
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    for _ in 0..budget {
        if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return Ok(output);
        }
    }
    Err(BudgetExhausted { budget })
}

/// Polls `future` at most `polls` times and reports where it stands.
///
/// The cancellation primitive: drive a future to a chosen point, then drop
/// it to cancel there. Returns [`Poll::Pending`] if it is still running
/// after `polls` polls, having consumed exactly that many.
///
/// ```
/// # use gramdrive_testkit::exec;
/// # use std::pin::pin;
/// # use std::task::Poll;
/// let mut count = 0;
/// let future = pin!(async {
///     for _ in 0..3 {
///         std::future::pending::<()>().await;
///     }
/// });
/// assert_eq!(exec::poll_n(future, 2), Poll::Pending);
/// # let _ = count;
/// ```
pub fn poll_n<F: Future>(mut future: Pin<&mut F>, polls: usize) -> Poll<F::Output> {
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    for _ in 0..polls {
        if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return Poll::Ready(output);
        }
    }
    Poll::Pending
}

/// Yields to the executor `remaining` times before completing.
///
/// Every yield is a cancellation point: the future is dropped there if the
/// caller stops polling (SYNC-043). It wakes itself before returning
/// `Pending` — the `yield_now` pattern — so it is drivable by this crate's
/// noop-waker loop *and* by a real runtime, rather than parking forever on
/// one of them.
///
/// This is what a scripted "delay" is in a deterministic fake: a bounded
/// number of scheduling points, not a duration. See [`crate::fault`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct Yield {
    remaining: u32,
}

impl Yield {
    /// Yields `count` times; `0` completes on the first poll.
    pub(crate) fn new(count: u32) -> Self {
        Self { remaining: count }
    }

    /// Yields exactly once — the between-chunks cancellation point.
    pub(crate) fn once() -> Self {
        Self::new(1)
    }
}

impl Future for Yield {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<()> {
        if self.remaining == 0 {
            return Poll::Ready(());
        }
        self.remaining -= 1;
        context.waker().wake_by_ref();
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::pending;

    #[test]
    fn drive_returns_a_ready_value() {
        assert_eq!(drive(async { 7 }), 7);
    }

    #[test]
    fn drive_polls_through_yields() {
        let value = drive(async {
            Yield::new(5).await;
            "done"
        });
        assert_eq!(value, "done");
    }

    #[test]
    fn yield_consumes_exactly_the_requested_polls() {
        let future = pin!(async {
            Yield::new(3).await;
            "done"
        });
        // Three polls yield; the fourth completes.
        assert_eq!(poll_n(future, 4), Poll::Ready("done"));

        let future = pin!(async {
            Yield::new(3).await;
            "done"
        });
        assert_eq!(poll_n(future, 3), Poll::Pending, "one poll short");
    }

    #[test]
    fn zero_yields_is_ready_immediately() {
        let future = pin!(Yield::new(0));
        assert_eq!(poll_n(future, 1), Poll::Ready(()));
    }

    #[test]
    fn try_drive_reports_a_stuck_future() {
        let error = try_drive(pending::<()>(), 32).expect_err("pending never completes");
        assert_eq!(error, BudgetExhausted { budget: 32 });
        assert!(error.to_string().contains("32 polls"));
    }

    #[test]
    fn poll_n_leaves_a_pending_future_droppable() {
        // Dropping after a partial poll is exactly how cancellation is
        // tested against the fake; assert the shape works at all.
        let mut future = Box::pin(async {
            Yield::new(u32::MAX).await;
        });
        assert_eq!(poll_n(future.as_mut(), 4), Poll::Pending);
        drop(future);
    }
}
