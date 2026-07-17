//! What the suite found, and which clause it found it against.
//!
//! # A failure names a clause, not a call stack
//!
//! The suite's output is the deliverable, not a side effect of it. A backend
//! author who runs this suite is usually someone who has *not* read the whole
//! contract, and the report is where they meet it: every case carries the
//! [`Clause`] it pins and the claim it makes, so a failure reads as "SYNC-003
//! says enumeration is repeatable; it was not" rather than "assertion failed
//! at line 214".
//!
//! That is why cases return [`Failure`] instead of panicking. A panic would
//! stop at the first broken clause and describe it in the vocabulary of this
//! crate's internals; a report describes every broken clause in the
//! vocabulary of the contract, which is the only vocabulary a `tdjson` or
//! remote backend and this suite share.
//!
//! # Skipped is not passed
//!
//! [`CaseOutcome::Skipped`] exists because a backend that cannot stage a
//! version race has not proved it survives one. Counting that as a pass would
//! make the suite most flattering to the backends that support least. The
//! report keeps the two apart and prints the skips, so "conformant" always
//! comes with the list of what went untested.

use std::fmt;

use crate::conformance::harness::Capability;

/// A contract clause one conformance case pins.
///
/// Cases name a clause rather than a test file, so a failure is traceable to
/// `.spec/` without reading this crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Clause {
    /// SYNC-001 — uniform provider-neutral semantics.
    Sync001,
    /// SYNC-003 — paginated, repeatable, snapshot enumeration.
    Sync003,
    /// SYNC-004 — durable cursors; explicit account/schema mismatch.
    Sync004,
    /// SYNC-005 — bounded deadlines; long work is cancellable.
    Sync005,
    /// SYNC-022 — updates apply in source order behind a checkpoint.
    Sync022,
    /// SYNC-025 — source deletions are observed, and are not evictions.
    Sync025,
    /// SYNC-041 — fetch serves the requested byte range, however it chunks.
    Sync041,
    /// SYNC-042 — content pinned to a version; no A-bytes as B.
    Sync042,
    /// SYNC-043 — cancellation leaves state resumable or disposable.
    Sync043,
    /// SYNC-044 — the normalized failure taxonomy and its retry classes.
    Sync044,
    /// SYNC-045 — a reference refresh never moves an item's identity.
    Sync045,
    /// SYNC-046 — concurrent requests do not corrupt range accounting.
    Sync046,
    /// POL-4 — restricted content is refused through every door.
    Pol4,
}

impl Clause {
    /// The requirement ID as `.spec/` writes it.
    pub fn id(self) -> &'static str {
        match self {
            Self::Sync001 => "SYNC-001",
            Self::Sync003 => "SYNC-003",
            Self::Sync004 => "SYNC-004",
            Self::Sync005 => "SYNC-005",
            Self::Sync022 => "SYNC-022",
            Self::Sync025 => "SYNC-025",
            Self::Sync041 => "SYNC-041",
            Self::Sync042 => "SYNC-042",
            Self::Sync043 => "SYNC-043",
            Self::Sync044 => "SYNC-044",
            Self::Sync045 => "SYNC-045",
            Self::Sync046 => "SYNC-046",
            Self::Pol4 => "POL-4",
        }
    }

    /// What the clause requires, in the specification's own words.
    ///
    /// Verbatim from `.spec/`, not a paraphrase. It is printed with every
    /// failure, and a reader deciding whether their backend is really wrong
    /// has to be reading the requirement rather than this crate's summary of
    /// it — a summary is where a suite quietly acquires opinions the
    /// specification does not hold.
    pub fn statement(self) -> &'static str {
        match self {
            Self::Sync001 => {
                "Local and remote sources expose the same provider-neutral item, page, \
                 change, fetch, thumbnail, cancellation, and error semantics."
            }
            Self::Sync003 => {
                "Enumeration is paginated and repeatable for a declared snapshot/version; \
                 duplicate or missing children across pages are contract failures."
            }
            Self::Sync004 => {
                "Change cursors survive normal process restart and reject account/schema \
                 mismatches explicitly."
            }
            Self::Sync005 => {
                "Provider callbacks have bounded deadlines; long work is cancellable or \
                 converted into durable background/transfer state."
            }
            Self::Sync022 => {
                "Incremental updates apply in source order and persist a checkpoint \
                 transactionally with normalized state."
            }
            Self::Sync025 => {
                "Deletions observed after synchronization remove or tombstone current records \
                 according to the selected product policy; source deletion and cache eviction \
                 remain distinct."
            }
            Self::Sync041 => {
                "Fetch accepts byte ranges even if a source internally downloads larger aligned \
                 chunks."
            }
            Self::Sync042 => {
                "Partial data is stored under a transfer identity and promoted atomically only \
                 after version and integrity checks."
            }
            Self::Sync043 => {
                "Cancellation stops network and disk work promptly where supported and leaves \
                 resumable or safely disposable state."
            }
            Self::Sync044 => {
                "Retries classify flood wait, transient network, expired file reference, \
                 authorization, source deletion, unsupported/protected content, disk full, and \
                 integrity failure."
            }
            Self::Sync045 => "File-reference refresh never changes provider item identity.",
            Self::Sync046 => {
                "Concurrent requests for the same item/version coalesce where safe and do not \
                 corrupt range/accounting state."
            }
            Self::Pol4 => {
                "Media items appear as unavailable placeholders with an explicit \"restricted \
                 by Telegram\" state and are never fetched into the archive."
            }
        }
    }
}

impl fmt::Display for Clause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

/// A broken clause, described in contract terms.
///
/// The `detail` is written from what the *contract* exposes — pages, items,
/// errors, delivered bytes — and never from a backend's internals, so the
/// same message is meaningful whichever implementation produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    detail: String,
}

impl Failure {
    /// A failure described by `detail`.
    pub fn new(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
        }
    }

    /// What the source did, and what the clause required instead.
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.detail)
    }
}

/// The fixture could not be staged.
///
/// A harness fault, not a contract failure: the backend was never asked the
/// question. Reported apart from [`Failure`] so a broken fixture can never be
/// read as a broken source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessError {
    detail: String,
}

impl HarnessError {
    /// A staging failure described by `detail`.
    pub fn new(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
        }
    }

    /// Why the fixture could not be staged.
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl fmt::Display for HarnessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.detail)
    }
}

impl std::error::Error for HarnessError {}

/// How one case ended, from the case's own body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CaseError {
    /// The source broke the clause.
    Contract(Failure),
    /// The fixture never got far enough to ask.
    Harness(HarnessError),
}

impl From<Failure> for CaseError {
    fn from(failure: Failure) -> Self {
        Self::Contract(failure)
    }
}

impl From<HarnessError> for CaseError {
    fn from(error: HarnessError) -> Self {
        Self::Harness(error)
    }
}

/// What happened to one case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaseOutcome {
    /// The source honored the clause.
    Passed,
    /// The source broke it.
    Failed(Failure),
    /// The harness cannot stage what the case needs, so the clause went
    /// untested. Never a pass.
    Skipped {
        /// The capability the harness does not have.
        capability: Capability,
    },
    /// The harness failed to stage a fixture it claimed to support.
    HarnessFailed(HarnessError),
}

impl CaseOutcome {
    /// Whether this outcome is a contract failure.
    pub fn is_failure(&self) -> bool {
        matches!(self, Self::Failed(_) | Self::HarnessFailed(_))
    }
}

/// One case's identity, claim, and outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaseReport {
    /// Stable dotted identifier, e.g. `enumeration.is-repeatable`.
    pub id: &'static str,
    /// The clause the case pins.
    pub clause: Clause,
    /// What the case asserts, in one sentence.
    pub claim: &'static str,
    /// How it ended.
    pub outcome: CaseOutcome,
}

/// The suite's verdict on one backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    harness: String,
    cases: Vec<CaseReport>,
}

impl Report {
    pub(crate) fn new(harness: impl Into<String>, cases: Vec<CaseReport>) -> Self {
        Self {
            harness: harness.into(),
            cases,
        }
    }

    /// The backend this report is about.
    pub fn harness(&self) -> &str {
        &self.harness
    }

    /// Every case, in the order the suite ran them.
    pub fn cases(&self) -> &[CaseReport] {
        &self.cases
    }

    /// The cases that broke a clause.
    pub fn failures(&self) -> impl Iterator<Item = &CaseReport> {
        self.cases.iter().filter(|case| case.outcome.is_failure())
    }

    /// The cases the harness could not stage.
    pub fn skipped(&self) -> impl Iterator<Item = &CaseReport> {
        self.cases
            .iter()
            .filter(|case| matches!(case.outcome, CaseOutcome::Skipped { .. }))
    }

    /// How many cases passed.
    pub fn passed(&self) -> usize {
        self.cases
            .iter()
            .filter(|case| case.outcome == CaseOutcome::Passed)
            .count()
    }

    /// Whether the backend broke no clause it was asked about.
    ///
    /// Says nothing about the clauses it was never asked about — read
    /// [`skipped`](Self::skipped) for those.
    pub fn is_conformant(&self) -> bool {
        self.failures().next().is_none()
    }

    /// Every clause the suite exercised and the backend honored.
    pub fn clauses_upheld(&self) -> Vec<Clause> {
        let mut clauses = Vec::new();
        for case in &self.cases {
            if case.outcome == CaseOutcome::Passed && !clauses.contains(&case.clause) {
                clauses.push(case.clause);
            }
        }
        clauses
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let failed = self.failures().count();
        let skipped = self.skipped().count();
        writeln!(
            f,
            "DriveSource conformance — {}: {} passed, {failed} failed, {skipped} skipped",
            self.harness,
            self.passed(),
        )?;

        for case in self.failures() {
            writeln!(f, "\nFAILED {} [{}]", case.id, case.clause)?;
            writeln!(f, "  clause:   {}", case.clause.statement())?;
            writeln!(f, "  claim:    {}", case.claim)?;
            match &case.outcome {
                CaseOutcome::Failed(failure) => writeln!(f, "  observed: {failure}")?,
                CaseOutcome::HarnessFailed(error) => {
                    writeln!(f, "  harness:  could not stage the fixture: {error}")?;
                }
                CaseOutcome::Passed | CaseOutcome::Skipped { .. } => {}
            }
        }

        if skipped > 0 {
            writeln!(f, "\nSkipped — untested, not upheld:")?;
            for case in self.skipped() {
                if let CaseOutcome::Skipped { capability } = &case.outcome {
                    writeln!(
                        f,
                        "  {} [{}] — the harness cannot stage {capability}",
                        case.id, case.clause
                    )?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn case(id: &'static str, outcome: CaseOutcome) -> CaseReport {
        CaseReport {
            id,
            clause: Clause::Sync003,
            claim: "a claim",
            outcome,
        }
    }

    #[test]
    fn every_clause_states_its_id_and_requirement() {
        let clauses = [
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
        ];
        for clause in clauses {
            assert!(!clause.statement().is_empty(), "{clause} states nothing");
            assert_eq!(clause.to_string(), clause.id());
        }
        assert_eq!(Clause::Sync003.id(), "SYNC-003");
        assert_eq!(Clause::Pol4.id(), "POL-4");
    }

    #[test]
    fn a_report_with_no_failures_is_conformant() {
        let report = Report::new("fake", vec![case("a", CaseOutcome::Passed)]);
        assert!(report.is_conformant());
        assert_eq!(report.passed(), 1);
        assert_eq!(report.clauses_upheld(), vec![Clause::Sync003]);
    }

    #[test]
    fn a_skipped_case_is_neither_passed_nor_failed() {
        let report = Report::new(
            "partial",
            vec![case(
                "a",
                CaseOutcome::Skipped {
                    capability: Capability::VersionRace,
                },
            )],
        );
        assert!(report.is_conformant(), "a skip breaks no clause");
        assert_eq!(report.passed(), 0, "a skip is not a pass");
        assert_eq!(report.skipped().count(), 1);
        assert!(
            report.clauses_upheld().is_empty(),
            "a skipped clause is untested, not upheld"
        );
    }

    #[test]
    fn a_failed_case_is_not_conformant() {
        let report = Report::new(
            "broken",
            vec![case("a", CaseOutcome::Failed(Failure::new("saw a gap")))],
        );
        assert!(!report.is_conformant());
        assert_eq!(report.failures().count(), 1);
    }

    #[test]
    fn a_harness_failure_is_not_a_pass() {
        let report = Report::new(
            "broken-fixture",
            vec![case(
                "a",
                CaseOutcome::HarnessFailed(HarnessError::new("no account")),
            )],
        );
        assert!(!report.is_conformant());
        assert_eq!(report.passed(), 0);
    }

    #[test]
    fn the_rendered_report_names_the_clause_and_what_was_observed() {
        let report = Report::new(
            "fake",
            vec![
                case("enumeration.passes", CaseOutcome::Passed),
                case(
                    "enumeration.is-repeatable",
                    CaseOutcome::Failed(Failure::new("page 2 repeated child X")),
                ),
                case(
                    "fetch.races",
                    CaseOutcome::Skipped {
                        capability: Capability::VersionRace,
                    },
                ),
            ],
        );
        let text = report.to_string();
        assert!(text.contains("1 passed, 1 failed, 1 skipped"), "{text}");
        assert!(
            text.contains("FAILED enumeration.is-repeatable [SYNC-003]"),
            "{text}"
        );
        assert!(text.contains("page 2 repeated child X"), "{text}");
        assert!(
            text.contains("paginated and repeatable"),
            "the clause text travels with the failure: {text}"
        );
        assert!(
            text.contains("Skipped — untested, not upheld"),
            "skips are never silent: {text}"
        );
    }
}
