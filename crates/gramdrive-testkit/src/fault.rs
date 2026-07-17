//! Scripted faults: what goes wrong, to which call, and when.
//!
//! # A delay is a count of cancellation points, not a duration
//!
//! [`Fault::delay`] takes a number of yields, not a `Duration`, and this is
//! the crate's central design decision rather than a shortcut.
//!
//! A fake that slept on a real clock would be non-reproducible by
//! construction — the contradiction of a deterministic fixture whose
//! results depend on how loaded the host is — and it would force a runtime
//! dependency on every consumer just to advance a timer. More to the point,
//! nothing the delay exists to test is about elapsed time. What the tests
//! need is what an `await` *provides*: a point where the caller can stop
//! polling and drop the future (SYNC-043), a point where a concurrent call
//! can interleave, a point where a fetch is provably still in flight. A
//! yield is exactly that point, minus the clock.
//!
//! So `delay(3)` means "return `Pending` three times before proceeding",
//! giving a test three places to cancel and a real runtime three places to
//! reschedule. Wall-clock time reaches the contract only where the contract
//! actually names it — the `Duration` inside
//! [`SourceError::RateLimited`](gramdrive_source::SourceError::RateLimited),
//! which is data the caller reads, not time anyone waits.
//!
//! # Matching
//!
//! A fault fires when its [`Operation`] matches the call, its item filter
//! matches (or is absent), and its [`Occurrence`] matches the count of
//! calls that have already matched the first two. Counting is per-fault and
//! one-based, so `Occurrence::Nth(1)` is "the first attempt" — the shape of
//! "fail once, then succeed on retry".
//!
//! At most one fault *fires* per call: the first in script order whose
//! occurrence is due. But **every** fault that matches the operation and
//! item advances its own counter, firing or not — a fault counts the calls
//! it saw, not the calls it won.
//!
//! This is what makes occurrences compose. Two `Fetch` faults at `Nth(1)`
//! and `Nth(2)` fire on the first and second fetch, because the second one
//! counted the first fetch even though the first fault handled it. Under
//! the other rule — counters advancing only on the fault that fired — the
//! second fault would still be at count 0 after the first call, fire on the
//! *third* fetch, and `Nth(n)` would silently mean "the n-th call this
//! particular fault happened to win", which depends on what else is in the
//! script.
//!
//! ```
//! # use gramdrive_testkit::{Fault, Occurrence, Operation};
//! # use gramdrive_testkit::source::SourceError;
//! // Fail the first fetch attempt; a retry succeeds.
//! let flaky = Fault::on(Operation::Fetch)
//!     .occurrence(Occurrence::Nth(1))
//!     .fail(SourceError::Unavailable { detail: "link dropped".to_owned() });
//!
//! // Every enumeration takes two yields to answer.
//! let slow = Fault::on(Operation::Children).delay(2);
//! ```

use gramdrive_model::identity::ItemId;
use gramdrive_model::version::ContentVersion;
use gramdrive_source::SourceError;

/// Which `DriveSource` operation a fault targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Operation {
    /// [`DriveSource::root`](gramdrive_source::DriveSource::root).
    Root,
    /// [`DriveSource::children`](gramdrive_source::DriveSource::children).
    /// The item filter matches the *parent* being enumerated.
    Children,
    /// [`DriveSource::latest_cursor`](gramdrive_source::DriveSource::latest_cursor).
    LatestCursor,
    /// [`DriveSource::changes`](gramdrive_source::DriveSource::changes).
    Changes,
    /// [`DriveSource::fetch`](gramdrive_source::DriveSource::fetch).
    Fetch,
    /// [`DriveSource::thumbnail`](gramdrive_source::DriveSource::thumbnail).
    Thumbnail,
}

/// Which matching calls a fault fires on.
///
/// Counted per fault, one-based, over calls that already matched the
/// operation and item filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Occurrence {
    /// Every matching call.
    Always,
    /// Exactly the `n`-th matching call. `Nth(1)` is the first attempt.
    Nth(u32),
    /// The `n`-th matching call and every one after it — a source that
    /// breaks and stays broken.
    FromNth(u32),
    /// The first `n` matching calls — a source that fails a bounded number
    /// of times, then recovers.
    FirstN(u32),
}

impl Occurrence {
    /// Whether a fault with this occurrence fires on the `count`-th
    /// matching call (one-based).
    pub fn fires_on(self, count: u32) -> bool {
        match self {
            Self::Always => true,
            Self::Nth(n) => count == n,
            Self::FromNth(n) => count >= n,
            Self::FirstN(n) => count <= n,
        }
    }
}

/// What a fault does once it fires, after any delay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Nothing — the call proceeds normally. Paired with a delay, this is
    /// how a call is made slow (and cancellable) without being broken.
    Proceed,
    /// Fail the call with this error.
    Fail(SourceError),
    /// Deliver `after_bytes` of the requested range, then fail with
    /// [`SourceError::VersionConflict`] — content that changed underneath
    /// a fetch already in flight.
    ///
    /// The race the drive core must survive, and the one a fake that only
    /// answers requests cannot pose: it proves partial bytes of version A
    /// are never published as version B (SYNC-042). `after_bytes` of `0`
    /// fails before delivering anything — the same conflict observed one
    /// moment earlier.
    ///
    /// `Fetch` only; any other operation is rejected by
    /// [`SourceScript`](crate::SourceScript) at build time.
    VersionRace {
        /// Bytes delivered before the conflict surfaces. Clamped to the
        /// requested range's length.
        after_bytes: u64,
        /// The version the source reports as current, carried in the
        /// error. `None` models a source that knows the pin is stale but
        /// not what replaced it.
        current: Option<ContentVersion>,
    },
}

/// One scripted fault.
///
/// Build with [`Fault::on`] and register with
/// [`ScriptBuilder::fault`](crate::ScriptBuilder::fault).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fault {
    pub(crate) operation: Operation,
    pub(crate) item: Option<ItemId>,
    pub(crate) occurrence: Occurrence,
    pub(crate) delay_yields: u32,
    pub(crate) effect: Effect,
}

impl Fault {
    /// A fault on every call to `operation`, with no delay and no effect.
    ///
    /// On its own this changes nothing; add [`delay`](Self::delay),
    /// [`fail`](Self::fail), or [`version_race`](Self::version_race).
    pub fn on(operation: Operation) -> Self {
        Self {
            operation,
            item: None,
            occurrence: Occurrence::Always,
            delay_yields: 0,
            effect: Effect::Proceed,
        }
    }

    /// Narrows the fault to calls targeting `item` — the parent for
    /// `children`, the item for `fetch` and `thumbnail`.
    ///
    /// Ignored by the account-wide operations (`root`, `latest_cursor`,
    /// `changes`), which target no item; a filter on those is rejected at
    /// build time rather than silently never firing.
    pub fn for_item(mut self, item: ItemId) -> Self {
        self.item = Some(item);
        self
    }

    /// Sets which matching calls fire. Defaults to [`Occurrence::Always`].
    pub fn occurrence(mut self, occurrence: Occurrence) -> Self {
        self.occurrence = occurrence;
        self
    }

    /// Yields `count` times before the effect — `count` cancellation
    /// points, not a duration. See the module docs.
    pub fn delay(mut self, count: u32) -> Self {
        self.delay_yields = count;
        self
    }

    /// Fails the call with `error` once the fault fires.
    pub fn fail(mut self, error: SourceError) -> Self {
        self.effect = Effect::Fail(error);
        self
    }

    /// Races a version change against an in-flight fetch: deliver
    /// `after_bytes`, then conflict. See [`Effect::VersionRace`].
    pub fn version_race(mut self, after_bytes: u64, current: Option<ContentVersion>) -> Self {
        self.effect = Effect::VersionRace {
            after_bytes,
            current,
        };
        self
    }

    /// Whether this fault matches a call to `operation` against `item`.
    pub(crate) fn matches(&self, operation: Operation, item: Option<&ItemId>) -> bool {
        if self.operation != operation {
            return false;
        }
        match (&self.item, item) {
            (None, _) => true,
            (Some(wanted), Some(actual)) => wanted == actual,
            (Some(_), None) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;

    fn item() -> ItemId {
        fixture::account_root_id(fixture::scope())
    }

    #[test]
    fn always_fires_on_every_call() {
        for count in 1..=5 {
            assert!(Occurrence::Always.fires_on(count));
        }
    }

    #[test]
    fn nth_fires_once() {
        let occurrence = Occurrence::Nth(2);
        assert!(!occurrence.fires_on(1));
        assert!(occurrence.fires_on(2));
        assert!(!occurrence.fires_on(3));
    }

    #[test]
    fn from_nth_fires_from_a_point_onward() {
        let occurrence = Occurrence::FromNth(3);
        assert!(!occurrence.fires_on(1));
        assert!(!occurrence.fires_on(2));
        assert!(occurrence.fires_on(3));
        assert!(occurrence.fires_on(100));
    }

    #[test]
    fn first_n_fires_a_bounded_number_of_times() {
        let occurrence = Occurrence::FirstN(2);
        assert!(occurrence.fires_on(1));
        assert!(occurrence.fires_on(2));
        assert!(!occurrence.fires_on(3));
    }

    #[test]
    fn an_unfiltered_fault_matches_any_item() {
        let fault = Fault::on(Operation::Fetch);
        assert!(fault.matches(Operation::Fetch, Some(&item())));
        assert!(fault.matches(Operation::Fetch, None));
        assert!(!fault.matches(Operation::Children, Some(&item())));
    }

    #[test]
    fn an_item_filter_matches_only_that_item() {
        let other = fixture::chat_list_id(
            fixture::scope(),
            gramdrive_model::identity::ChatListKind::Archive,
        );
        let fault = Fault::on(Operation::Fetch).for_item(item());
        assert!(fault.matches(Operation::Fetch, Some(&item())));
        assert!(!fault.matches(Operation::Fetch, Some(&other)));
        assert!(
            !fault.matches(Operation::Fetch, None),
            "an item filter cannot match a call that targets no item"
        );
    }

    #[test]
    fn builder_defaults_are_inert() {
        let fault = Fault::on(Operation::Root);
        assert_eq!(fault.occurrence, Occurrence::Always);
        assert_eq!(fault.delay_yields, 0);
        assert_eq!(fault.effect, Effect::Proceed);
        assert_eq!(fault.item, None);
    }

    #[test]
    fn builder_composes_delay_and_effect() {
        let error = SourceError::Unavailable {
            detail: "offline".to_owned(),
        };
        let fault = Fault::on(Operation::Fetch)
            .for_item(item())
            .occurrence(Occurrence::Nth(1))
            .delay(3)
            .fail(error.clone());
        assert_eq!(fault.delay_yields, 3);
        assert_eq!(fault.effect, Effect::Fail(error));
        assert_eq!(fault.occurrence, Occurrence::Nth(1));
    }

    #[test]
    fn version_race_carries_its_cut_and_current_version() {
        let current = ContentVersion::new("c2").unwrap();
        let fault = Fault::on(Operation::Fetch).version_race(64, Some(current.clone()));
        assert_eq!(
            fault.effect,
            Effect::VersionRace {
                after_bytes: 64,
                current: Some(current)
            }
        );
    }
}
