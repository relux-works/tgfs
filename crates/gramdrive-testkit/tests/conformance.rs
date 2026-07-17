//! The conformance suite, run against the deterministic fake — and against
//! sources built to break it (TASK-260715-3e8q4m).
//!
//! An integration test rather than a `#[cfg(test)]` module for the same
//! reason as `fake_source.rs`: it links the crate the way
//! `gramdrive-source-tdjson` will, through the public API and nothing else.
//! If the suite cannot be run from outside the crate that defines it, it is
//! not the shared suite SYNC-002 asks for.
//!
//! # Two halves, and the second is the important one
//!
//! The first half is the obvious one: the fake passes, and skips nothing.
//! Alone it proves very little — a suite whose every case asserted `true`
//! would pass exactly as loudly. So the second half runs the suite against
//! [`Saboteur`], a source that wraps the fake and breaks one clause on
//! purpose, and asserts the suite *fails*, on the case that owns that clause.
//! That is what says the cases have teeth, and it is the only test here that
//! would catch a case that stopped asserting anything.

// The workspace denies `expect_used`/`panic` because a panic in the core is
// an aborted File Provider extension or a lost error category (NFR-030), and
// clippy.toml exempts test code on the grounds that a panicking test is just
// a failing test. That exemption keys on `#[test]` functions and
// `#[cfg(test)]` modules, and the saboteur below is neither: it sits at
// module level in an integration-test binary. The rationale still applies in
// full — this file is test code and links into no product artifact — so the
// exemption is restated here rather than worked around by threading `Result`
// through helpers whose only failure mode is a typo in a literal.
#![allow(clippy::expect_used, clippy::panic)]

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use gramdrive_testkit::conformance::{
    self, CaseOutcome, Clause, FakeHarness, HarnessError, Setup, SourceHarness, Staged, WorldSpec,
};
use gramdrive_testkit::exec;
use gramdrive_testkit::model::identity::{AccountScope, ItemId};
use gramdrive_testkit::model::version::MetadataVersion;
use gramdrive_testkit::source::{
    ChangePage, ContentChunk, ContentSink, DriveSource, FetchRequest, ItemPage, PageRequest,
    SourceError, SourceFuture, SourceItem, Thumbnail, ThumbnailSpec,
};
use gramdrive_testkit::{FakeSource, model::cursor::ChangeCursor};

// --- The fake passes its own suite -----------------------------------------

#[test]
fn the_deterministic_fake_conforms() {
    let report = conformance::assert_conforms(&FakeHarness::new());
    assert!(report.is_conformant(), "{report}");
}

#[test]
fn the_fake_skips_nothing_so_every_clause_is_actually_exercised() {
    let report = conformance::run(&FakeHarness::new());

    let skipped: Vec<&str> = report.skipped().map(|case| case.id).collect();
    assert!(
        skipped.is_empty(),
        "the fake exists so that no clause goes unexercised, but the suite skipped {skipped:?}"
    );
    assert_eq!(
        report.passed(),
        report.cases().len(),
        "every case must have run and passed: {report}"
    );
}

#[test]
fn a_conformant_run_upholds_every_clause_the_suite_knows() {
    let report = conformance::run(&FakeHarness::new());
    let upheld = report.clauses_upheld();

    for clause in [
        Clause::Sync001,
        Clause::Sync003,
        Clause::Sync004,
        Clause::Sync005,
        Clause::Sync022,
        Clause::Sync025,
        Clause::Sync042,
        Clause::Sync043,
        Clause::Sync044,
        Clause::Sync041,
        Clause::Sync045,
        Clause::Sync046,
        Clause::Pol4,
    ] {
        assert!(
            upheld.contains(&clause),
            "the fake's run upheld {upheld:?}, which does not include {clause}"
        );
    }
}

#[test]
fn the_report_is_readable_without_knowing_the_backend() {
    let report = conformance::run(&FakeHarness::new());
    let text = report.to_string();

    assert!(text.contains("gramdrive-testkit fake"), "{text}");
    assert!(text.contains("passed"), "{text}");
    assert!(
        !text.contains("revision") && !text.contains("script"),
        "a report must be written in the contract's vocabulary, not a backend's: {text}"
    );
}

// --- Sources built to break one clause each --------------------------------

/// One way a source can be wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sabotage {
    /// Repeats a child on every page — the duplicate SYNC-003 forbids.
    DuplicatesAChild,
    /// Reports a fresh snapshot on every page, splicing states together.
    ShiftsTheSnapshot,
    /// Serves any cursor at all, including another account's.
    AcceptsAnyCursor,
    /// Delivers one byte past the range it was asked for.
    OverrunsTheRange,
    /// Delivers the right offsets with the wrong bytes — the shape of two
    /// concurrent fetches served out of one shared buffer.
    DeliversTheWrongBytes,
    /// Reports every failure as unreachable, whatever it was.
    MiscategorizesFailures,
}

/// The fake, wrapped in one specific lie.
struct Saboteur {
    inner: Arc<FakeSource>,
    sabotage: Sabotage,
    pages: AtomicU32,
}

impl DriveSource for Saboteur {
    fn scope(&self) -> AccountScope {
        self.inner.scope()
    }

    fn root(&self) -> SourceFuture<'_, SourceItem> {
        match self.sabotage {
            Sabotage::MiscategorizesFailures => Box::pin(async move {
                self.inner
                    .root()
                    .await
                    .map_err(|_| SourceError::Unavailable {
                        detail: "everything is a network problem if you squint".to_owned(),
                    })
            }),
            _ => self.inner.root(),
        }
    }

    fn children(&self, parent: ItemId, request: PageRequest) -> SourceFuture<'_, ItemPage> {
        Box::pin(async move {
            let mut page = self.inner.children(parent, request).await?;
            match self.sabotage {
                Sabotage::DuplicatesAChild => {
                    if let Some(first) = page.items.first().cloned() {
                        page.items.push(first);
                    }
                }
                Sabotage::ShiftsTheSnapshot => {
                    let nth = self.pages.fetch_add(1, Ordering::AcqRel);
                    page.snapshot =
                        MetadataVersion::new(format!("drift-{nth}")).expect("non-empty token");
                }
                _ => {}
            }
            Ok(page)
        })
    }

    fn latest_cursor(&self) -> SourceFuture<'_, ChangeCursor> {
        self.inner.latest_cursor()
    }

    fn changes(&self, cursor: ChangeCursor) -> SourceFuture<'_, ChangePage> {
        match self.sabotage {
            // No scope check at all: whatever you hand it, it answers.
            Sabotage::AcceptsAnyCursor => Box::pin(async move {
                Ok(ChangePage {
                    changes: Vec::new(),
                    next: cursor,
                    more_available: false,
                })
            }),
            _ => self.inner.changes(cursor),
        }
    }

    fn fetch<'a>(
        &'a self,
        request: FetchRequest,
        sink: &'a mut dyn ContentSink,
    ) -> SourceFuture<'a, ()> {
        match self.sabotage {
            Sabotage::OverrunsTheRange => Box::pin(async move {
                let over = vec![0xabu8; request.range.len() as usize + 1];
                let chunk =
                    ContentChunk::new(request.range.start(), &over).expect("a non-empty chunk");
                let _ = sink.accept(chunk);
                Ok(())
            }),
            // Right offsets, wrong bytes: the delivery contract is honored to
            // the letter and the content is still garbage. This is the shape a
            // concurrency bug takes — two fetches served out of one buffer,
            // each delivering at its own offsets whatever the other left there
            // — and it is invisible to `FetchProgress`, which accounts offsets
            // and lengths and never looks at a byte.
            Sabotage::DeliversTheWrongBytes => Box::pin(async move {
                let wrong = vec![0xcdu8; request.range.len() as usize];
                let chunk =
                    ContentChunk::new(request.range.start(), &wrong).expect("a non-empty chunk");
                let _ = sink.accept(chunk);
                Ok(())
            }),
            Sabotage::MiscategorizesFailures => Box::pin(async move {
                self.inner
                    .fetch(request, sink)
                    .await
                    .map_err(|_| SourceError::Unavailable {
                        detail: "everything is a network problem if you squint".to_owned(),
                    })
            }),
            _ => self.inner.fetch(request, sink),
        }
    }

    fn thumbnail(&self, item: ItemId, spec: ThumbnailSpec) -> SourceFuture<'_, Option<Thumbnail>> {
        self.inner.thumbnail(item, spec)
    }
}

/// Stages the fake's world, then wraps the source in a lie.
struct SaboteurHarness {
    inner: FakeHarness,
    sabotage: Sabotage,
}

impl SaboteurHarness {
    fn new(sabotage: Sabotage) -> Self {
        Self {
            inner: FakeHarness::new(),
            sabotage,
        }
    }
}

impl SourceHarness for SaboteurHarness {
    type Source = Saboteur;

    fn name(&self) -> &str {
        "saboteur"
    }

    fn supports(&self, capability: conformance::Capability) -> bool {
        self.inner.supports(capability)
    }

    fn block_on<T>(&self, future: impl Future<Output = T>) -> T {
        exec::drive(future)
    }

    fn stage(
        &self,
        world: &WorldSpec,
        setup: &Setup,
    ) -> Result<Staged<Self::Source>, HarnessError> {
        let staged = self.inner.stage(world, setup)?;
        Ok(Staged {
            source: Arc::new(Saboteur {
                inner: staged.source,
                sabotage: self.sabotage,
                pages: AtomicU32::new(0),
            }),
            landmarks: staged.landmarks,
            control: staged.control,
        })
    }
}

/// Every case a saboteur breaks, as `(case id, clause)`.
fn failures_of(sabotage: Sabotage) -> Vec<(&'static str, Clause)> {
    let report = conformance::run(&SaboteurHarness::new(sabotage));
    assert!(
        !report.is_conformant(),
        "a source that {sabotage:?} passed the whole suite — the cases that should have \
         caught it are asserting nothing"
    );
    assert!(
        report.skipped().count() == 0,
        "the saboteur harness stages everything the fake does"
    );
    report
        .failures()
        .map(|case| (case.id, case.clause))
        .collect()
}

/// Asserts `sabotage` is caught, by `case`, against `clause`.
fn assert_caught(sabotage: Sabotage, case: &str, clause: Clause) {
    let failures = failures_of(sabotage);
    assert!(
        failures
            .iter()
            .any(|(id, broken)| *id == case && *broken == clause),
        "a source that {sabotage:?} should fail {case} against {clause}; the suite reported \
         {failures:?}"
    );
}

#[test]
fn the_suite_catches_a_duplicated_child() {
    assert_caught(
        Sabotage::DuplicatesAChild,
        "enumeration.covers-every-child-exactly-once",
        Clause::Sync003,
    );
}

#[test]
fn the_suite_catches_a_snapshot_that_shifts_between_pages() {
    assert_caught(
        Sabotage::ShiftsTheSnapshot,
        "enumeration.is-one-snapshot",
        Clause::Sync003,
    );
}

#[test]
fn the_suite_catches_a_source_that_serves_another_accounts_cursor() {
    assert_caught(
        Sabotage::AcceptsAnyCursor,
        "cursor.another-accounts-cursor-is-rejected",
        Clause::Sync004,
    );
}

#[test]
fn the_suite_catches_a_source_that_serves_another_namespace_epochs_cursor() {
    assert_caught(
        Sabotage::AcceptsAnyCursor,
        "cursor.another-namespace-epochs-cursor-is-rejected",
        Clause::Sync004,
    );
}

#[test]
fn the_suite_catches_a_delivery_that_runs_past_its_range() {
    assert_caught(
        Sabotage::OverrunsTheRange,
        "fetch.a-full-range-delivers-exactly-the-content",
        Clause::Sync041,
    );
}

#[test]
fn the_suite_catches_a_source_that_reports_the_wrong_failure_category() {
    // Authorization reported as a network fault is the expensive one: the
    // engine retries forever against a source no retry can fix.
    assert_caught(
        Sabotage::MiscategorizesFailures,
        "failure.lost-authorization-is-reported-as-such",
        Clause::Sync044,
    );
}

#[test]
fn the_suite_catches_an_expired_reference_reported_as_a_network_fault() {
    // The subtler half of the same lie, and the likelier one: a stale file
    // reference needs a refresh, not a wait, so reporting it as transient
    // sends the engine to sleep instead of to the fix.
    assert_caught(
        Sabotage::MiscategorizesFailures,
        "failure.an-expired-reference-is-refreshable",
        Clause::Sync044,
    );
}

#[test]
fn the_suite_catches_a_delivery_of_the_wrong_bytes() {
    assert_caught(
        Sabotage::DeliversTheWrongBytes,
        "fetch.a-full-range-delivers-exactly-the-content",
        Clause::Sync041,
    );
}

#[test]
fn the_suite_catches_concurrent_fetches_serving_each_other_garbage() {
    // The assertion that catches this is the byte comparison, not the
    // delivery fold: these chunks arrive at exactly the right offsets.
    assert_caught(
        Sabotage::DeliversTheWrongBytes,
        "fetch.concurrent-fetches-do-not-corrupt-each-other",
        Clause::Sync046,
    );
}

#[test]
fn a_failure_report_names_the_clause_and_what_was_seen() {
    let report = conformance::run(&SaboteurHarness::new(Sabotage::DuplicatesAChild));
    let text = report.to_string();

    assert!(text.contains("[SYNC-003]"), "{text}");
    assert!(
        text.contains("paginated and repeatable"),
        "the clause's own words travel with the failure: {text}"
    );
    assert!(
        text.contains("was served twice"),
        "the report says what the source did: {text}"
    );
}

// --- A harness that declines is reported, not flattered ---------------------

/// A harness that stages the world and nothing else — the shape of a backend
/// running against a live account.
struct AusteHarness(FakeHarness);

impl SourceHarness for AusteHarness {
    type Source = FakeSource;

    fn name(&self) -> &str {
        "austere"
    }

    fn supports(&self, _capability: conformance::Capability) -> bool {
        false
    }

    fn block_on<T>(&self, future: impl Future<Output = T>) -> T {
        exec::drive(future)
    }

    fn stage(
        &self,
        world: &WorldSpec,
        setup: &Setup,
    ) -> Result<Staged<Self::Source>, HarnessError> {
        self.0.stage(world, setup)
    }
}

#[test]
fn a_harness_that_stages_nothing_gets_skips_not_passes() {
    let report = conformance::run(&AusteHarness(FakeHarness::new()));

    assert!(
        report.skipped().count() > 0,
        "a harness that supports nothing must have cases skipped"
    );
    assert!(
        report.is_conformant(),
        "a skip breaks no clause, so the run is still conformant: {report}"
    );
    for case in report.skipped() {
        assert!(
            matches!(case.outcome, CaseOutcome::Skipped { .. }),
            "{} was not skipped",
            case.id
        );
    }
    // The point: the clauses it never got asked about are not credited.
    let upheld = report.clauses_upheld();
    assert!(
        !upheld.contains(&Clause::Sync042),
        "a harness that cannot stage a version race must not be credited with SYNC-042"
    );
    assert!(
        report
            .to_string()
            .contains("Skipped — untested, not upheld"),
        "the skip list is printed, so 'conformant' is never read as 'complete'"
    );
}
