//! Ranged delivery and the version it is pinned to (SYNC-046, SYNC-042).
//!
//! # The sink is the assertion
//!
//! Every case here delivers into a [`RecordingSink`], which folds each chunk
//! through `FetchProgress` as it arrives. That fold *is* the SYNC-046 check —
//! contiguous from the range's start, ending at its end, never past it — so a
//! source that gaps, overlaps, or overruns is caught at the offending chunk
//! rather than at a byte comparison that happens to come out even. The cases
//! then assert `violation() == None` as well as the bytes: a buffer that
//! reassembled correctly out of a delivery that broke the contract is still a
//! broken delivery, and only the fold can tell the difference.
//!
//! # A race is proved by what did *not* arrive
//!
//! [`losing_a_race_never_completes`] is the case the whole `VersionRace`
//! capability exists for. Its real assertion is not that the fetch failed —
//! it is that the bytes the sink took are a prefix of the *pinned* version
//! and the fetch never reported completion. A source that filled the tail of
//! the range from the new version and then failed would still have published
//! bytes of version B under a fetch for version A, which is the thing
//! SYNC-042 forbids.

use gramdrive_model::ByteRange;
use gramdrive_source::{DriveSource, FetchRequest, SourceError};

use crate::conformance::cases::Case;
use crate::conformance::harness::{Mutation, Perturbation, Setup, SourceHarness, Staged, WORLD};
use crate::conformance::report::{Clause, Failure};
use crate::conformance::support::{CaseResult, both, expect_err, fetch, require};
use crate::sink::RecordingSink;

/// Bytes the racing fetch is allowed to deliver before it loses.
///
/// Short enough to leave a tail undelivered against
/// [`WORLD`](crate::conformance::WORLD)'s file, which is what makes the
/// "never completes" half of the case observable.
const RACE_AFTER_BYTES: u64 = 8;

pub(crate) fn cases<H: SourceHarness>() -> Vec<Case<H>> {
    vec![
        Case {
            id: "fetch.a-full-range-delivers-exactly-the-content",
            clause: Clause::Sync041,
            claim: "fetching a file's whole extent delivers exactly its bytes, contiguously",
            needs: &[],
            setup: Setup::new,
            run: a_full_range_delivers_exactly_the_content::<H>,
        },
        Case {
            id: "fetch.a-partial-range-delivers-exactly-that-slice",
            clause: Clause::Sync041,
            claim: "fetching a sub-range delivers that slice and nothing either side of it",
            needs: &[],
            setup: Setup::new,
            run: a_partial_range_delivers_exactly_that_slice::<H>,
        },
        Case {
            id: "fetch.a-suffix-range-starts-at-its-own-offset",
            clause: Clause::Sync041,
            claim: "delivery of a range begins at the range's start, not at the file's",
            needs: &[],
            setup: Setup::new,
            run: a_suffix_range_starts_at_its_own_offset::<H>,
        },
        Case {
            id: "fetch.a-range-past-the-extent-is-an-invalid-request",
            clause: Clause::Sync041,
            claim: "a range the file cannot satisfy is refused, and no byte is delivered",
            needs: &[],
            setup: Setup::new,
            run: a_range_past_the_extent_is_an_invalid_request::<H>,
        },
        Case {
            id: "fetch.a-directory-is-an-invalid-request",
            clause: Clause::Sync041,
            claim: "fetching a directory fails with InvalidRequest",
            needs: &[],
            setup: Setup::new,
            run: a_directory_is_an_invalid_request::<H>,
        },
        Case {
            id: "fetch.an-absent-item-is-not-found",
            clause: Clause::Sync041,
            claim: "fetching an item that does not exist fails with NotFound",
            needs: &[],
            setup: Setup::new,
            run: an_absent_item_is_not_found::<H>,
        },
        Case {
            id: "fetch.a-stale-pin-conflicts-before-any-byte-moves",
            clause: Clause::Sync042,
            claim: "a fetch pinned to a version the source has left fails, delivering nothing",
            needs: &[],
            setup: || Setup::new().plan(Mutation::ContentChanges),
            run: a_stale_pin_conflicts_before_any_byte_moves::<H>,
        },
        Case {
            id: "fetch.losing-a-race-never-completes",
            clause: Clause::Sync042,
            claim: "a fetch overtaken mid-delivery conflicts, and never passes off the new \
                    version's bytes as the pinned one's",
            needs: &[],
            setup: || {
                Setup::new().arm(Perturbation::FetchRacesContentChange {
                    after_bytes: RACE_AFTER_BYTES,
                })
            },
            run: losing_a_race_never_completes::<H>,
        },
        Case {
            id: "fetch.concurrent-fetches-do-not-corrupt-each-other",
            clause: Clause::Sync046,
            claim: "two fetches of the same item and version, in flight at once, each receive \
                    their whole range intact",
            needs: &[],
            setup: Setup::new,
            run: concurrent_fetches_do_not_corrupt_each_other::<H>,
        },
    ]
}

/// A fetch of `range` from the world's file, pinned to its current version.
fn request<S>(staged: &Staged<S>, range: ByteRange) -> FetchRequest {
    FetchRequest {
        item: staged.landmarks.file.clone(),
        version: staged.landmarks.file_version.clone(),
        range,
    }
}

/// The whole extent of the world's file.
fn full_range() -> Result<ByteRange, Failure> {
    ByteRange::new(0, WORLD.file_bytes.len() as u64)
        .map_err(|error| Failure::new(format!("the world's file has no valid extent: {error}")))
}

fn a_full_range_delivers_exactly_the_content<H: SourceHarness>(
    harness: &H,
    staged: Staged<H::Source>,
) -> CaseResult {
    let range = full_range()?;
    let (result, sink) = fetch(
        harness,
        staged.source.as_ref(),
        FetchRequest {
            item: staged.landmarks.file.clone(),
            version: staged.landmarks.file_version.clone(),
            range,
        },
    );

    if let Err(error) = result {
        return Err(Failure::new(format!(
            "fetching the file's whole extent must succeed, but failed: {error}"
        ))
        .into());
    }
    require!(
        sink.violation().is_none(),
        "the source delivered the range out of contract: {:?}",
        sink.violation()
    );
    require!(
        sink.bytes() == WORLD.file_bytes,
        "the file holds {} bytes but the fetch delivered {}",
        WORLD.file_bytes.len(),
        sink.bytes().len()
    );
    require!(
        sink.is_complete(),
        "the fetch resolved successfully with {} of {} bytes delivered",
        sink.progress().delivered(),
        sink.progress().expected()
    );
    Ok(())
}

fn a_partial_range_delivers_exactly_that_slice<H: SourceHarness>(
    harness: &H,
    staged: Staged<H::Source>,
) -> CaseResult {
    let (start, end) = (4u64, 12u64);
    let Ok(range) = ByteRange::new(start, end) else {
        return Err(Failure::new("4..12 is a valid range").into());
    };
    let (result, sink) = fetch(harness, staged.source.as_ref(), request(&staged, range));

    if let Err(error) = result {
        return Err(Failure::new(format!(
            "fetching bytes {start}..{end} of the file must succeed, but failed: {error}"
        ))
        .into());
    }
    require!(
        sink.violation().is_none(),
        "the source delivered the range out of contract: {:?}",
        sink.violation()
    );

    let expected = &WORLD.file_bytes[start as usize..end as usize];
    require!(
        sink.bytes() == expected,
        "bytes {start}..{end} of the file are {expected:?}; the fetch delivered {:?}",
        sink.bytes()
    );
    Ok(())
}

fn a_suffix_range_starts_at_its_own_offset<H: SourceHarness>(
    harness: &H,
    staged: Staged<H::Source>,
) -> CaseResult {
    let extent = WORLD.file_bytes.len() as u64;
    let start = extent - 6;
    let Ok(range) = ByteRange::new(start, extent) else {
        return Err(Failure::new("the file's last six bytes are a valid range").into());
    };
    let (result, sink) = fetch(harness, staged.source.as_ref(), request(&staged, range));

    if let Err(error) = result {
        return Err(Failure::new(format!(
            "fetching the file's last six bytes must succeed, but failed: {error}"
        ))
        .into());
    }
    require!(
        sink.violation().is_none(),
        "the source delivered the range out of contract: {:?}",
        sink.violation()
    );

    let Some(first) = sink.chunks().first() else {
        return Err(Failure::new("a successful fetch of six bytes delivered no chunk").into());
    };
    require!(
        first.start() == start,
        "a fetch of bytes {start}.. delivered its first chunk at offset {}",
        first.start()
    );
    require!(
        sink.bytes() == &WORLD.file_bytes[start as usize..],
        "the file's last six bytes were not what arrived"
    );
    Ok(())
}

fn a_range_past_the_extent_is_an_invalid_request<H: SourceHarness>(
    harness: &H,
    staged: Staged<H::Source>,
) -> CaseResult {
    let extent = WORLD.file_bytes.len() as u64;
    let Ok(range) = ByteRange::new(extent - 2, extent + 64) else {
        return Err(Failure::new("a range running past the extent is constructible").into());
    };
    let (result, sink) = fetch(harness, staged.source.as_ref(), request(&staged, range));

    let error = expect_err(result, "fetching a range past the file's extent")?;
    require!(
        matches!(error, SourceError::InvalidRequest { .. }),
        "a range past the extent must fail with InvalidRequest; got {error}"
    );
    require!(
        sink.bytes().is_empty(),
        "the source refused the range but still delivered {} bytes into the sink",
        sink.bytes().len()
    );
    Ok(())
}

fn a_directory_is_an_invalid_request<H: SourceHarness>(
    harness: &H,
    staged: Staged<H::Source>,
) -> CaseResult {
    let Ok(range) = ByteRange::new(0, 4) else {
        return Err(Failure::new("0..4 is a valid range").into());
    };
    let (result, sink) = fetch(
        harness,
        staged.source.as_ref(),
        FetchRequest {
            item: staged.landmarks.listing.clone(),
            version: staged.landmarks.file_version.clone(),
            range,
        },
    );

    let error = expect_err(result, "fetching a directory")?;
    require!(
        matches!(error, SourceError::InvalidRequest { .. }),
        "fetching a directory must fail with InvalidRequest; got {error}"
    );
    require!(
        sink.bytes().is_empty(),
        "a directory has no bytes, but the source delivered {}",
        sink.bytes().len()
    );
    Ok(())
}

fn an_absent_item_is_not_found<H: SourceHarness>(
    harness: &H,
    staged: Staged<H::Source>,
) -> CaseResult {
    let Ok(range) = ByteRange::new(0, 4) else {
        return Err(Failure::new("0..4 is a valid range").into());
    };
    let (result, _) = fetch(
        harness,
        staged.source.as_ref(),
        FetchRequest {
            item: staged.landmarks.absent.clone(),
            version: staged.landmarks.file_version.clone(),
            range,
        },
    );

    let error = expect_err(result, "fetching an item that does not exist")?;
    require!(
        matches!(error, SourceError::NotFound { .. }),
        "fetching an absent item must fail with NotFound; got {error}"
    );
    Ok(())
}

fn a_stale_pin_conflicts_before_any_byte_moves<H: SourceHarness>(
    harness: &H,
    staged: Staged<H::Source>,
) -> CaseResult {
    let pinned = staged.landmarks.file_version.clone();

    // The content moves on; the caller still holds the version it observed.
    staged.control.advance()?;

    let range = full_range()?;
    let (result, sink) = fetch(
        harness,
        staged.source.as_ref(),
        FetchRequest {
            item: staged.landmarks.file.clone(),
            version: pinned.clone(),
            range,
        },
    );

    let error = expect_err(
        result,
        "fetching content pinned to a version the source has left",
    )?;
    let SourceError::VersionConflict { current, .. } = &error else {
        return Err(Failure::new(format!(
            "a fetch pinned to {pinned} after the content moved to {} must fail with \
             VersionConflict; got {error}",
            staged.landmarks.next_file_version
        ))
        .into());
    };
    if let Some(current) = current {
        require!(
            current == &staged.landmarks.next_file_version,
            "the source reports {current} as the file's current version; the world moved it \
             to {}",
            staged.landmarks.next_file_version
        );
    }
    require!(
        sink.bytes().is_empty(),
        "the pin was stale before the fetch began, but the source still delivered {} bytes",
        sink.bytes().len()
    );
    Ok(())
}

/// Sources are shared and callers issue concurrent operations; a source's
/// obligation is that concurrent calls never corrupt each other's answers
/// (SYNC-046). Two overlapping fetches of the same item and version is the
/// case where a source that keeps per-fetch state on itself — a running chunk
/// cursor, one shared buffer — hands one caller the other's bytes.
///
/// Overlapping, not identical: ranges that share a middle but differ at both
/// ends catch a source that serves the second caller the first's offsets.
fn concurrent_fetches_do_not_corrupt_each_other<H: SourceHarness>(
    harness: &H,
    staged: Staged<H::Source>,
) -> CaseResult {
    let extent = WORLD.file_bytes.len() as u64;
    let (Ok(left), Ok(right)) = (ByteRange::new(0, extent - 4), ByteRange::new(4, extent)) else {
        return Err(Failure::new("the file's overlapping halves are valid ranges").into());
    };

    let mut left_sink = RecordingSink::new(left);
    let mut right_sink = RecordingSink::new(right);
    let source = staged.source.as_ref();

    let (left_result, right_result) = harness.block_on(both(
        source.fetch(request(&staged, left), &mut left_sink),
        source.fetch(request(&staged, right), &mut right_sink),
    ));

    for (label, result, sink, range) in [
        ("the first", left_result, &left_sink, left),
        ("the second", right_result, &right_sink, right),
    ] {
        if let Err(error) = result {
            return Err(Failure::new(format!(
                "{label} of two concurrent fetches failed with {error}; each is a request the \
                 source answers on its own"
            ))
            .into());
        }
        require!(
            sink.violation().is_none(),
            "{label} of two concurrent fetches broke the delivery contract: {:?}",
            sink.violation()
        );
        let expected = &WORLD.file_bytes[range.start() as usize..range.end() as usize];
        require!(
            sink.bytes() == expected,
            "{label} of two concurrent fetches asked for bytes {}..{} and got {} bytes that \
             are not that range's content — the two calls corrupted each other",
            range.start(),
            range.end(),
            sink.bytes().len()
        );
    }
    Ok(())
}

fn losing_a_race_never_completes<H: SourceHarness>(
    harness: &H,
    staged: Staged<H::Source>,
) -> CaseResult {
    let range = full_range()?;
    let (result, sink) = fetch(
        harness,
        staged.source.as_ref(),
        FetchRequest {
            item: staged.landmarks.file.clone(),
            version: staged.landmarks.file_version.clone(),
            range,
        },
    );

    let error = expect_err(result, "a fetch overtaken by a content change mid-delivery")?;
    require!(
        matches!(error, SourceError::VersionConflict { .. }),
        "a fetch that loses a version race must fail with VersionConflict; got {error}"
    );
    require!(
        sink.violation().is_none(),
        "the source broke the delivery contract on its way to the conflict: {:?}",
        sink.violation()
    );
    require!(
        !sink.is_complete(),
        "a fetch that failed with a version conflict reported the whole range delivered; a \
         partial delivery must never look complete"
    );

    // The heart of SYNC-042: whatever arrived was version A's, not version B's.
    let delivered = sink.bytes();
    require!(
        WORLD.file_bytes.starts_with(delivered),
        "the {} bytes delivered under a pin on {} are not a prefix of that version's \
         content — the source published bytes of the version it moved to",
        delivered.len(),
        staged.landmarks.file_version
    );
    Ok(())
}
