//! Paged enumeration (SYNC-003).
//!
//! # Repeatable, not ordered a particular way
//!
//! SYNC-003 requires enumeration to be *repeatable*, and the contract calls
//! `ItemPage::items` "the source's stable enumeration order". Neither says
//! which order. So no case here asserts that a source lists children the way
//! [`Landmarks::listing_children`](crate::conformance::Landmarks::listing_children)
//! happens to hold them — that would test the harness's bookkeeping, not the
//! source. What they assert instead is that the source agrees with *itself*:
//! the same children, in the same order, across enumerations and across page
//! sizes.
//!
//! # A moving listing has two legal answers, and one illegal one
//!
//! When the world changes mid-enumeration a source may reject the
//! continuation (re-baseline) or keep serving the snapshot it started. Both
//! honor SYNC-003. What it may not do is splice — serve a page from the new
//! state onto pages from the old one, producing a listing with a duplicate or
//! a hole. [`does_not_splice_when_the_listing_moves`] accepts either legal
//! answer and only fails the third.

use gramdrive_source::{DriveSource, SourceError};

use crate::conformance::cases::Case;
use crate::conformance::harness::{Mutation, Setup, SourceHarness, Staged};
use crate::conformance::report::{Clause, Failure};
use crate::conformance::support::{
    CaseResult, continue_from, enumerate, enumerate_from, expect_err, expect_ok, first_duplicate,
    page_request, require, served_ids,
};

/// Items per page for the cases that page deliberately: small enough that
/// [`WORLD`](crate::conformance::WORLD)'s listing spans several pages.
const PAGE: u32 = 3;

pub(crate) fn cases<H: SourceHarness>() -> Vec<Case<H>> {
    vec![
        Case {
            id: "enumeration.covers-every-child-exactly-once",
            clause: Clause::Sync003,
            claim: "paging through a listing serves every child once — no duplicate, no hole",
            needs: &[],
            setup: Setup::new,
            run: covers_every_child_exactly_once::<H>,
        },
        Case {
            id: "enumeration.is-one-snapshot",
            clause: Clause::Sync003,
            claim: "every page of one enumeration reports the same snapshot",
            needs: &[],
            setup: Setup::new,
            run: is_one_snapshot::<H>,
        },
        Case {
            id: "enumeration.is-repeatable",
            clause: Clause::Sync003,
            claim: "enumerating an unchanged listing twice serves the same children in the \
                    same order",
            needs: &[],
            setup: Setup::new,
            run: is_repeatable::<H>,
        },
        Case {
            id: "enumeration.order-does-not-depend-on-page-size",
            clause: Clause::Sync003,
            claim: "the enumeration order is the source's, not the page size's",
            needs: &[],
            setup: Setup::new,
            run: order_does_not_depend_on_page_size::<H>,
        },
        Case {
            id: "enumeration.never-exceeds-the-requested-page-size",
            clause: Clause::Sync003,
            claim: "a page holds at most the items the caller asked for",
            needs: &[],
            setup: Setup::new,
            run: never_exceeds_the_requested_page_size::<H>,
        },
        Case {
            id: "enumeration.an-empty-directory-is-an-empty-page",
            clause: Clause::Sync003,
            claim: "a childless directory enumerates to one empty page, not an error",
            needs: &[],
            setup: Setup::new,
            run: an_empty_directory_is_an_empty_page::<H>,
        },
        Case {
            id: "enumeration.a-file-is-an-invalid-request",
            clause: Clause::Sync003,
            claim: "enumerating a file fails with InvalidRequest, which no retry fixes",
            needs: &[],
            setup: Setup::new,
            run: a_file_is_an_invalid_request::<H>,
        },
        Case {
            id: "enumeration.an-absent-item-is-not-found",
            clause: Clause::Sync003,
            claim: "enumerating an item that does not exist fails with NotFound",
            needs: &[],
            setup: Setup::new,
            run: an_absent_item_is_not_found::<H>,
        },
        Case {
            id: "enumeration.does-not-splice-when-the-listing-moves",
            clause: Clause::Sync003,
            claim: "a listing that changes mid-enumeration is re-baselined or kept whole, \
                    never spliced",
            needs: &[],
            setup: || Setup::new().plan(Mutation::ChildAppears),
            run: does_not_splice_when_the_listing_moves::<H>,
        },
    ]
}

fn covers_every_child_exactly_once<H: SourceHarness>(
    harness: &H,
    staged: Staged<H::Source>,
) -> CaseResult {
    let listing = &staged.landmarks.listing;
    let pages = enumerate(harness, staged.source.as_ref(), listing, PAGE)?;
    let served = served_ids(&pages);

    if let Some(duplicate) = first_duplicate(&served) {
        return Err(Failure::new(format!(
            "child {duplicate} was served twice across {} pages",
            pages.len()
        ))
        .into());
    }
    for expected in &staged.landmarks.listing_children {
        require!(
            served.contains(expected),
            "child {expected} is in the listing, but no page of the enumeration served it"
        );
    }
    require!(
        served.len() == staged.landmarks.listing_children.len(),
        "the listing holds {} children; the enumeration served {}",
        staged.landmarks.listing_children.len(),
        served.len()
    );
    Ok(())
}

fn is_one_snapshot<H: SourceHarness>(harness: &H, staged: Staged<H::Source>) -> CaseResult {
    let pages = enumerate(
        harness,
        staged.source.as_ref(),
        &staged.landmarks.listing,
        PAGE,
    )?;
    require!(
        pages.len() > 1,
        "the listing completed in one page at {PAGE} per page, so nothing about \
         cross-page consistency was tested"
    );
    let Some(first) = pages.first() else {
        return Err(Failure::new("the source served no pages at all").into());
    };
    for (index, page) in pages.iter().enumerate() {
        require!(
            page.snapshot == first.snapshot,
            "page 1 declared snapshot {} but page {} declared {}: one enumeration is one \
             snapshot",
            first.snapshot,
            index + 1,
            page.snapshot
        );
    }
    Ok(())
}

fn is_repeatable<H: SourceHarness>(harness: &H, staged: Staged<H::Source>) -> CaseResult {
    let listing = &staged.landmarks.listing;
    let first = enumerate(harness, staged.source.as_ref(), listing, PAGE)?;
    let second = enumerate(harness, staged.source.as_ref(), listing, PAGE)?;

    let (before, after) = (served_ids(&first), served_ids(&second));
    require!(
        before == after,
        "two enumerations of an unchanged listing disagree: first served {before:?}, \
         second served {after:?}"
    );
    Ok(())
}

fn order_does_not_depend_on_page_size<H: SourceHarness>(
    harness: &H,
    staged: Staged<H::Source>,
) -> CaseResult {
    let listing = &staged.landmarks.listing;
    let narrow = served_ids(&enumerate(harness, staged.source.as_ref(), listing, 2)?);
    let wide = served_ids(&enumerate(harness, staged.source.as_ref(), listing, 5)?);

    require!(
        narrow == wide,
        "the same listing enumerated 2-at-a-time and 5-at-a-time disagrees: {narrow:?} \
         versus {wide:?}"
    );
    Ok(())
}

fn never_exceeds_the_requested_page_size<H: SourceHarness>(
    harness: &H,
    staged: Staged<H::Source>,
) -> CaseResult {
    let pages = enumerate(
        harness,
        staged.source.as_ref(),
        &staged.landmarks.listing,
        PAGE,
    )?;
    for (index, page) in pages.iter().enumerate() {
        require!(
            page.items.len() <= PAGE as usize,
            "page {} holds {} items; the caller asked for at most {PAGE}",
            index + 1,
            page.items.len()
        );
    }
    Ok(())
}

// There is deliberately no "a page larger than the listing completes it in one
// page" case. Both halves of it are stricter than the contract: `PageRequest`
// says a source "may return fewer, never more", so a smaller page is legal at
// any requested size; and `next: None` is defined as *meaning* the enumeration
// is complete, never as an obligation to know that without another round trip.
// A source with an internal page cap, or one that hands back a token it then
// answers with an empty page, honors SYNC-003 and would have failed here. What
// the case was reaching for is covered without the over-reach:
// `never_exceeds_the_requested_page_size` pins the one bound the contract
// states, and `order_does_not_depend_on_page_size` enumerates the listing whole
// at two different sizes.

fn an_empty_directory_is_an_empty_page<H: SourceHarness>(
    harness: &H,
    staged: Staged<H::Source>,
) -> CaseResult {
    // No assertion on the page count: a source is free to hand back a
    // continuation it then answers with an empty page, rather than knowing it
    // was exhausted without another round trip. What is contractual is that
    // an empty directory enumerates at all — `NotFound` or `InvalidRequest`
    // here would be the source treating "nothing in it" as "not a directory".
    let pages = enumerate(
        harness,
        staged.source.as_ref(),
        &staged.landmarks.empty_directory,
        PAGE,
    )?;
    let served = served_ids(&pages);
    require!(
        served.is_empty(),
        "an empty directory served {} children: {served:?}",
        served.len()
    );
    Ok(())
}

fn a_file_is_an_invalid_request<H: SourceHarness>(
    harness: &H,
    staged: Staged<H::Source>,
) -> CaseResult {
    let error = expect_err(
        harness.block_on(
            staged
                .source
                .children(staged.landmarks.file.clone(), page_request(PAGE)),
        ),
        "enumerating a file",
    )?;
    require!(
        matches!(error, SourceError::InvalidRequest { .. }),
        "enumerating a file must fail with InvalidRequest; got {error}"
    );
    Ok(())
}

fn an_absent_item_is_not_found<H: SourceHarness>(
    harness: &H,
    staged: Staged<H::Source>,
) -> CaseResult {
    let error = expect_err(
        harness.block_on(
            staged
                .source
                .children(staged.landmarks.absent.clone(), page_request(PAGE)),
        ),
        "enumerating an item that does not exist",
    )?;
    require!(
        matches!(error, SourceError::NotFound { .. }),
        "enumerating an absent item must fail with NotFound; got {error}"
    );
    Ok(())
}

fn does_not_splice_when_the_listing_moves<H: SourceHarness>(
    harness: &H,
    staged: Staged<H::Source>,
) -> CaseResult {
    let listing = &staged.landmarks.listing;
    let source = staged.source.as_ref();

    let first = expect_ok(
        harness.block_on(source.children(listing.clone(), page_request(2))),
        "the first page of the enumeration",
    )?;
    let Some(token) = first.next.clone() else {
        return Err(Failure::new(
            "the listing completed in one page of 2, leaving no continuation to test",
        )
        .into());
    };

    // The world moves underneath the enumeration in flight.
    staged.control.advance()?;

    let continued = harness.block_on(source.children(listing.clone(), continue_from(token, 2)));

    let second = match continued {
        // Legal answer one: the snapshot is gone, so the anchor is refused
        // and the caller re-baselines.
        Err(SourceError::CursorRejected { .. }) => return Ok(()),
        Err(other) => {
            return Err(Failure::new(format!(
                "a continuation across a change must either be rejected with CursorRejected \
                 or keep serving the enumeration's snapshot; the source failed with {other}"
            ))
            .into());
        }
        // Legal answer two: the source keeps serving the snapshot it started.
        Ok(page) => page,
    };

    require!(
        second.snapshot == first.snapshot,
        "the source served a continuation under snapshot {} against an enumeration that \
         began at snapshot {}: that splices two states into one listing",
        second.snapshot,
        first.snapshot
    );

    // Finish the enumeration and hold the whole of it to SYNC-003.
    let mut pages = vec![first, second];
    if let Some(token) = pages.last().and_then(|page| page.next.clone()) {
        pages.extend(enumerate_from(
            harness,
            source,
            listing,
            continue_from(token, 2),
        )?);
    }

    let served = served_ids(&pages);
    if let Some(duplicate) = first_duplicate(&served) {
        return Err(Failure::new(format!(
            "an enumeration that ran across a change served child {duplicate} twice"
        ))
        .into());
    }
    require!(
        !served.contains(&staged.landmarks.appearing_child),
        "the enumeration kept serving its original snapshot but leaked {} into it — the \
         child appeared only after the enumeration began",
        staged.landmarks.appearing_child
    );
    for expected in &staged.landmarks.listing_children {
        require!(
            served.contains(expected),
            "an enumeration that ran across a change dropped child {expected}, which was in \
             the listing before the change and after it"
        );
    }
    Ok(())
}
