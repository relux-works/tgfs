//! What a backend must lend the suite in order to be asked the questions.
//!
//! # The suite states the world; the harness materializes it
//!
//! A conformance case needs a directory with children to page through and a
//! file with known bytes to fetch. It cannot go looking for them — "find a
//! chat with at least seven children" is how a suite ends up asserting
//! against whatever a test account happens to hold that week. So the suite
//! declares [`WORLD`], one fixed world; the harness builds it and answers
//! with [`Landmarks`] saying where each part of it landed.
//!
//! Neither half knows the other's vocabulary. The suite never names a chat, a
//! revision, or a page-token format; the harness never names a clause. What
//! crosses between them is [`ItemId`]s, bytes, and versions — the contract's
//! own words.
//!
//! # Arming and mutating are different things
//!
//! Two kinds of interference, split because backends implement them
//! differently:
//!
//! - [`Perturbation`] — armed *before* the source goes live: a call that will
//!   fail, rate-limit, take its time, or lose a race. A backend arms these in
//!   whatever layer it can (a scripted fault, a proxy, a stubbed transport).
//! - [`Mutation`] — applied *while* the source is live: a child appears, a
//!   child leaves, content moves on. These have to be mid-test, because the
//!   clause under test is what happens to an enumeration or a fetch that was
//!   already running.
//!
//! Mutations are declared up front in [`Setup::plan`] and applied in order by
//! [`Control::advance`]. Declaring them buys the harness the right to prepare
//! — the deterministic fake compiles the plan into change batches at build
//! time, and a real backend can pre-stage the messages it will send — without
//! costing the case anything: a case knows what it intends to do to the world
//! before it starts.
//!
//! # Unsupported must be visible
//!
//! No backend stages everything. A `tdjson` source against a live account
//! cannot conjure a flood wait on demand, and one that pretended to would be
//! testing its own pretence. [`SourceHarness::supports`] lets a harness
//! decline, and the case is reported
//! [`Skipped`](crate::conformance::CaseOutcome::Skipped) — never passed. The
//! suite is worth exactly what it is allowed to ask.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use gramdrive_model::identity::ItemId;
use gramdrive_model::version::ContentVersion;
use gramdrive_source::DriveSource;

use crate::conformance::report::HarnessError;
use crate::fault::Operation;

/// The world every case is staged against.
///
/// Fixed, not configurable: a suite whose fixture varies per backend is a
/// suite whose results cannot be compared across backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorldSpec {
    /// How many children the listing directory starts with. More than one
    /// page at any sane page size, and enough that a paging bug has room to
    /// drop or repeat one.
    pub listing_children: u32,
    /// The bytes behind the world's file at [`file_version`](Self::file_version).
    pub file_bytes: &'static [u8],
    /// The file's content version before any mutation.
    pub file_version: &'static str,
    /// The bytes the file holds after [`Mutation::ContentChanges`].
    pub next_file_bytes: &'static [u8],
    /// The file's content version after [`Mutation::ContentChanges`].
    /// Distinct from [`file_version`](Self::file_version) — that is the whole
    /// point of it.
    pub next_file_version: &'static str,
}

/// The one world the suite knows how to ask about.
pub const WORLD: WorldSpec = WorldSpec {
    listing_children: 7,
    file_bytes: b"gramdrive conformance payload: the exact bytes a source owes a caller.",
    file_version: "conformance-c1",
    next_file_bytes: b"gramdrive conformance payload, revised: different bytes, different version.",
    next_file_version: "conformance-c2",
};

// A listing that fits in one page at the sizes the paging cases use would let
// every SYNC-003 case pass without a second page ever being asked for. Checked
// at compile time rather than in a test: it is a property of the constant, and
// a test asserting on a constant is a test the compiler folds away.
const _: () = assert!(
    WORLD.listing_children >= 3,
    "the listing must span several pages at the page sizes the cases use"
);

// The range cases slice the file at fixed offsets. Checked here so that
// shortening the payload is a compile error rather than an index panic
// surfacing from inside a case, where it would read as a crashed suite rather
// than a broken constant.
const _: () = assert!(
    WORLD.file_bytes.len() >= 16,
    "the file must be long enough for the range cases to take a slice out of its middle"
);

/// Where the harness put each part of [`WORLD`].
///
/// Identities only — the suite reads the rest through the contract. A harness
/// that returns a landmark it did not actually build fails the cases that use
/// it, which is the right outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Landmarks {
    /// The account root — the only parentless item.
    pub root: ItemId,
    /// A directory holding exactly [`WorldSpec::listing_children`] children.
    pub listing: ItemId,
    /// Those children, in no particular order: the suite compares sets and
    /// asserts the *source's* order is repeatable, never that it matches
    /// this vector.
    pub listing_children: Vec<ItemId>,
    /// The child [`Mutation::ChildRemoved`] removes. One of
    /// [`listing_children`](Self::listing_children).
    pub removable_child: ItemId,
    /// The child [`Mutation::ChildAppears`] adds. Not in
    /// [`listing_children`](Self::listing_children) until then.
    pub appearing_child: ItemId,
    /// A directory with no children at all.
    pub empty_directory: ItemId,
    /// The directory holding [`file`](Self::file) and
    /// [`restricted_file`](Self::restricted_file).
    ///
    /// The contract has no lookup by identity, so a case that needs the
    /// *item* rather than its bytes reaches it the only way a caller can:
    /// by enumerating its parent.
    pub file_parent: ItemId,
    /// A fetchable file holding [`WorldSpec::file_bytes`].
    pub file: ItemId,
    /// The file's content version, for pinning a fetch.
    pub file_version: ContentVersion,
    /// What the file's version becomes after [`Mutation::ContentChanges`].
    pub next_file_version: ContentVersion,
    /// A file whose bytes the source refuses to serve (POL-4). `None` when
    /// the backend cannot stage restricted content — the POL-4 cases are
    /// skipped rather than faked.
    pub restricted_file: Option<ItemId>,
    /// An identity of an item that does not exist in this world.
    pub absent: ItemId,
}

/// Something a backend must be able to do for a case to run at all.
///
/// Named after what it stages, not after how: "can you make a call fail
/// transiently" is answerable by a scripted fake, a fault-injecting proxy, or
/// a backend built with a test hook, and the suite does not care which.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Capability {
    /// The world can be changed while a source is live — a child appears or
    /// leaves, content moves on.
    Mutation,
    /// A named call can be made to fail with a chosen category.
    FaultInjection,
    /// A named call can be made slow enough to be cancelled in flight.
    Latency,
    /// A fetch can be made to lose a race against a content change while it
    /// is already delivering.
    VersionRace,
    /// The world can hold content the source refuses to serve (POL-4).
    RestrictedContent,
}

impl Capability {
    /// What a harness must be able to stage to claim this.
    pub fn requirement(self) -> &'static str {
        match self {
            Self::Mutation => "a world that changes while a source is live",
            Self::FaultInjection => "a call that fails with a chosen category",
            Self::Latency => "a call slow enough to cancel in flight",
            Self::VersionRace => "a content change that lands mid-fetch",
            Self::RestrictedContent => "content the source refuses to serve",
        }
    }
}

impl std::fmt::Display for Capability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?} ({})", self.requirement())
    }
}

/// Interference armed before the source goes live.
///
/// Every variant names the [`Operation`] it targets, so "the backend is
/// broken" is always "this call is broken" — a source whose every operation
/// fails proves nothing about the one under test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Perturbation {
    /// The first call to `operation` fails because the source could not be
    /// reached; later calls succeed. The shape of "fail once, recover on
    /// retry".
    ///
    /// Specifically unreachable, not "some retryable failure": a case can
    /// only pin the category a backend must report if the harness staged a
    /// condition with one right answer.
    Unreachable {
        /// The call that fails.
        operation: Operation,
    },
    /// The first call to `operation` fails because the source's locator for
    /// the content expired — Telegram's `FILE_REFERENCE_EXPIRED` class. The
    /// reference is refreshable, so a later call succeeds.
    ///
    /// The one failure with a recovery protocol rather than a wait: the
    /// adapter refreshes and the caller retries, and the item's identity must
    /// come through unmoved (SYNC-045, DOM-007).
    ReferenceExpired {
        /// The call that fails.
        operation: Operation,
    },
    /// The first call to `operation` is throttled, carrying `retry_after`.
    RateLimited {
        /// The call that is throttled.
        operation: Operation,
        /// The backoff the source states, if any.
        retry_after: Option<Duration>,
    },
    /// Every call to `operation` fails for want of authorization.
    AuthRevoked {
        /// The call that fails.
        operation: Operation,
    },
    /// Every call to `operation` reaches at least one suspension point
    /// before answering, so a caller can stop polling and drop it.
    Slow {
        /// The call that is slow.
        operation: Operation,
    },
    /// A fetch delivers `after_bytes` of its range and then loses a race to a
    /// content change: the pinned version is gone, mid-delivery.
    FetchRacesContentChange {
        /// Bytes delivered before the conflict surfaces.
        after_bytes: u64,
    },
}

impl Perturbation {
    /// What a harness must support to arm this.
    pub fn capability(&self) -> Capability {
        match self {
            Self::Unreachable { .. }
            | Self::ReferenceExpired { .. }
            | Self::RateLimited { .. }
            | Self::AuthRevoked { .. } => Capability::FaultInjection,
            Self::Slow { .. } => Capability::Latency,
            Self::FetchRacesContentChange { .. } => Capability::VersionRace,
        }
    }
}

/// A change to the world, applied while the source is live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mutation {
    /// [`Landmarks::appearing_child`] joins the listing directory.
    ChildAppears,
    /// [`Landmarks::removable_child`] is removed at the source.
    ChildRemoved,
    /// The file's content moves to [`WorldSpec::next_file_version`].
    ContentChanges,
}

impl Mutation {
    /// What a harness must support to apply this.
    pub fn capability(self) -> Capability {
        Capability::Mutation
    }
}

/// What one case needs staged.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Setup {
    /// Interference armed before the source goes live.
    pub arm: Vec<Perturbation>,
    /// The mutations this case will apply, in the order
    /// [`Control::advance`] will apply them.
    pub plan: Vec<Mutation>,
}

impl Setup {
    /// A plain world: nothing armed, nothing planned.
    pub fn new() -> Self {
        Self::default()
    }

    /// Arms `perturbation` before the source goes live.
    pub fn arm(mut self, perturbation: Perturbation) -> Self {
        self.arm.push(perturbation);
        self
    }

    /// Appends `mutation` to the plan [`Control::advance`] walks.
    pub fn plan(mut self, mutation: Mutation) -> Self {
        self.plan.push(mutation);
        self
    }

    /// Every capability this setup needs, in declaration order.
    pub fn capabilities(&self) -> Vec<Capability> {
        let mut needed: Vec<Capability> = Vec::new();
        for capability in self
            .arm
            .iter()
            .map(Perturbation::capability)
            .chain(self.plan.iter().map(|mutation| mutation.capability()))
        {
            if !needed.contains(&capability) {
                needed.push(capability);
            }
        }
        needed
    }
}

/// Applies the staged mutation plan, one step at a time.
pub trait Control {
    /// Applies the next mutation in [`Setup::plan`], returning `false` when
    /// the plan is drained.
    ///
    /// Returns after the change is observable through the source: a case that
    /// calls this and then enumerates must see the new world, or the clause
    /// under test is not the one being measured.
    fn advance(&self) -> Result<bool, HarnessError>;
}

/// A staged world and the live source serving it.
pub struct Staged<S> {
    /// The source under test. Shared, as sources are in production.
    pub source: Arc<S>,
    /// Where the world's parts landed.
    pub landmarks: Landmarks,
    /// The mutation plan's driver.
    pub control: Box<dyn Control>,
}

// Written out rather than derived: deriving would demand `S: Debug` of every
// backend's source type and `Debug` of every `Control`, to print two things
// no reader of a staged world wants. The landmarks are the part worth seeing —
// they are what a case addresses.
impl<S> std::fmt::Debug for Staged<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Staged")
            .field("landmarks", &self.landmarks)
            .finish_non_exhaustive()
    }
}

/// A backend's side of the conformance suite.
///
/// Implement this once per `DriveSource` implementation and hand it to
/// [`run`](crate::conformance::run). The suite is generic over it rather than
/// taking a trait object: [`block_on`](Self::block_on) is generic in the
/// future's output, because how futures are driven is the backend's business
/// — the deterministic fake needs no runtime at all, and a `tdjson` source
/// needs its own.
pub trait SourceHarness {
    /// The source this harness stages.
    type Source: DriveSource + 'static;

    /// The backend's name, for the report.
    fn name(&self) -> &str;

    /// Whether this harness can stage `capability`.
    ///
    /// Answer honestly. A `true` the harness cannot honor surfaces as a
    /// staging failure, and a `false` surfaces as a skip — the second is a
    /// gap, the first is a broken fixture.
    fn supports(&self, capability: Capability) -> bool;

    /// Drives `future` to completion.
    fn block_on<T>(&self, future: impl Future<Output = T>) -> T;

    /// Builds [`WORLD`] with `setup` staged, and returns it live.
    ///
    /// Called once per case: a case must not inherit another's mutations or
    /// spent faults. A harness whose backend is expensive to rebuild may
    /// reset rather than recreate, so long as what the case sees is a world
    /// no earlier case has touched.
    fn stage(&self, world: &WorldSpec, setup: &Setup)
    -> Result<Staged<Self::Source>, HarnessError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_world_pins_two_distinct_content_versions() {
        assert_ne!(
            WORLD.file_version, WORLD.next_file_version,
            "SYNC-042 needs a version the content can move away from"
        );
        assert_ne!(
            WORLD.file_bytes, WORLD.next_file_bytes,
            "a race is only observable if the bytes differ"
        );
    }

    #[test]
    fn a_setup_reports_every_capability_it_needs_once() {
        let setup = Setup::new()
            .arm(Perturbation::Unreachable {
                operation: Operation::Fetch,
            })
            .arm(Perturbation::AuthRevoked {
                operation: Operation::Root,
            })
            .arm(Perturbation::Slow {
                operation: Operation::Children,
            })
            .plan(Mutation::ChildAppears)
            .plan(Mutation::ContentChanges);

        assert_eq!(
            setup.capabilities(),
            vec![
                Capability::FaultInjection,
                Capability::Latency,
                Capability::Mutation
            ],
            "each capability once, in declaration order"
        );
    }

    #[test]
    fn a_plain_setup_needs_nothing() {
        assert!(Setup::new().capabilities().is_empty());
    }

    #[test]
    fn every_perturbation_names_the_capability_that_stages_it() {
        assert_eq!(
            Perturbation::FetchRacesContentChange { after_bytes: 8 }.capability(),
            Capability::VersionRace
        );
        assert_eq!(
            Perturbation::RateLimited {
                operation: Operation::Fetch,
                retry_after: Some(Duration::from_millis(10)),
            }
            .capability(),
            Capability::FaultInjection
        );
        assert_eq!(Mutation::ChildRemoved.capability(), Capability::Mutation);
    }

    #[test]
    fn a_capability_describes_what_it_asks_of_a_harness() {
        assert!(
            Capability::VersionRace.to_string().contains("mid-fetch"),
            "the report tells a harness author what it declined"
        );
    }
}
