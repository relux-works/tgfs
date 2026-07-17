//! The vocabulary cases are written in.
//!
//! Cases return [`CaseError`] rather than panicking, so this module supplies
//! what `assert!` would have: [`require!`] for a claim, [`expect_ok`] and
//! [`expect_err`] for a call that must or must not succeed. The cost of not
//! panicking is that every helper threads a `Result`; the return is that a
//! broken clause becomes a line in a report instead of a stack trace, and
//! that this module is ordinary library code the workspace's lints apply to
//! in full.
//!
//! [`require!`]: crate::conformance::support::require

use std::future::Future;
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};

use gramdrive_model::cursor::ChangeCursor;
use gramdrive_model::identity::ItemId;
use gramdrive_source::{
    DriveSource, FetchRequest, ItemChange, ItemPage, PageRequest, PageToken, SourceError,
    SourceItem,
};

use crate::conformance::harness::SourceHarness;
use crate::conformance::report::{CaseError, Failure};
use crate::sink::RecordingSink;

/// How one case ends.
pub(crate) type CaseResult = Result<(), CaseError>;

/// Pages an enumeration may take before the suite calls it non-terminating.
///
/// Far above any page count [`WORLD`] can produce at the page sizes cases
/// use, so reaching it means the source keeps handing back a continuation it
/// never honors — a contract failure, and one that would otherwise hang the
/// suite rather than report itself.
const MAX_PAGES: usize = 1024;

/// Fails the case unless `condition` holds.
///
/// The message is the observation: what the source did, in contract terms.
/// The clause it broke is already on the case, so the message must not repeat
/// it.
macro_rules! require {
    ($condition:expr, $($detail:tt)+) => {
        if !$condition {
            return ::core::result::Result::Err(
                $crate::conformance::report::Failure::new(format!($($detail)+)).into(),
            );
        }
    };
}
pub(crate) use require;

/// A page request for `max` items.
///
/// `max` is a literal at every call site, so the zero fallback is
/// unreachable; it exists because `NonZeroU32::new` is not const-callable
/// here without an unwrap the workspace forbids.
pub(crate) fn page_request(max: u32) -> PageRequest {
    PageRequest::first(NonZeroU32::new(max).unwrap_or(NonZeroU32::MIN))
}

/// A request continuing an enumeration from `token`, `max` items at a time.
pub(crate) fn continue_from(token: PageToken, max: u32) -> PageRequest {
    PageRequest {
        continuation: Some(token),
        max_items: NonZeroU32::new(max).unwrap_or(NonZeroU32::MIN),
    }
}

/// Unwraps a call that the contract says must succeed.
pub(crate) fn expect_ok<T>(result: Result<T, SourceError>, what: &str) -> Result<T, CaseError> {
    result.map_err(|error| Failure::new(format!("{what} must succeed, but failed: {error}")).into())
}

/// Unwraps a call that the contract says must fail.
pub(crate) fn expect_err<T>(
    result: Result<T, SourceError>,
    what: &str,
) -> Result<SourceError, CaseError> {
    match result {
        Err(error) => Ok(error),
        Ok(_) => Err(Failure::new(format!("{what} must fail, but it succeeded")).into()),
    }
}

/// Every page of one enumeration of `parent`, at `page_size` items per page.
///
/// Stops at the first page without a continuation, and fails the case if the
/// source never produces one.
pub(crate) fn enumerate(
    harness: &impl SourceHarness,
    source: &dyn DriveSource,
    parent: &ItemId,
    page_size: u32,
) -> Result<Vec<ItemPage>, CaseError> {
    enumerate_from(harness, source, parent, page_request(page_size))
}

/// Every page of an enumeration continuing from `request`.
///
/// The half of [`enumerate`] a case needs when it has already taken the first
/// page by hand — to look at it, or to change the world behind it.
pub(crate) fn enumerate_from(
    harness: &impl SourceHarness,
    source: &dyn DriveSource,
    parent: &ItemId,
    request: PageRequest,
) -> Result<Vec<ItemPage>, CaseError> {
    let mut request = request;
    let mut pages = Vec::new();
    loop {
        let page = expect_ok(
            harness.block_on(source.children(parent.clone(), request.clone())),
            "enumerating a directory",
        )?;
        let next = page.next.clone();
        pages.push(page);

        match next {
            None => return Ok(pages),
            Some(token) => {
                require!(
                    pages.len() < MAX_PAGES,
                    "enumeration did not terminate: {MAX_PAGES} pages served and the source \
                     still returns a continuation"
                );
                request = PageRequest {
                    continuation: Some(token),
                    max_items: request.max_items,
                };
            }
        }
    }
}

/// The item `id`, found by enumerating `parent`.
///
/// The contract has no "get by identity": a source is asked for a root and
/// for children, and everything else is reached through them. So is this.
pub(crate) fn find_item(
    harness: &impl SourceHarness,
    source: &dyn DriveSource,
    parent: &ItemId,
    id: &ItemId,
) -> Result<SourceItem, CaseError> {
    let pages = enumerate(harness, source, parent, 32)?;
    pages
        .iter()
        .flat_map(|page| page.items.iter())
        .find(|item| &item.id == id)
        .cloned()
        .ok_or_else(|| Failure::new(format!("no child of {parent} has identity {id}")).into())
}

/// Every change from `cursor` until the feed drains, and the cursor that
/// leaves the caller level with the source.
pub(crate) fn drain_changes(
    harness: &impl SourceHarness,
    source: &dyn DriveSource,
    cursor: ChangeCursor,
) -> Result<(Vec<ItemChange>, ChangeCursor), CaseError> {
    let mut cursor = cursor;
    let mut changes = Vec::new();
    for _ in 0..MAX_PAGES {
        let page = expect_ok(
            harness.block_on(source.changes(cursor.clone())),
            "reading the change feed",
        )?;
        changes.extend(page.changes);
        cursor = page.next;
        if !page.more_available {
            return Ok((changes, cursor));
        }
    }
    Err(Failure::new(format!(
        "the change feed never drained: {MAX_PAGES} pages read and the source still reports \
         more changes available"
    ))
    .into())
}

/// Whether `changes` carries an upsert of `id`.
pub(crate) fn upserts(changes: &[ItemChange], id: &ItemId) -> bool {
    changes
        .iter()
        .any(|change| matches!(change, ItemChange::Upserted(item) if &item.id == id))
}

/// Whether `changes` carries a removal of `id`.
pub(crate) fn removes(changes: &[ItemChange], id: &ItemId) -> bool {
    changes
        .iter()
        .any(|change| matches!(change, ItemChange::Removed(removed) if removed == id))
}

/// The identity of every item across `pages`, in the order they were served.
pub(crate) fn served_ids(pages: &[ItemPage]) -> Vec<ItemId> {
    pages
        .iter()
        .flat_map(|page| page.items.iter().map(|item| item.id.clone()))
        .collect()
}

/// Reports any identity served twice across an enumeration.
pub(crate) fn first_duplicate(ids: &[ItemId]) -> Option<&ItemId> {
    ids.iter()
        .enumerate()
        .find(|(index, id)| ids[..*index].contains(id))
        .map(|(_, id)| id)
}

/// Runs one fetch into a fresh [`RecordingSink`] and returns both.
pub(crate) fn fetch(
    harness: &impl SourceHarness,
    source: &dyn DriveSource,
    request: FetchRequest,
) -> (Result<(), SourceError>, RecordingSink) {
    let mut sink = RecordingSink::new(request.range);
    let result = harness.block_on(source.fetch(request, &mut sink));
    (result, sink)
}

/// Drives `future` until `abandon` says to stop, then drops it.
///
/// Dropping a future *is* cancellation (SYNC-005, SYNC-043), and this is how
/// a case cancels one without knowing what drives it. `abandon` is consulted
/// at each poll, before the inner future is polled; once it answers `true`
/// the inner future is never polled again and is dropped with the returned
/// future.
///
/// Runtime-agnostic on purpose: it wraps the future rather than reaching for
/// an executor, so the same case cancels a fake driven by
/// [`exec`](crate::exec) and a `tdjson` source driven by tokio.
pub(crate) fn until_abandoned<F: Future>(
    future: F,
    abandon: impl Fn() -> bool,
) -> impl Future<Output = Option<F::Output>> {
    let mut future = Box::pin(future);
    std::future::poll_fn(move |context: &mut Context<'_>| {
        if abandon() {
            return Poll::Ready(None);
        }
        future.as_mut().poll(context).map(Some)
    })
}

/// An `abandon` predicate that gives up after `polls` polls.
///
/// For calls that deliver nothing to wait on — a slow `root` has no bytes to
/// count, only suspension points.
pub(crate) fn after_polls(polls: u64) -> impl Fn() -> bool {
    let seen = AtomicU64::new(0);
    move || seen.fetch_add(1, Ordering::AcqRel) >= polls
}

/// Runs `first` and `second` concurrently, resolving when both have.
///
/// Sources are shared and callers may issue concurrent operations; a source's
/// obligation is that concurrent calls never corrupt each other's answers
/// (SYNC-046). Testing that needs two calls genuinely in flight at once,
/// which `block_on` alone cannot arrange — it drives one future.
///
/// Hand-rolled rather than taken from `futures`: this crate has no async
/// dependency by design, and the whole of what a two-way join needs is to
/// poll both sides on every poll and finish when neither is outstanding. It
/// interleaves under any executor, so a case written against it means the
/// same thing driven by [`exec`](crate::exec) or by tokio.
pub(crate) fn both<A: Future, B: Future>(
    first: A,
    second: B,
) -> impl Future<Output = (A::Output, B::Output)> {
    let mut first = Box::pin(first);
    let mut second = Box::pin(second);
    let mut first_out: Option<A::Output> = None;
    let mut second_out: Option<B::Output> = None;

    std::future::poll_fn(move |context: &mut Context<'_>| {
        if first_out.is_none()
            && let Poll::Ready(value) = first.as_mut().poll(context)
        {
            first_out = Some(value);
        }
        if second_out.is_none()
            && let Poll::Ready(value) = second.as_mut().poll(context)
        {
            second_out = Some(value);
        }
        match (first_out.take(), second_out.take()) {
            (Some(a), Some(b)) => Poll::Ready((a, b)),
            // Put back whichever finished; the other is still running.
            (a, b) => {
                first_out = a;
                second_out = b;
                Poll::Pending
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec;
    use std::sync::Arc;

    #[test]
    fn require_returns_a_failure_carrying_its_detail() {
        fn case(ok: bool) -> CaseResult {
            require!(ok, "the source served {} items", 3);
            Ok(())
        }
        assert_eq!(case(true), Ok(()));
        assert_eq!(
            case(false),
            Err(CaseError::Contract(Failure::new(
                "the source served 3 items"
            )))
        );
    }

    #[test]
    fn expect_ok_turns_an_unexpected_failure_into_a_case_failure() {
        let error = SourceError::NotFound {
            detail: "gone".to_owned(),
        };
        let result: Result<(), CaseError> = expect_ok(Err::<(), _>(error), "fetching the file");
        match result {
            Err(CaseError::Contract(failure)) => {
                assert!(failure.detail().contains("fetching the file must succeed"));
                assert!(failure.detail().contains("not found: gone"));
            }
            other => panic!("expected a contract failure, got {other:?}"),
        }
    }

    #[test]
    fn expect_err_turns_an_unexpected_success_into_a_case_failure() {
        let result = expect_err(Ok::<_, SourceError>(()), "enumerating a file");
        match result {
            Err(CaseError::Contract(failure)) => {
                assert!(failure.detail().contains("must fail, but it succeeded"));
            }
            other => panic!("expected a contract failure, got {other:?}"),
        }
    }

    #[test]
    fn first_duplicate_finds_a_repeated_identity() {
        let a = crate::fixture::chat_id(crate::fixture::scope(), 1);
        let b = crate::fixture::chat_id(crate::fixture::scope(), 2);
        assert_eq!(first_duplicate(&[a.clone(), b.clone()]), None);
        assert_eq!(first_duplicate(&[a.clone(), b, a.clone()]), Some(&a));
    }

    #[test]
    fn until_abandoned_returns_the_value_when_it_never_abandons() {
        let value = exec::drive(until_abandoned(async { 7 }, || false));
        assert_eq!(value, Some(7));
    }

    #[test]
    fn until_abandoned_drops_the_future_once_it_gives_up() {
        // A future that would never finish: only abandonment ends this.
        let outcome = exec::drive(until_abandoned(
            std::future::pending::<u8>(),
            after_polls(3),
        ));
        assert_eq!(outcome, None);
    }

    #[test]
    fn after_polls_gives_the_future_exactly_that_many_polls() {
        let polled = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&polled);
        let future = std::future::poll_fn(move |context| {
            counter.fetch_add(1, Ordering::AcqRel);
            context.waker().wake_by_ref();
            Poll::<u8>::Pending
        });
        assert_eq!(exec::drive(until_abandoned(future, after_polls(2))), None);
        assert_eq!(
            polled.load(Ordering::Acquire),
            2,
            "abandoned on the third check, having polled twice"
        );
    }
}
