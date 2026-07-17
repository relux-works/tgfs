//! Cancellation, by both of its paths (SYNC-005, SYNC-043).
//!
//! # Two paths, and only one of them needs a capability
//!
//! The in-band path — the sink answering [`SinkControl::Stop`] — needs
//! nothing from the harness: any source that delivers a chunk can be told to
//! stop on it, so [`a_sink_that_stops_cancels_the_fetch`] runs against every
//! backend. The out-of-band path needs the call to still be *running* when
//! the caller lets go of it, which needs a source slow enough to catch in the
//! act; that is [`Capability::Latency`], and without it the drop cases are
//! skipped rather than faked.
//!
//! # What a dropped future can be asked, and what it cannot
//!
//! It is tempting to assert that a cancelled fetch delivered fewer bytes than
//! it was asked for. It is also wrong: a source is free to deliver a small
//! range in one chunk, in which case the first poll finishes the job and
//! there is nothing left to cancel. Nothing about that is a contract failure,
//! so no case here counts bytes.
//!
//! What SYNC-005 and SYNC-043 actually promise is that cancellation is
//! *survivable* — the caller's state is left "resumable or safely
//! disposable", and the source keeps working. So that is what the drop cases
//! assert: the call was abandoned mid-flight, no byte arrived afterwards, and
//! the very next request against the same source is answered in full. A
//! source that wedged, leaked its cancelled call's state into the next one,
//! or kept writing into an abandoned sink fails on the second half.

use gramdrive_model::ByteRange;
use gramdrive_source::{DriveSource, FetchRequest, SourceError};

use crate::conformance::cases::Case;
use crate::conformance::harness::{Perturbation, Setup, SourceHarness, Staged, WORLD};
use crate::conformance::report::{Clause, Failure};
use crate::conformance::support::{
    CaseResult, after_polls, expect_err, expect_ok, fetch, require, until_abandoned,
};
use crate::fault::Operation;
use crate::sink::RecordingSink;

pub(crate) fn cases<H: SourceHarness>() -> Vec<Case<H>> {
    vec![
        Case {
            id: "cancellation.a-sink-that-stops-cancels-the-fetch",
            clause: Clause::Sync043,
            claim: "a sink answering Stop ends the fetch as Cancelled, not as success",
            needs: &[],
            setup: Setup::new,
            run: a_sink_that_stops_cancels_the_fetch::<H>,
        },
        Case {
            id: "cancellation.an-abandoned-call-leaves-the-source-usable",
            clause: Clause::Sync005,
            claim: "a call dropped while it is still running does not take the source with it",
            needs: &[],
            setup: || {
                Setup::new().arm(Perturbation::Slow {
                    operation: Operation::Root,
                })
            },
            run: an_abandoned_call_leaves_the_source_usable::<H>,
        },
        Case {
            id: "cancellation.an-abandoned-fetch-stops-delivering-and-can-be-refetched",
            clause: Clause::Sync043,
            claim: "a dropped fetch delivers nothing more into its sink, and the same content \
                    fetches whole afterwards",
            needs: &[],
            setup: || {
                Setup::new().arm(Perturbation::Slow {
                    operation: Operation::Fetch,
                })
            },
            run: an_abandoned_fetch_stops_delivering_and_can_be_refetched::<H>,
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

fn a_sink_that_stops_cancels_the_fetch<H: SourceHarness>(
    harness: &H,
    staged: Staged<H::Source>,
) -> CaseResult {
    let request = whole_file(&staged)?;
    let range = request.range;

    // Stop on the very first chunk: whatever a source's chunking, it meets
    // this on the first thing it hands over.
    let mut sink = RecordingSink::stopping_after(range, 0);
    let result = harness.block_on(staged.source.fetch(request, &mut sink));

    let error = expect_err(result, "a fetch whose sink asked it to stop")?;
    require!(
        matches!(error, SourceError::Cancelled { .. }),
        "a sink that answers Stop must end the fetch as Cancelled; got {error}"
    );
    require!(
        sink.violation().is_none(),
        "the source broke the delivery contract before it was stopped: {:?}",
        sink.violation()
    );
    Ok(())
}

fn an_abandoned_call_leaves_the_source_usable<H: SourceHarness>(
    harness: &H,
    staged: Staged<H::Source>,
) -> CaseResult {
    // One poll in, the call is still running: let go of it there.
    let abandoned = harness.block_on(until_abandoned(staged.source.root(), after_polls(1)));
    require!(
        abandoned.is_none(),
        "the source was staged to answer root() slowly, but it answered before the caller \
         could let go of the call; nothing about cancellation was tested"
    );

    // SYNC-005: the cancelled call is disposed of, and the source carries on.
    let root = expect_ok(
        harness.block_on(staged.source.root()),
        "reading the root after a previous root() call was abandoned",
    )?;
    require!(
        root.id == staged.landmarks.root,
        "after an abandoned call the source answered root() with {}, not the account root {}",
        root.id,
        staged.landmarks.root
    );
    Ok(())
}

fn an_abandoned_fetch_stops_delivering_and_can_be_refetched<H: SourceHarness>(
    harness: &H,
    staged: Staged<H::Source>,
) -> CaseResult {
    let request = whole_file(&staged)?;
    let range = request.range;

    let mut abandoned_sink = RecordingSink::new(range);
    let abandoned = harness.block_on(until_abandoned(
        staged.source.fetch(request.clone(), &mut abandoned_sink),
        after_polls(1),
    ));
    require!(
        abandoned.is_none(),
        "the source was staged to fetch slowly, but the fetch finished before the caller \
         could let go of it; nothing about cancellation was tested"
    );

    // Whatever it managed to deliver before being dropped, it delivered
    // legally — the sink is readable again now the fetch that borrowed it is
    // gone. Note what is *not* asserted: that it delivered less than the whole
    // range. A source may hand a small range over in one chunk and suspend
    // before resolving, in which case the range arrived and the call was still
    // cancelled. Counting bytes here would fail that source for nothing.
    require!(
        abandoned_sink.violation().is_none(),
        "the source broke the delivery contract before it was dropped: {:?}",
        abandoned_sink.violation()
    );

    // SYNC-043: partial state is disposable, and the content is still whole.
    let (result, sink) = fetch(harness, staged.source.as_ref(), request);
    if let Err(error) = result {
        return Err(Failure::new(format!(
            "after an abandoned fetch, re-fetching the same content failed with {error}"
        ))
        .into());
    }
    require!(
        sink.violation().is_none(),
        "the re-fetch broke the range contract: {:?}",
        sink.violation()
    );
    require!(
        sink.bytes() == WORLD.file_bytes,
        "a fetch following an abandoned one delivered {} of the file's {} bytes",
        sink.bytes().len(),
        WORLD.file_bytes.len()
    );
    Ok(())
}
