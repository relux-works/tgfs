//! Change cursors and the feed behind them (SYNC-004, SYNC-022).
//!
//! # "Survives a restart" is testable without a restart
//!
//! SYNC-004 says cursors survive normal process restart. No conformance case
//! can restart a process, but it does not have to: what a restart does to a
//! cursor is serialize it, lose every other trace of the source, and hand the
//! text back later. [`survives_a_durable_round_trip`] does exactly that
//! through [`ChangeCursor::encode`]/[`decode`](ChangeCursor::decode) — the
//! same call the state store makes — and then asks the source to honor the
//! decoded value. A cursor that survives that survives a restart; one that
//! does not, cannot.
//!
//! # A mismatched scope is constructed, not staged
//!
//! The account/schema mismatch cases need no harness capability at all. A
//! cursor carries its [`AccountScope`], and the suite can mint one for
//! another account or another namespace epoch from the source's own scope —
//! no backend cooperation required. That is why these run against every
//! implementation, including ones that support nothing else.

use gramdrive_model::cursor::ChangeCursor;
use gramdrive_model::identity::{AccountId, AccountKey, AccountScope, NamespaceVersion};
use gramdrive_source::{DriveSource, SourceError};

use crate::conformance::cases::Case;
use crate::conformance::harness::{Mutation, Setup, SourceHarness, Staged};
use crate::conformance::report::{Clause, Failure};
use crate::conformance::support::{
    CaseResult, drain_changes, expect_err, expect_ok, removes, require, upserts,
};

pub(crate) fn cases<H: SourceHarness>() -> Vec<Case<H>> {
    vec![
        Case {
            id: "cursor.carries-the-sources-scope",
            clause: Clause::Sync004,
            claim: "a cursor the source mints is scoped to the account the source serves",
            needs: &[],
            setup: Setup::new,
            run: carries_the_sources_scope::<H>,
        },
        Case {
            id: "cursor.survives-a-durable-round-trip",
            clause: Clause::Sync004,
            claim: "a cursor serialized and restored — what a process restart does to it — is \
                    still served",
            needs: &[],
            setup: Setup::new,
            run: survives_a_durable_round_trip::<H>,
        },
        Case {
            id: "cursor.another-accounts-cursor-is-rejected",
            clause: Clause::Sync004,
            claim: "a cursor from another account is refused explicitly, never served",
            needs: &[],
            setup: Setup::new,
            run: another_accounts_cursor_is_rejected::<H>,
        },
        Case {
            id: "cursor.another-namespace-epochs-cursor-is-rejected",
            clause: Clause::Sync004,
            claim: "a cursor from another namespace epoch is refused explicitly, never served",
            needs: &[],
            setup: Setup::new,
            run: another_namespace_epochs_cursor_is_rejected::<H>,
        },
        Case {
            id: "feed.a-drained-feed-reports-nothing",
            clause: Clause::Sync022,
            claim: "a caller level with the source is told so, and its cursor does not rewind",
            needs: &[],
            setup: Setup::new,
            run: a_drained_feed_reports_nothing::<H>,
        },
        Case {
            id: "feed.reports-a-change-the-caller-has-not-seen",
            clause: Clause::Sync022,
            claim: "a change made after a cursor was taken is reported behind that cursor",
            needs: &[],
            setup: || Setup::new().plan(Mutation::ChildAppears),
            run: reports_a_change_the_caller_has_not_seen::<H>,
        },
        Case {
            id: "feed.an-applied-page-advances-past-its-changes",
            clause: Clause::Sync022,
            claim: "the cursor a page hands back does not replay that page's changes",
            needs: &[],
            setup: || Setup::new().plan(Mutation::ChildAppears),
            run: an_applied_page_advances_past_its_changes::<H>,
        },
        Case {
            id: "feed.reports-a-removal-as-an-explicit-event",
            clause: Clause::Sync025,
            claim: "an item deleted at the source is reported as a removal, not by quietly \
                    ceasing to be listed",
            needs: &[],
            setup: || Setup::new().plan(Mutation::ChildRemoved),
            run: reports_a_removal_as_an_explicit_event::<H>,
        },
    ]
}

/// A scope naming a different account than `scope`.
///
/// Derived from the source's own scope rather than from a fixture constant:
/// "foreign" has to mean foreign *to this source*, whichever account it
/// happens to serve.
fn another_account(scope: AccountScope) -> AccountScope {
    AccountScope {
        account: AccountKey {
            account_id: AccountId(scope.account.account_id.0.wrapping_add(1)),
        },
        namespace_version: scope.namespace_version,
    }
}

/// The same account under a different namespace epoch.
fn another_epoch(scope: AccountScope) -> AccountScope {
    AccountScope {
        account: scope.account,
        namespace_version: NamespaceVersion(scope.namespace_version.0.wrapping_add(1)),
    }
}

fn carries_the_sources_scope<H: SourceHarness>(
    harness: &H,
    staged: Staged<H::Source>,
) -> CaseResult {
    let cursor = expect_ok(
        harness.block_on(staged.source.latest_cursor()),
        "taking the source's latest cursor",
    )?;
    require!(
        cursor.scope() == staged.source.scope(),
        "the source serves scope {:?} but minted a cursor scoped to {:?}",
        staged.source.scope(),
        cursor.scope()
    );
    Ok(())
}

fn survives_a_durable_round_trip<H: SourceHarness>(
    harness: &H,
    staged: Staged<H::Source>,
) -> CaseResult {
    let cursor = expect_ok(
        harness.block_on(staged.source.latest_cursor()),
        "taking the source's latest cursor",
    )?;

    // Exactly what the state store does to a cursor across a restart.
    let encoded = cursor.encode();
    let restored = match ChangeCursor::decode(&encoded) {
        Ok(restored) => restored,
        Err(error) => {
            return Err(Failure::new(format!(
                "a cursor the source minted did not survive its own serialization: {error}"
            ))
            .into());
        }
    };
    require!(
        restored == cursor,
        "a cursor changed across a serialization round trip"
    );

    expect_ok(
        harness.block_on(staged.source.changes(restored)),
        "reading the feed from a cursor restored the way a restart restores it",
    )?;
    Ok(())
}

fn another_accounts_cursor_is_rejected<H: SourceHarness>(
    harness: &H,
    staged: Staged<H::Source>,
) -> CaseResult {
    let scope = another_account(staged.source.scope());
    let Ok(foreign) = ChangeCursor::new(scope, Vec::new()) else {
        return Err(Failure::new("an empty cursor payload is within every cap").into());
    };

    let error = expect_err(
        harness.block_on(staged.source.changes(foreign)),
        "reading the feed with another account's cursor",
    )?;
    require!(
        matches!(error, SourceError::CursorRejected { .. }),
        "a cursor from another account must be refused with CursorRejected; got {error}"
    );
    Ok(())
}

fn another_namespace_epochs_cursor_is_rejected<H: SourceHarness>(
    harness: &H,
    staged: Staged<H::Source>,
) -> CaseResult {
    let scope = another_epoch(staged.source.scope());
    let Ok(stale) = ChangeCursor::new(scope, Vec::new()) else {
        return Err(Failure::new("an empty cursor payload is within every cap").into());
    };

    let error = expect_err(
        harness.block_on(staged.source.changes(stale)),
        "reading the feed with a cursor from another namespace epoch",
    )?;
    require!(
        matches!(error, SourceError::CursorRejected { .. }),
        "a cursor from another namespace epoch must be refused with CursorRejected; got {error}"
    );
    Ok(())
}

fn a_drained_feed_reports_nothing<H: SourceHarness>(
    harness: &H,
    staged: Staged<H::Source>,
) -> CaseResult {
    let cursor = expect_ok(
        harness.block_on(staged.source.latest_cursor()),
        "taking the source's latest cursor",
    )?;
    let page = expect_ok(
        harness.block_on(staged.source.changes(cursor.clone())),
        "reading the feed from the latest cursor",
    )?;

    require!(
        page.changes.is_empty(),
        "the feed reported {} changes to a caller already level with the source",
        page.changes.len()
    );
    require!(
        !page.more_available,
        "the feed says more changes are available to a caller level with the source"
    );
    require!(
        page.next.scope() == staged.source.scope(),
        "a drained feed handed back a cursor scoped to {:?}, not the source's {:?}",
        page.next.scope(),
        staged.source.scope()
    );
    Ok(())
}

fn reports_a_change_the_caller_has_not_seen<H: SourceHarness>(
    harness: &H,
    staged: Staged<H::Source>,
) -> CaseResult {
    let cursor = expect_ok(
        harness.block_on(staged.source.latest_cursor()),
        "taking a cursor before the world changes",
    )?;

    staged.control.advance()?;

    let (changes, _) = drain_changes(harness, staged.source.as_ref(), cursor)?;
    require!(
        !changes.is_empty(),
        "a child appeared after the cursor was taken, but the feed reported no changes"
    );
    require!(
        upserts(&changes, &staged.landmarks.appearing_child),
        "the feed reported {} changes after a child appeared, but none of them upserts {}",
        changes.len(),
        staged.landmarks.appearing_child
    );
    Ok(())
}

/// SYNC-025 draws a line between a source deletion and a cache eviction, and
/// the drive can only act on the difference if the source states it. An item
/// that simply stops being listed is indistinguishable from one the source
/// forgot to mention — so the removal has to arrive as an event.
fn reports_a_removal_as_an_explicit_event<H: SourceHarness>(
    harness: &H,
    staged: Staged<H::Source>,
) -> CaseResult {
    let removed = staged.landmarks.removable_child.clone();
    let cursor = expect_ok(
        harness.block_on(staged.source.latest_cursor()),
        "taking a cursor before the removal",
    )?;

    staged.control.advance()?;

    let (changes, _) = drain_changes(harness, staged.source.as_ref(), cursor)?;
    require!(
        removes(&changes, &removed),
        "{removed} was deleted at the source, but the feed reported {} changes and none of \
         them removes it",
        changes.len()
    );
    require!(
        !upserts(&changes, &removed),
        "the feed both upserts and removes {removed} in one batch"
    );
    Ok(())
}

fn an_applied_page_advances_past_its_changes<H: SourceHarness>(
    harness: &H,
    staged: Staged<H::Source>,
) -> CaseResult {
    let cursor = expect_ok(
        harness.block_on(staged.source.latest_cursor()),
        "taking a cursor before the world changes",
    )?;

    staged.control.advance()?;

    let (changes, applied) = drain_changes(harness, staged.source.as_ref(), cursor)?;
    require!(
        !changes.is_empty(),
        "the feed reported nothing to advance past"
    );

    // The engine persists `applied` once the page is applied; from there the
    // same changes must not arrive again.
    let (replayed, _) = drain_changes(harness, staged.source.as_ref(), applied)?;
    require!(
        replayed.is_empty(),
        "the cursor handed back with a page replayed {} of its own changes when presented \
         again",
        replayed.len()
    );
    Ok(())
}
