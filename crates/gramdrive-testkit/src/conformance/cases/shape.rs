//! The shape of what a source serves (SYNC-001, POL-4).
//!
//! # Capabilities are only worth asserting where a backend can be wrong
//!
//! `SourceItem::capabilities` derives what a provider may advertise from the
//! item's own structure, and every branch of that derivation hardcodes the
//! writes to `false`. So "a directory advertises no write" is not something a
//! source can fail — no implementation can produce a writable capability set,
//! and a case asserting otherwise cannot fail for any input. It would be the
//! same tautology as asserting a `CursorRejected` advises `AfterRebaseline`:
//! a test of `gramdrive-source`'s arithmetic wearing a backend's name.
//!
//! (SYNC-060, the read-only clause, is not in this suite for a second reason:
//! it says those capabilities are not advertised *through native providers* —
//! an obligation of the File Provider and DocumentsProvider layers, which is
//! not this contract's to break.)
//!
//! What a backend does decide is the *structure* the derivation reads: it
//! chose to call this item a directory and that one a fetchable file. So
//! [`the_worlds_file_is_served_as_readable_content`] asserts the consequence
//! of that choice against what the world says the item is — a source that
//! served the world's file as a directory, or as restricted, advertises no
//! `read_content`, and that is a real bug with a real failure.
//!
//! # POL-4 is tested through both doors
//!
//! Restricted content is refused as content *and* as a thumbnail. Sources
//! tend to remember the first and forget the second, because a thumbnail
//! feels like metadata; it is not, and
//! [`restricted_content_is_refused_through_every_door`] knocks on both.

use std::num::NonZeroU32;

use gramdrive_model::ByteRange;
use gramdrive_model::version::ContentVersion;
use gramdrive_source::{
    DriveSource, FetchRequest, ItemContent, SourceError, SourceItem, ThumbnailSpec,
};

use crate::conformance::cases::Case;
use crate::conformance::harness::{Capability, Setup, SourceHarness, Staged};
use crate::conformance::report::{Clause, Failure, HarnessError};
use crate::conformance::support::{
    CaseResult, expect_err, expect_ok, find_item, page_request, require,
};
use crate::sink::RecordingSink;

pub(crate) fn cases<H: SourceHarness>() -> Vec<Case<H>> {
    vec![
        Case {
            id: "shape.the-root-is-a-parentless-directory",
            clause: Clause::Sync001,
            claim: "the account root is a directory and the only item with no parent",
            needs: &[],
            setup: Setup::new,
            run: the_root_is_a_parentless_directory::<H>,
        },
        Case {
            id: "shape.children-name-their-parent",
            clause: Clause::Sync001,
            claim: "an item served as a child of a directory names that directory as its parent",
            needs: &[],
            setup: Setup::new,
            run: children_name_their_parent::<H>,
        },
        Case {
            id: "capabilities.the-worlds-file-is-served-as-readable-content",
            clause: Clause::Sync001,
            claim: "a fetchable file is served as a file, and so advertises a content read and \
                    no enumeration",
            needs: &[],
            setup: Setup::new,
            run: the_worlds_file_is_served_as_readable_content::<H>,
        },
        Case {
            id: "restricted.is-refused-through-every-door",
            clause: Clause::Pol4,
            claim: "restricted content is refused as content and as a thumbnail, advertises no \
                    read, and stays visible",
            needs: &[Capability::RestrictedContent],
            setup: Setup::new,
            run: restricted_content_is_refused_through_every_door::<H>,
        },
    ]
}

/// A modest thumbnail box; every source is free to answer smaller.
fn spec() -> ThumbnailSpec {
    let side = NonZeroU32::new(256).unwrap_or(NonZeroU32::MIN);
    ThumbnailSpec {
        max_width_px: side,
        max_height_px: side,
    }
}

fn the_root_is_a_parentless_directory<H: SourceHarness>(
    harness: &H,
    staged: Staged<H::Source>,
) -> CaseResult {
    let root = expect_ok(harness.block_on(staged.source.root()), "reading the root")?;
    require!(
        root.parent.is_none(),
        "the account root names {:?} as its parent; the root is the one item with none",
        root.parent
    );
    require!(
        root.is_directory(),
        "the account root is not a directory, so nothing can be enumerated from it"
    );
    require!(
        root.id == staged.landmarks.root,
        "the source served {} as its root; the world's root is {}",
        root.id,
        staged.landmarks.root
    );
    Ok(())
}

fn children_name_their_parent<H: SourceHarness>(
    harness: &H,
    staged: Staged<H::Source>,
) -> CaseResult {
    let listing = &staged.landmarks.listing;
    let page = expect_ok(
        harness.block_on(staged.source.children(listing.clone(), page_request(32))),
        "enumerating the listing",
    )?;

    for item in &page.items {
        require!(
            item.parent.as_ref() == Some(listing),
            "{} was served as a child of {listing} but names {:?} as its parent",
            item.id,
            item.parent
        );
    }
    Ok(())
}

fn the_worlds_file_is_served_as_readable_content<H: SourceHarness>(
    harness: &H,
    staged: Staged<H::Source>,
) -> CaseResult {
    let file = find_item(
        harness,
        staged.source.as_ref(),
        &staged.landmarks.file_parent,
        &staged.landmarks.file,
    )?;
    let capabilities = file.capabilities();

    require!(
        !file.is_directory(),
        "the world's file was served as a directory"
    );
    require!(
        capabilities.read_content,
        "the world's file holds fetchable bytes, but the source serves it as an item whose \
         content cannot be read — a caller reading capabilities will never ask for them"
    );
    require!(
        !capabilities.enumerate_children,
        "the world's file advertises enumeration"
    );
    Ok(())
}

fn restricted_content_is_refused_through_every_door<H: SourceHarness>(
    harness: &H,
    staged: Staged<H::Source>,
) -> CaseResult {
    let Some(restricted) = staged.landmarks.restricted_file.clone() else {
        return Err(HarnessError::new(
            "the harness claims it can stage restricted content but staged none",
        )
        .into());
    };

    // POL-4: the item stays visible; only its bytes are withheld.
    let item = find_item(
        harness,
        staged.source.as_ref(),
        &staged.landmarks.file_parent,
        &restricted,
    )?;
    require!(
        !item.capabilities().read_content,
        "restricted content advertises a content read; the bytes are never served, and a \
         caller that believes otherwise will keep asking"
    );

    // Door one: the content.
    let Ok(range) = ByteRange::new(0, 1) else {
        return Err(Failure::new("0..1 is a valid range").into());
    };
    let mut sink = RecordingSink::new(range);
    let result = harness.block_on(staged.source.fetch(
        FetchRequest {
            item: restricted.clone(),
            version: item_version(&item)?,
            range,
        },
        &mut sink,
    ));
    let error = expect_err(result, "fetching restricted content")?;
    require!(
        matches!(error, SourceError::Restricted { .. }),
        "restricted content must be refused with Restricted; got {error}"
    );
    require!(
        sink.bytes().is_empty(),
        "restricted content was refused, but {} bytes still reached the sink",
        sink.bytes().len()
    );

    // Door two: the thumbnail. A thumbnail is a rendering of the bytes, so
    // it is the same refusal — not a separate, laxer question.
    let error = expect_err(
        harness.block_on(staged.source.thumbnail(restricted, spec())),
        "requesting a thumbnail of restricted content",
    )?;
    require!(
        matches!(error, SourceError::Restricted { .. }),
        "a thumbnail of restricted content must be refused with Restricted, not answered or \
         reported missing; got {error}"
    );
    Ok(())
}

/// The content version of a file item.
fn item_version(item: &SourceItem) -> Result<ContentVersion, Failure> {
    match &item.content {
        ItemContent::File(facts) => Ok(facts.content_version.clone()),
        ItemContent::Directory(_) => Err(Failure::new(format!(
            "{} was staged as restricted content but served as a directory",
            item.id
        ))),
    }
}

// There is deliberately no case for "a thumbnail of an item that does not
// exist". It is tempting — the fake answers `NotFound` — but the contract does
// not require it: `DriveSource::thumbnail` says `Ok(None)` means "this item has
// no thumbnail", "a normal answer, not an error", and mandates a failure only
// for restricted content. A remote source that answers `None` without spending
// a round trip proving the item exists honors the contract, and a case pinning
// `NotFound` would fail it for a rule the specification never states.
