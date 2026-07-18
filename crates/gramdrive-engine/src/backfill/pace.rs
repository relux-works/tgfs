//! The account-global request pacer (SEC-031, NFR-033) — pure policy over
//! the durable [`BackfillControlRecord`].
//!
//! The scheduler issues no clock reads and holds no timers: pacing is a set
//! of pure functions from `(config, now_ms, durable record)` to a decision
//! or a next record. Two deadlines compose, and the gate honors whichever
//! is later:
//!
//! * `next_request_at_ms` — a soft request *spacer* (SEC-031's
//!   request-concurrency bound extended over time): after every issued
//!   request the next may not follow for [`PaceConfig::min_spacing_ms`],
//!   which keeps a drained backlog from turning into a request flood.
//! * `flood_wait_until_ms` — a *hard* honored Telegram flood wait: when a
//!   429 states a delay, no request issues before it (NFR-033 — a flood
//!   wait is never a tight retry loop). It survives restart so a crash
//!   cannot re-hammer an account still under the wait (NFR-031 progress
//!   survives restart, SYNC-070 startup recovery).
//!
//! Deterministic on purpose — no jitter, exactly like the transfer retry
//! policy ([`crate::transfer::RetryPolicy`]): the decorrelation jitter
//! belongs to the host that *schedules* the next tick, not to the durable
//! policy whose tests replay exactly.

use gramdrive_state::repo::BackfillControlRecord;

/// Tuning for the account-global request pacer. Every field is policy, not
/// durable state; the durable state is the [`BackfillControlRecord`] these
/// functions read and produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaceConfig {
    /// Minimum gap between two successive provider requests, in ms — the
    /// request spacer (SEC-031).
    pub min_spacing_ms: i64,
    /// The wait honored for a retryable failure that states no delay — a
    /// transport loss, or a flood wait whose delay could not be parsed. A
    /// conservative floor so an unstated backoff is never a tight loop
    /// (NFR-033).
    pub fallback_backoff_ms: i64,
    /// Retryable attempts of one step allowed before the scheduler abandons
    /// it. The count is the source machine's own per-request `attempt`
    /// (passed in), so this bound needs no durable counter of its own.
    pub attempt_budget: u32,
}

impl Default for PaceConfig {
    /// A quarter-second spacer, a thirty-second unstated-backoff floor, and
    /// five attempts — a safe starting point for hosts that state no
    /// policy, not a tuning claim.
    fn default() -> Self {
        Self {
            min_spacing_ms: 250,
            fallback_backoff_ms: 30_000,
            attempt_budget: 5,
        }
    }
}

impl PaceConfig {
    /// The spacer deadline to arm after issuing a request: `now + spacing`,
    /// saturating rather than overflowing at the end of time.
    pub(crate) fn next_request_after(&self, now_ms: i64) -> i64 {
        now_ms.saturating_add(self.min_spacing_ms.max(0))
    }

    /// The flood-wait deadline to honor: `now + stated`, or `now + fallback`
    /// when the source stated no delay. Never earlier than `now`.
    pub(crate) fn flood_deadline(&self, retry_after_ms: Option<i64>, now_ms: i64) -> i64 {
        let wait = retry_after_ms.unwrap_or(self.fallback_backoff_ms).max(0);
        now_ms.saturating_add(wait)
    }

    /// Whether an attempt count has passed the budget — the step should be
    /// abandoned rather than retried again.
    pub(crate) fn attempt_exhausted(&self, attempt: u32) -> bool {
        attempt > self.attempt_budget
    }
}

/// Why the pacer is holding requests, when it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitReason {
    /// The request spacer between successive calls (SEC-031).
    Spacing,
    /// A honored Telegram flood wait (NFR-033).
    FloodWait,
}

/// The effective earliest time a request may issue given a durable record —
/// the later of the two deadlines — or `None` when neither is armed.
pub(crate) fn effective_deadline(record: &BackfillControlRecord) -> Option<i64> {
    match (record.next_request_at_ms, record.flood_wait_until_ms) {
        (None, None) => None,
        (spacer, flood) => Some(spacer.unwrap_or(i64::MIN).max(flood.unwrap_or(i64::MIN))),
    }
}

/// The pacer's verdict at `now_ms`: `None` when a request may issue,
/// `Some((until, reason))` when it must wait. A flood deadline in the future
/// dominates the reason; otherwise the hold is spacing.
pub(crate) fn gate(record: &BackfillControlRecord, now_ms: i64) -> Option<(i64, WaitReason)> {
    let deadline = effective_deadline(record)?;
    if deadline <= now_ms {
        return None;
    }
    let reason = match record.flood_wait_until_ms {
        Some(flood) if flood > now_ms => WaitReason::FloodWait,
        _ => WaitReason::Spacing,
    };
    Some((deadline, reason))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(spacer: Option<i64>, flood: Option<i64>) -> BackfillControlRecord {
        BackfillControlRecord {
            paused: false,
            next_request_at_ms: spacer,
            flood_wait_until_ms: flood,
            updated_at_ms: 0,
        }
    }

    #[test]
    fn no_deadlines_gate_open() {
        assert_eq!(gate(&record(None, None), 1_000), None);
    }

    #[test]
    fn spacer_holds_until_its_deadline() {
        let r = record(Some(1_500), None);
        assert_eq!(gate(&r, 1_000), Some((1_500, WaitReason::Spacing)));
        assert_eq!(gate(&r, 1_500), None, "boundary is inclusive");
        assert_eq!(gate(&r, 2_000), None);
    }

    #[test]
    fn flood_dominates_the_reason_and_honors_the_later_deadline() {
        // Flood wait later than the spacer: the later deadline wins and the
        // reason is the flood wait.
        let r = record(Some(1_200), Some(5_000));
        assert_eq!(gate(&r, 1_000), Some((5_000, WaitReason::FloodWait)));
        // Once the flood has elapsed but the spacer has not, the reason
        // falls back to spacing.
        let r = record(Some(6_000), Some(5_000));
        assert_eq!(gate(&r, 5_500), Some((6_000, WaitReason::Spacing)));
    }

    #[test]
    fn flood_deadline_honors_stated_delay_and_falls_back_when_unstated() {
        let cfg = PaceConfig::default();
        assert_eq!(cfg.flood_deadline(Some(300_000), 1_000), 301_000);
        assert_eq!(
            cfg.flood_deadline(None, 1_000),
            1_000 + cfg.fallback_backoff_ms,
            "no stated delay uses the fallback floor",
        );
        assert_eq!(
            cfg.flood_deadline(Some(-5), 1_000),
            1_000,
            "a negative stated delay never rewinds the clock",
        );
    }

    #[test]
    fn spacer_and_budget_are_saturating_and_bounded() {
        let cfg = PaceConfig::default();
        assert_eq!(cfg.next_request_after(i64::MAX), i64::MAX, "no overflow");
        assert!(!cfg.attempt_exhausted(cfg.attempt_budget));
        assert!(cfg.attempt_exhausted(cfg.attempt_budget + 1));
    }
}
