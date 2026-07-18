//! The metadata-first local backfill scheduler (TASK-260715-mua1ng;
//! POL-2/DEC-014, SYNC-020/021, SEC-031, NFR-033, NFR-031, SYNC-070).
//!
//! # What this layer owns
//!
//! The tdjson source ships sans-IO machines — the history `CrawlMachine`,
//! the `LiveMachine`, the `SnapshotMachine` — that each name one obligation
//! (`Submit`, `Backoff`, `Commit`, …) and expect a composing caller to own
//! the clock, the request pacing, and the flood-wait budget. This is that
//! caller's *policy*, kept provider-neutral: it reads no TDLib type, only
//! the durable projection `gramdrive-state` persists and the source failure
//! taxonomy. The tdjson glue that maps a [`BackfillStep::AdvanceHistory`]
//! onto `CrawlMachine::set_priority` is a thin host/FFI seam layered on top.
//!
//! # Metadata first, and no eager mobile media (POL-2, SYNC-020)
//!
//! [`BackfillScheduler::plan_next`] only ever schedules *history* — chat
//! metadata and message text. It never returns a media action. Media is not
//! mirrored eagerly by default: it hydrates on open (the fetch coordinator's
//! job) or, under Archive Mode, in bulk — and even then only when
//! [`BackfillScheduler::media_policy`] says the conditions permit it. That
//! separation is the whole point: the default install never pulls a
//! gigabyte of video onto a phone just because it discovered a chat.
//!
//! # Visible-item priority (task description)
//!
//! History work is ordered [`BackfillPriority::Visible`] (a chat on screen)
//! before [`BackfillPriority::Requested`] (a chat the user opened into)
//! before [`BackfillPriority::Background`] (the least-recently-synced tail
//! of the state layer's `backfill_backlog`). Foreground work runs even under
//! a metered network or power saving — the user is waiting; only background
//! metadata yields to those constraints.
//!
//! # Durable, bounded, observable, pausable (the AC)
//!
//! - **Durable:** the pause switch and the flood-wait deadline live in the
//!   [`BackfillControlRecord`] row, so a restart resumes neither paused work
//!   nor a violated flood wait. Per-chat progress is the existing
//!   `chat_sync_state` row.
//! - **Bounded:** one action per [`plan_next`](BackfillScheduler::plan_next)
//!   call, and the background backlog scan is capped at
//!   [`BackfillConfig::backlog_scan`].
//! - **Observable:** [`BackfillScheduler::observe`] reports the pause state,
//!   the pending gate deadline, and the bounded history backlog size.
//! - **Pausable:** [`BackfillScheduler::set_paused`] durably toggles the
//!   switch; a paused scheduler plans [`BackfillStep::Paused`] and suspends
//!   eager media.
//!
//! # Determinism
//!
//! No clock and no entropy. Time enters as `now_ms` on every method, exactly
//! like the state layer and the transfer machine, so a scripted test reads
//! back the same decision on every run.

mod pace;

use std::collections::HashSet;

use gramdrive_model::identity::{AccountScope, ChatId, ChatKey};
use gramdrive_state::StateStore;
use gramdrive_state::repo::BackfillControlRecord;

use crate::transfer::EngineError;

pub use pace::{PaceConfig, WaitReason};

/// Scheduler tuning. Policy only — the durable state is the
/// [`BackfillControlRecord`] and the `chat_sync_state` rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackfillConfig {
    /// The account-global request pacer.
    pub pace: PaceConfig,
    /// The most background-backlog chats one plan considers — the bound that
    /// keeps a huge account's backlog read (and the [`observe`] count)
    /// cheap.
    ///
    /// [`observe`]: BackfillScheduler::observe
    pub backlog_scan: u32,
}

impl Default for BackfillConfig {
    fn default() -> Self {
        Self {
            pace: PaceConfig::default(),
            backlog_scan: 32,
        }
    }
}

/// How urgent one chat's history is, highest first (the task description's
/// visible-item priority). Maps 1:1 onto the tdjson crawl machine's own
/// priority ladder at the host seam.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BackfillPriority {
    /// The opportunistic backlog tail.
    Background,
    /// A chat the user explicitly opened into.
    Requested,
    /// A chat on screen right now.
    Visible,
}

/// Why the scheduler has no history action to hand out this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleReason {
    /// Every reachable chat has complete history — nothing to backfill until
    /// new demand or a live update arrives.
    BacklogDrained,
    /// There is no connectivity; the host retries when the network returns.
    Offline,
    /// Background metadata is deferred on a metered network; foreground work
    /// (visible/requested) would still have run had any been due.
    Metered,
    /// Background metadata is deferred while the device is saving power.
    PowerSaving,
}

/// The one action a [`plan_next`](BackfillScheduler::plan_next) call yields.
///
/// History only — media never appears here (see [`media_policy`] and the
/// module docs).
///
/// [`media_policy`]: BackfillScheduler::media_policy
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackfillStep {
    /// The user paused backfill; nothing runs until [`set_paused`] clears it.
    ///
    /// [`set_paused`]: BackfillScheduler::set_paused
    Paused,
    /// A pacer hold: no provider request may issue before `until_ms`. The
    /// host schedules the next tick for then.
    Wait {
        /// The wall-clock ms the hold clears at.
        until_ms: i64,
        /// Whether the hold is the request spacer or a flood wait.
        reason: WaitReason,
    },
    /// Advance one chat's history — drive one crawl step for `chat_id`. The
    /// host maps `priority` onto the crawl machine and, on the outcome,
    /// reports back through [`note_dispatch`]/[`note_flood_wait`].
    ///
    /// [`note_dispatch`]: BackfillScheduler::note_dispatch
    /// [`note_flood_wait`]: BackfillScheduler::note_flood_wait
    AdvanceHistory {
        /// The chat to advance.
        chat_id: ChatId,
        /// How it was prioritized.
        priority: BackfillPriority,
    },
    /// No history action is due; `reason` says why.
    Idle {
        /// Why nothing runs this tick.
        reason: IdleReason,
    },
}

/// Whether Archive-Mode eager media backfill may run now, or why it is held
/// (POL-2/DEC-014).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaPolicy {
    /// Eager archive-media backfill is permitted: the host may hydrate the
    /// archive-scope chats' attachments in bulk. Quota-exempt by
    /// construction — this decision never consults the cache quota (POL-2:
    /// Archive-Mode content is quota-exempt); it gates only on physical disk
    /// and device conditions.
    Eager,
    /// Eager media is withheld; `reason` says why.
    Suspended {
        /// Why eager media is held.
        reason: MediaSuspend,
    },
}

/// Why eager Archive-Mode media backfill is withheld.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaSuspend {
    /// No connectivity.
    Offline,
    /// The user paused backfill.
    Paused,
    /// A flood wait is in effect; even archive media honors it (NFR-033).
    FloodWait,
    /// The account is not in Archive Mode, so there is no eager-media scope.
    /// Media still hydrates on demand (the default, POL-2).
    ArchiveModeOff,
    /// Disk space is critically low — no eager media (POL-2 disk warning).
    DiskCritical,
    /// Disk space is low — eager media is held until it recovers (POL-2).
    DiskLow,
    /// The network is metered: eager media never runs on metered links (the
    /// "no eager mobile media mirroring" rule).
    Metered,
    /// The device is saving power.
    PowerSaving,
    /// History is still being backfilled for this scope; media waits until
    /// metadata is complete (metadata-first, SYNC-020).
    MetadataPending,
}

/// The chats the user is looking at or has opened into — the visible-item
/// priority signal (the task description). Both lists are the host's live
/// view, not durable state.
#[derive(Debug, Clone, Copy)]
pub struct BackfillDemand<'a> {
    /// Chats on screen right now — highest priority.
    pub visible: &'a [ChatId],
    /// Chats the user explicitly opened into — above background, below
    /// visible.
    pub requested: &'a [ChatId],
}

impl BackfillDemand<'_> {
    /// No foreground demand — a purely background pass.
    pub const NONE: BackfillDemand<'static> = BackfillDemand {
        visible: &[],
        requested: &[],
    };
}

/// Connectivity class the host reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkState {
    /// Unmetered connectivity — all work permitted.
    Online,
    /// Connected but metered (typically cellular): foreground history still
    /// runs, but background metadata and all eager media do not.
    Metered,
    /// No connectivity — nothing runs.
    Offline,
}

/// Device power class the host reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerState {
    /// No power constraint.
    Unconstrained,
    /// Low-power / battery-saving: background metadata and eager media defer.
    Saving,
}

/// Disk headroom the host reports — the POL-2 disk-space warning input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskState {
    /// Ample free space.
    Ample,
    /// Low free space — eager media backfill is withheld and surfaced.
    Low,
    /// Critically low — eager media backfill is withheld.
    Critical,
}

/// Device power/network/disk conditions the host supplies each tick. The
/// scheduler reads no platform API of its own (it must not — the engine
/// crate forbids `cfg(target_os)`); these are the whole of its device
/// awareness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostConditions {
    /// Connectivity class.
    pub network: NetworkState,
    /// Power class.
    pub power: PowerState,
    /// Disk headroom.
    pub disk: DiskState,
}

impl HostConditions {
    /// Unconstrained desktop-like conditions — the default for a deep
    /// backfill host.
    pub const UNCONSTRAINED: Self = Self {
        network: NetworkState::Online,
        power: PowerState::Unconstrained,
        disk: DiskState::Ample,
    };

    /// Whether background (non-foreground) history metadata may run: only on
    /// an unmetered network with no power constraint. Foreground history
    /// runs regardless (the user is waiting).
    fn background_permitted(&self) -> bool {
        matches!(self.network, NetworkState::Online)
            && matches!(self.power, PowerState::Unconstrained)
    }
}

/// The outcome of reporting a flood wait to the pacer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FloodOutcome {
    /// The armed flood-wait deadline in ms — no request issues before it.
    pub until_ms: i64,
    /// Whether the failed step has exhausted its attempt budget and should
    /// be abandoned rather than retried again (NFR-033).
    pub exhausted: bool,
}

/// A point-in-time view of a scope's backfill state for the UI and logs
/// (the AC's observability).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackfillObservation {
    /// Whether backfill is paused.
    pub paused: bool,
    /// The armed request spacer deadline, if any.
    pub next_request_at_ms: Option<i64>,
    /// The armed flood-wait deadline, if any.
    pub flood_wait_until_ms: Option<i64>,
    /// The effective gate deadline, if a hold is currently in the future.
    pub pending_until_ms: Option<i64>,
    /// Chats still needing history, counted up to
    /// [`BackfillConfig::backlog_scan`] (a lower bound past the cap).
    pub history_backlog: usize,
    /// Whether the backlog count hit the scan cap (the true backlog may be
    /// larger).
    pub backlog_capped: bool,
}

/// The metadata-first local backfill scheduler — stateless policy over the
/// durable projection. Construct once per host and call across ticks; all
/// state lives in `gramdrive-state`.
#[derive(Debug, Clone, Copy)]
pub struct BackfillScheduler {
    config: BackfillConfig,
}

impl BackfillScheduler {
    /// A scheduler with the given tuning.
    pub fn new(config: BackfillConfig) -> Self {
        Self { config }
    }

    /// A scheduler with default tuning.
    pub fn with_defaults() -> Self {
        Self::new(BackfillConfig::default())
    }

    /// The tuning in effect.
    pub fn config(&self) -> &BackfillConfig {
        &self.config
    }

    /// Decide the next history action for `scope` (SYNC-020, SYNC-021).
    ///
    /// One action per call and never media: `Paused` when paused, `Wait`
    /// when the pacer holds, `AdvanceHistory` for the highest-priority chat
    /// that still needs history, else `Idle` with the binding reason. Opens
    /// one read transaction.
    pub fn plan_next(
        &self,
        store: &mut StateStore,
        scope: AccountScope,
        demand: BackfillDemand<'_>,
        conditions: HostConditions,
        now_ms: i64,
    ) -> Result<BackfillStep, EngineError> {
        if matches!(conditions.network, NetworkState::Offline) {
            return Ok(BackfillStep::Idle {
                reason: IdleReason::Offline,
            });
        }

        let tx = store.read_txn()?;
        let control = tx
            .backfill_control(scope)?
            .unwrap_or_else(|| BackfillControlRecord::fresh(now_ms));

        if control.paused {
            return Ok(BackfillStep::Paused);
        }
        if let Some((until_ms, reason)) = pace::gate(&control, now_ms) {
            return Ok(BackfillStep::Wait { until_ms, reason });
        }

        // Foreground first: visible, then requested. Both run even under a
        // metered/power-saving constraint — a user is waiting on them.
        for &chat_id in demand.visible {
            if chat_needs_history(&tx, scope, chat_id)? {
                return Ok(BackfillStep::AdvanceHistory {
                    chat_id,
                    priority: BackfillPriority::Visible,
                });
            }
        }
        let visible: HashSet<i64> = demand.visible.iter().map(|c| c.0).collect();
        for &chat_id in demand.requested {
            if visible.contains(&chat_id.0) {
                continue;
            }
            if chat_needs_history(&tx, scope, chat_id)? {
                return Ok(BackfillStep::AdvanceHistory {
                    chat_id,
                    priority: BackfillPriority::Requested,
                });
            }
        }

        // Background backlog: least-recently-synced first, skipping any chat
        // already offered as foreground work.
        let foreground: HashSet<i64> = visible
            .into_iter()
            .chain(demand.requested.iter().map(|c| c.0))
            .collect();
        let backlog = tx.backfill_backlog(&scope, self.config.backlog_scan)?;
        let has_background = backlog.iter().any(|c| !foreground.contains(&c.0));

        if conditions.background_permitted() {
            if let Some(&chat_id) = backlog.iter().find(|c| !foreground.contains(&c.0)) {
                return Ok(BackfillStep::AdvanceHistory {
                    chat_id,
                    priority: BackfillPriority::Background,
                });
            }
        } else if has_background {
            // There is background work, but the device conditions defer it.
            let reason = match conditions.network {
                NetworkState::Metered => IdleReason::Metered,
                _ => IdleReason::PowerSaving,
            };
            return Ok(BackfillStep::Idle { reason });
        }

        Ok(BackfillStep::Idle {
            reason: IdleReason::BacklogDrained,
        })
    }

    /// Whether Archive-Mode eager media backfill may run now (POL-2/DEC-014).
    ///
    /// Metadata-first: suspended while any history remains for the scope.
    /// Quota-exempt: never consults the cache quota, only physical disk and
    /// device conditions. Opens one read transaction.
    pub fn media_policy(
        &self,
        store: &mut StateStore,
        scope: AccountScope,
        conditions: HostConditions,
        now_ms: i64,
    ) -> Result<MediaPolicy, EngineError> {
        if matches!(conditions.network, NetworkState::Offline) {
            return Ok(MediaPolicy::Suspended {
                reason: MediaSuspend::Offline,
            });
        }

        let tx = store.read_txn()?;
        let control = tx
            .backfill_control(scope)?
            .unwrap_or_else(|| BackfillControlRecord::fresh(now_ms));

        if control.paused {
            return Ok(suspend(MediaSuspend::Paused));
        }
        if let Some((_, WaitReason::FloodWait)) = pace::gate(&control, now_ms) {
            return Ok(suspend(MediaSuspend::FloodWait));
        }

        let archive_on = tx
            .account(scope.account)?
            .is_some_and(|account| account.archive_mode);
        if !archive_on {
            return Ok(suspend(MediaSuspend::ArchiveModeOff));
        }

        // Disk warnings before device conditions: the disk headline is what
        // POL-2 requires the app to surface for Archive Mode.
        match conditions.disk {
            DiskState::Critical => return Ok(suspend(MediaSuspend::DiskCritical)),
            DiskState::Low => return Ok(suspend(MediaSuspend::DiskLow)),
            DiskState::Ample => {}
        }
        match conditions.network {
            NetworkState::Metered => return Ok(suspend(MediaSuspend::Metered)),
            NetworkState::Offline => return Ok(suspend(MediaSuspend::Offline)),
            NetworkState::Online => {}
        }
        if matches!(conditions.power, PowerState::Saving) {
            return Ok(suspend(MediaSuspend::PowerSaving));
        }

        // Metadata-first: any incomplete history holds eager media.
        if !tx.backfill_backlog(&scope, 1)?.is_empty() {
            return Ok(suspend(MediaSuspend::MetadataPending));
        }

        Ok(MediaPolicy::Eager)
    }

    /// Arm the request spacer after the host issues a provider request for a
    /// planned step (SEC-031). Clears an already-elapsed flood deadline so
    /// the observation stays clean. Opens one write transaction.
    pub fn note_dispatch(
        &self,
        store: &mut StateStore,
        scope: AccountScope,
        now_ms: i64,
    ) -> Result<(), EngineError> {
        self.update_control(store, scope, now_ms, |control| {
            control.next_request_at_ms = Some(self.config.pace.next_request_after(now_ms));
            if control
                .flood_wait_until_ms
                .is_some_and(|until| until <= now_ms)
            {
                control.flood_wait_until_ms = None;
            }
        })
    }

    /// Record a retryable flood wait the host observed (NFR-033, SEC-031).
    ///
    /// Arms the durable flood deadline (honoring the stated delay, or a
    /// conservative fallback when none was stated) and reports whether the
    /// step's `attempt` count has exhausted the budget and should be
    /// abandoned. `attempt` is the source machine's own per-request counter.
    /// Opens one write transaction.
    pub fn note_flood_wait(
        &self,
        store: &mut StateStore,
        scope: AccountScope,
        retry_after_ms: Option<i64>,
        attempt: u32,
        now_ms: i64,
    ) -> Result<FloodOutcome, EngineError> {
        let until_ms = self.config.pace.flood_deadline(retry_after_ms, now_ms);
        self.update_control(store, scope, now_ms, |control| {
            // Never rewind an already-later flood deadline.
            control.flood_wait_until_ms = Some(match control.flood_wait_until_ms {
                Some(existing) => existing.max(until_ms),
                None => until_ms,
            });
        })?;
        Ok(FloodOutcome {
            until_ms,
            exhausted: self.config.pace.attempt_exhausted(attempt),
        })
    }

    /// Durably set or clear the user pause switch — the task AC's user-pausable
    /// requirement, held as durable resumable state (SYNC-043 cancellation
    /// leaves resumable state; SYNC-005 long work becomes durable). Opens one
    /// write transaction.
    pub fn set_paused(
        &self,
        store: &mut StateStore,
        scope: AccountScope,
        paused: bool,
        now_ms: i64,
    ) -> Result<(), EngineError> {
        self.update_control(store, scope, now_ms, |control| {
            control.paused = paused;
        })
    }

    /// A point-in-time observation of the scope's backfill state (the AC's
    /// observability). Opens one read transaction.
    pub fn observe(
        &self,
        store: &mut StateStore,
        scope: AccountScope,
        now_ms: i64,
    ) -> Result<BackfillObservation, EngineError> {
        let tx = store.read_txn()?;
        let control = tx
            .backfill_control(scope)?
            .unwrap_or_else(|| BackfillControlRecord::fresh(now_ms));
        let backlog = tx.backfill_backlog(&scope, self.config.backlog_scan)?;
        let pending_until_ms = pace::effective_deadline(&control).filter(|&until| until > now_ms);
        Ok(BackfillObservation {
            paused: control.paused,
            next_request_at_ms: control.next_request_at_ms,
            flood_wait_until_ms: control.flood_wait_until_ms,
            pending_until_ms,
            history_backlog: backlog.len(),
            backlog_capped: backlog.len() as u32 >= self.config.backlog_scan,
        })
    }

    /// Read-modify-write one control record in a single immediate
    /// transaction — the race-free read-modify-write the pacer/pause
    /// mutations share.
    fn update_control(
        &self,
        store: &mut StateStore,
        scope: AccountScope,
        now_ms: i64,
        mutate: impl FnOnce(&mut BackfillControlRecord),
    ) -> Result<(), EngineError> {
        let tx = store.write_txn()?;
        let mut control = tx
            .read()
            .backfill_control(scope)?
            .unwrap_or_else(|| BackfillControlRecord::fresh(now_ms));
        mutate(&mut control);
        control.updated_at_ms = now_ms;
        tx.put_backfill_control(scope, &control)?;
        tx.commit()?;
        Ok(())
    }
}

fn suspend(reason: MediaSuspend) -> MediaPolicy {
    MediaPolicy::Suspended { reason }
}

/// Whether `chat_id` still needs history: never anchored (no sync row) or
/// anchored but not yet back to the beginning (SYNC-021).
fn chat_needs_history(
    tx: &gramdrive_state::ReadTxn<'_>,
    scope: AccountScope,
    chat_id: ChatId,
) -> Result<bool, EngineError> {
    let key = ChatKey { scope, chat_id };
    Ok(match tx.chat_sync_state(&key)? {
        None => true,
        Some(record) => !record.history_complete,
    })
}
