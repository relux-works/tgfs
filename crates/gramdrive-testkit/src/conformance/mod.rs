//! The one `DriveSource` conformance suite (SYNC-002, NFR-002;
//! TASK-260715-3e8q4m).
//!
//! Every implementation of the contract passes this suite or is not an
//! implementation of the contract. There is one of it, deliberately: the
//! point of DEC-003's provider-neutral boundary is that the engine can hold a
//! local TDLib source, a remote source, and a fake behind the same `dyn
//! DriveSource` and not care which — and that promise is worth exactly as
//! much as the shared test that checks it. A per-backend test suite would
//! certify each backend against its own habits.
//!
//! ```
//! use gramdrive_testkit::conformance::{self, FakeHarness};
//!
//! let report = conformance::run(&FakeHarness::new());
//! assert!(report.is_conformant(), "{report}");
//! ```
//!
//! # Running it against a backend of your own
//!
//! Implement [`SourceHarness`]: name the backend, say which
//! [`Capability`]s you can stage, drive a future, and build [`WORLD`] on
//! demand. Then call [`run`] — or [`assert_conforms`] from a `#[test]`, which
//! fails with the whole report rather than the first broken clause.
//!
//! The suite never constructs your source, never reaches past the trait, and
//! never learns your page-token format. Everything it knows about your world
//! it learns from the [`Landmarks`] you hand back.
//!
//! # What a run is worth
//!
//! Exactly the cases it was allowed to ask. A harness that supports no
//! [`Capability`] still gets a conformant [`Report`] — it will just be a
//! report whose skip list is longer than its pass list, and both numbers are
//! printed side by side for that reason. [`Report::is_conformant`] means "broke
//! nothing it was asked about", which is not the same as "correct", and the
//! suite does not pretend otherwise.
//!
//! # Boundaries
//!
//! This module is library code, so the workspace's `unwrap`/`panic` denials
//! apply in full: cases return [`Failure`] instead of asserting, which is
//! also what makes a report possible at all. [`assert_conforms`] is the one
//! place that panics, and it is the one place whose caller is a test.

mod cases;
mod fake;
mod harness;
mod report;
mod support;

pub use fake::FakeHarness;
pub use harness::{
    Capability, Control, Landmarks, Mutation, Perturbation, Setup, SourceHarness, Staged, WORLD,
    WorldSpec,
};
pub use report::{CaseOutcome, CaseReport, Clause, Failure, HarnessError, Report};

use report::CaseError;

/// Runs every case against `harness` and reports what it found.
///
/// Never panics and never stops early: a backend that breaks six clauses is
/// told about six clauses. Cases the harness cannot stage are reported
/// [`Skipped`](CaseOutcome::Skipped), never passed.
pub fn run<H: SourceHarness>(harness: &H) -> Report {
    let mut reports = Vec::new();

    for case in cases::all::<H>() {
        let outcome = run_case(harness, &case);
        reports.push(CaseReport {
            id: case.id,
            clause: case.clause,
            claim: case.claim,
            outcome,
        });
    }

    Report::new(harness.name(), reports)
}

fn run_case<H: SourceHarness>(harness: &H, case: &cases::Case<H>) -> CaseOutcome {
    if let Some(capability) = case
        .capabilities()
        .into_iter()
        .find(|capability| !harness.supports(*capability))
    {
        return CaseOutcome::Skipped { capability };
    }

    let staged = match harness.stage(&WORLD, &(case.setup)()) {
        Ok(staged) => staged,
        Err(error) => return CaseOutcome::HarnessFailed(error),
    };

    match (case.run)(harness, staged) {
        Ok(()) => CaseOutcome::Passed,
        Err(CaseError::Contract(failure)) => CaseOutcome::Failed(failure),
        Err(CaseError::Harness(error)) => CaseOutcome::HarnessFailed(error),
    }
}

/// Runs the suite and fails the test with the whole report if anything broke.
///
/// The `#[test]` entry point: `assert_conforms(&MyHarness::new())`.
///
/// # Panics
///
/// If any case broke a clause, or the harness failed to stage a fixture it
/// claimed to support. Skipped cases do not fail the run — read the printed
/// skip list, or assert on [`Report::skipped`] yourself if a backend is meant
/// to have grown a capability it is still declining.
// The workspace denies `panic` because a panic in the *core* is an aborted
// File Provider extension or a lost error category (NFR-030). None of that
// reasoning reaches here: this crate is a dev-dependency by architecture rule
// and never links into a product artifact, so this panic can only surface as
// a failing test — which is what it is for. The alternative, returning a
// `Result` for every caller to unwrap, buys nothing and costs every call site
// the report's formatting.
#[allow(clippy::panic)]
pub fn assert_conforms<H: SourceHarness>(harness: &H) -> Report {
    let report = run(harness);
    if !report.is_conformant() {
        panic!("{report}");
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_case_has_a_unique_id_and_a_claim() {
        let cases = cases::all::<FakeHarness>();
        assert!(
            cases.len() >= 25,
            "the suite lost cases: {} left",
            cases.len()
        );

        let mut seen: Vec<&str> = Vec::new();
        for case in &cases {
            assert!(
                !seen.contains(&case.id),
                "two cases share the id {}; a report keyed on it would be ambiguous",
                case.id
            );
            seen.push(case.id);
            assert!(!case.claim.is_empty(), "{} claims nothing", case.id);
            assert!(
                case.id.contains('.'),
                "{} is not a dotted identifier",
                case.id
            );
        }
    }

    #[test]
    fn the_suite_covers_every_clause_the_task_names() {
        let cases = cases::all::<FakeHarness>();
        // SYNC-001..005 are the story's acceptance surface; the rest are the
        // clauses those five delegate the detail to.
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
                cases.iter().any(|case| case.clause == clause),
                "no case pins {clause}"
            );
        }
    }

    #[test]
    fn a_case_needing_nothing_declares_no_capabilities() {
        let cases = cases::all::<FakeHarness>();
        let plain = cases
            .iter()
            .find(|case| case.id == "shape.the-root-is-a-parentless-directory")
            .expect("the root case is in the suite");
        assert!(
            plain.capabilities().is_empty(),
            "reading a root needs nothing staged"
        );
    }

    #[test]
    fn a_case_inherits_the_capabilities_its_setup_implies() {
        let cases = cases::all::<FakeHarness>();
        let racing = cases
            .iter()
            .find(|case| case.id == "fetch.losing-a-race-never-completes")
            .expect("the race case is in the suite");
        assert_eq!(
            racing.capabilities(),
            vec![Capability::VersionRace],
            "the runner must learn what a case needs without running it"
        );
    }
}
