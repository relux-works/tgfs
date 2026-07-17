//! The normalized failure taxonomy (SYNC-044).
//!
//! # What is worth asserting about a failure, and what is not
//!
//! `SourceError::retry_advice` is derived from the category by
//! `gramdrive-source`, in one exhaustive match. Asserting that an
//! `Unavailable` advises `AfterBackoff` therefore tests that crate's
//! arithmetic, not this backend's behavior — a source *cannot* get it wrong
//! once it has picked the category. So the cases here pin the category, which
//! is the backend's actual obligation, and let the classification follow.
//!
//! What is not derived is [`SourceError::RateLimited::retry_after`]: a number
//! the backend read off a flood wait and had to carry across the boundary
//! intact. A source that normalizes `FLOOD_WAIT_2` into a bare `RateLimited`
//! with no duration has dropped the only thing that makes the error
//! actionable, and nothing but [`a_rate_limit_carries_its_backoff`] would
//! notice.
//!
//! # Recovery is the other half
//!
//! A category that says "retryable" and then never recovers is worse than one
//! that says "never": the engine spends its whole retry budget finding out.
//! [`a_transient_failure_recovers_on_retry`] asserts both halves — the
//! failure is transient, *and* the identical request succeeds afterwards,
//! delivering the whole content.

use std::time::Duration;

use gramdrive_model::ByteRange;
use gramdrive_source::{DriveSource, FetchRequest, SourceError};

use crate::conformance::cases::Case;
use crate::conformance::harness::{Perturbation, Setup, SourceHarness, Staged, WORLD};
use crate::conformance::report::{Clause, Failure};
use crate::conformance::support::{CaseResult, expect_err, fetch, find_item, require};
use crate::fault::Operation;

/// The backoff the rate-limit case expects to survive the boundary.
///
/// A whole number of seconds because a real flood wait is: Telegram's
/// `FLOOD_WAIT_n` carries integer seconds, and a suite that demanded a backend
/// reproduce 1500 ms would be demanding it stage something its backend cannot
/// say.
const BACKOFF: Duration = Duration::from_secs(2);

pub(crate) fn cases<H: SourceHarness>() -> Vec<Case<H>> {
    vec![
        Case {
            id: "failure.an-unreachable-source-recovers-on-retry",
            clause: Clause::Sync044,
            claim: "an unreachable source is reported as transient, and the identical request \
                    succeeds once it is back",
            needs: &[],
            setup: || {
                Setup::new().arm(Perturbation::Unreachable {
                    operation: Operation::Fetch,
                })
            },
            run: an_unreachable_source_recovers_on_retry::<H>,
        },
        Case {
            id: "failure.an-expired-reference-is-refreshable",
            clause: Clause::Sync044,
            claim: "an expired content reference is reported as such — refreshable, not a \
                    deletion and not a network fault — and a retry then succeeds",
            needs: &[],
            setup: || {
                Setup::new().arm(Perturbation::ReferenceExpired {
                    operation: Operation::Fetch,
                })
            },
            run: an_expired_reference_is_refreshable::<H>,
        },
        Case {
            id: "failure.a-reference-refresh-does-not-move-identity",
            clause: Clause::Sync045,
            claim: "an item whose content reference expired and was refreshed keeps the \
                    identity it had before",
            needs: &[],
            setup: || {
                Setup::new().arm(Perturbation::ReferenceExpired {
                    operation: Operation::Fetch,
                })
            },
            run: a_reference_refresh_does_not_move_identity::<H>,
        },
        Case {
            id: "failure.a-rate-limit-carries-its-backoff",
            clause: Clause::Sync044,
            claim: "a throttled source carries the backoff it was given across the boundary",
            needs: &[],
            setup: || {
                Setup::new().arm(Perturbation::RateLimited {
                    operation: Operation::Fetch,
                    retry_after: Some(BACKOFF),
                })
            },
            run: a_rate_limit_carries_its_backoff::<H>,
        },
        Case {
            id: "failure.lost-authorization-is-reported-as-such",
            clause: Clause::Sync044,
            claim: "a source with no usable authorization says so, rather than reporting a \
                    transient fault a retry could fix",
            needs: &[],
            setup: || {
                Setup::new().arm(Perturbation::AuthRevoked {
                    operation: Operation::Root,
                })
            },
            run: lost_authorization_is_reported_as_such::<H>,
        },
    ]
}

/// A fetch of the world's file's whole extent, pinned to its version.
fn whole_file<S>(staged: &Staged<S>) -> Result<FetchRequest, Failure> {
    let range = ByteRange::new(0, WORLD.file_bytes.len() as u64)
        .map_err(|error| Failure::new(format!("the world's file has no valid extent: {error}")))?;
    Ok(FetchRequest {
        item: staged.landmarks.file.clone(),
        version: staged.landmarks.file_version.clone(),
        range,
    })
}

fn an_unreachable_source_recovers_on_retry<H: SourceHarness>(
    harness: &H,
    staged: Staged<H::Source>,
) -> CaseResult {
    let request = whole_file(&staged)?;

    let (result, sink) = fetch(harness, staged.source.as_ref(), request.clone());
    let error = expect_err(result, "a fetch against an unreachable source")?;
    require!(
        matches!(error, SourceError::Unavailable { .. }),
        "an unreachable source must fail with Unavailable — the category that tells the \
         engine to back off and try again; got {error}"
    );
    require!(
        sink.bytes().is_empty(),
        "a failed fetch delivered {} bytes",
        sink.bytes().len()
    );

    // The other half: transient must mean it actually recovers.
    let (retried, sink) = fetch(harness, staged.source.as_ref(), request);
    if let Err(error) = retried {
        return Err(Failure::new(format!(
            "the source reported a transient failure, but the identical request failed again \
             with {error}"
        ))
        .into());
    }
    require!(
        sink.violation().is_none(),
        "the retried delivery broke the range contract: {:?}",
        sink.violation()
    );
    require!(
        sink.bytes() == WORLD.file_bytes,
        "the retry succeeded but delivered {} of the file's {} bytes",
        sink.bytes().len(),
        WORLD.file_bytes.len()
    );
    Ok(())
}

/// An expired reference is the one failure with a protocol rather than a
/// wait: the locator went stale, the adapter refreshes it, the caller retries.
/// Reported as `Unavailable` the engine would back off and retry blindly;
/// reported as `NotFound` it would tombstone an item that never went anywhere.
/// Both are worse than the truth, and both are easy mistakes to make.
fn an_expired_reference_is_refreshable<H: SourceHarness>(
    harness: &H,
    staged: Staged<H::Source>,
) -> CaseResult {
    let request = whole_file(&staged)?;

    let (result, sink) = fetch(harness, staged.source.as_ref(), request.clone());
    let error = expect_err(result, "a fetch whose content reference expired")?;
    require!(
        matches!(error, SourceError::StaleReference { .. }),
        "an expired content reference must fail with StaleReference — the item is still \
         there and its bytes are still fetchable, so neither NotFound nor Unavailable \
         describes what happened; got {error}"
    );
    require!(
        sink.bytes().is_empty(),
        "a fetch that failed on an expired reference delivered {} bytes",
        sink.bytes().len()
    );

    // A refreshable reference must actually be refreshable.
    let (retried, sink) = fetch(harness, staged.source.as_ref(), request);
    if let Err(error) = retried {
        return Err(Failure::new(format!(
            "the source reported a refreshable reference, but the retry that should have \
             followed the refresh failed with {error}"
        ))
        .into());
    }
    require!(
        sink.bytes() == WORLD.file_bytes,
        "the retry after a reference refresh delivered {} of the file's {} bytes",
        sink.bytes().len(),
        WORLD.file_bytes.len()
    );
    Ok(())
}

/// DOM-007: a content reference is refreshable metadata, never identity. A
/// source that mints a new [`ItemId`] when a reference expires re-parents the
/// user's file — every pin, cursor, and cached row naming the old identity is
/// orphaned by a purely internal event.
fn a_reference_refresh_does_not_move_identity<H: SourceHarness>(
    harness: &H,
    staged: Staged<H::Source>,
) -> CaseResult {
    let before = find_item(
        harness,
        staged.source.as_ref(),
        &staged.landmarks.file_parent,
        &staged.landmarks.file,
    )?;

    // Burn the expired reference, and the refresh behind it.
    let request = whole_file(&staged)?;
    let (failed, _) = fetch(harness, staged.source.as_ref(), request.clone());
    expect_err(failed, "a fetch whose content reference expired")?;
    let (retried, _) = fetch(harness, staged.source.as_ref(), request);
    if let Err(error) = retried {
        return Err(Failure::new(format!(
            "the retry after a reference refresh failed with {error}, so whether identity \
             survived a refresh was never observed"
        ))
        .into());
    }

    let after = find_item(
        harness,
        staged.source.as_ref(),
        &staged.landmarks.file_parent,
        &staged.landmarks.file,
    )?;
    require!(
        before.id == after.id,
        "the file was {} before its reference was refreshed and {} after: a refresh moved \
         the item's identity",
        before.id,
        after.id
    );
    Ok(())
}

fn a_rate_limit_carries_its_backoff<H: SourceHarness>(
    harness: &H,
    staged: Staged<H::Source>,
) -> CaseResult {
    let request = whole_file(&staged)?;
    let (result, _) = fetch(harness, staged.source.as_ref(), request);

    let error = expect_err(result, "a fetch against a throttled source")?;
    let SourceError::RateLimited { retry_after, .. } = &error else {
        return Err(Failure::new(format!(
            "a throttled source must fail with RateLimited; got {error}"
        ))
        .into());
    };
    require!(
        *retry_after == Some(BACKOFF),
        "the source was throttled with a {BACKOFF:?} backoff but reported {retry_after:?}: a \
         flood wait's duration is the only part of it the caller can act on, and it must \
         cross the boundary intact"
    );
    Ok(())
}

fn lost_authorization_is_reported_as_such<H: SourceHarness>(
    harness: &H,
    staged: Staged<H::Source>,
) -> CaseResult {
    let error = expect_err(
        harness.block_on(staged.source.root()),
        "reading the root of a source with no authorization",
    )?;
    require!(
        matches!(error, SourceError::AuthRequired { .. }),
        "a source with no usable authorization must fail with AuthRequired — reported as \
         anything retryable, it sends the engine into a loop no retry can end; got {error}"
    );
    Ok(())
}
