//! The cases, and what one is.
//!
//! A [`Case`] is data, not a `#[test]`: an id, the clause it pins, the claim
//! it makes, what it needs staged, and the body. That shape is what lets one
//! suite run against every backend — the cases are collected generically over
//! the harness, so adding a `tdjson` source adds no cases and edits none.
//!
//! # Why the setup is declared apart from the body
//!
//! [`Case::setup`] returns the [`Setup`] the body will run against, and the
//! runner — not the body — stages it. Two things fall out. The runner can ask
//! [`Setup::capabilities`] what the case needs *before* running it, so an
//! unsupported case is skipped rather than run and misread as a failure. And
//! a case cannot quietly need something it did not declare: the body never
//! stages anything itself, so what it declares is what it gets.
//!
//! [`Case::needs`] covers the rest — what the *world* must contain rather
//! than what must be done to it, which no perturbation implies. Restricted
//! content is the only one so far.

pub(crate) mod cancellation;
pub(crate) mod cursors;
pub(crate) mod enumeration;
pub(crate) mod failures;
pub(crate) mod fetch;
pub(crate) mod shape;

use crate::conformance::harness::{Capability, Setup, SourceHarness, Staged};
use crate::conformance::report::Clause;
use crate::conformance::support::CaseResult;

/// One conformance case.
pub(crate) struct Case<H: SourceHarness> {
    /// Stable dotted identifier, e.g. `enumeration.is-repeatable`.
    pub(crate) id: &'static str,
    /// The clause this case pins.
    pub(crate) clause: Clause,
    /// What the case asserts, in one sentence, for the report.
    pub(crate) claim: &'static str,
    /// Capabilities the case needs that its [`Setup`] does not imply.
    pub(crate) needs: &'static [Capability],
    /// What the world must be for the case to mean anything.
    pub(crate) setup: fn() -> Setup,
    /// The case itself, against a world already staged.
    pub(crate) run: fn(&H, Staged<H::Source>) -> CaseResult,
}

impl<H: SourceHarness> Case<H> {
    /// Everything the harness must support for this case to run, from its
    /// setup and its declared extras alike.
    pub(crate) fn capabilities(&self) -> Vec<Capability> {
        let mut needed = (self.setup)().capabilities();
        for capability in self.needs {
            if !needed.contains(capability) {
                needed.push(*capability);
            }
        }
        needed
    }
}

/// Every case, in the order the suite runs them.
///
/// Ordered by what a failure most likely means: a source that cannot serve a
/// root or page a listing has bigger problems than its flood-wait
/// classification, and the report reads better when the foundation is checked
/// first.
pub(crate) fn all<H: SourceHarness>() -> Vec<Case<H>> {
    let mut cases = Vec::new();
    cases.extend(shape::cases::<H>());
    cases.extend(enumeration::cases::<H>());
    cases.extend(cursors::cases::<H>());
    cases.extend(fetch::cases::<H>());
    cases.extend(failures::cases::<H>());
    cases.extend(cancellation::cases::<H>());
    cases
}
