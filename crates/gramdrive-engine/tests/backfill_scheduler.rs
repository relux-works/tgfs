//! The metadata-first local backfill scheduler (TASK-260715-mua1ng),
//! driven end-to-end against a real state store: visible-item scheduling
//! order, device power/network/disk gating, the durable request pacer and
//! flood-wait budget, user pause, Archive-Mode eager-media policy, and
//! durability across a process restart (POL-2/DEC-014, SYNC-020/021,
//! SEC-031, NFR-033, NFR-031, SYNC-070).

// clippy.toml exempts test code on the grounds that a panicking test is just
// a failing test. That exemption keys on `#[test]` functions, and the shared
// fixture helpers below sit at module level in an integration-test binary.
// The rationale still applies in full — this file links into no product
// artifact — so the exemption is restated here, matching the established
// test-suite pattern.
#![allow(clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use gramdrive_engine::backfill::{
    BackfillConfig, BackfillDemand, BackfillPriority, BackfillScheduler, BackfillStep, DiskState,
    HostConditions, IdleReason, MediaPolicy, MediaSuspend, NetworkState, PaceConfig, PowerState,
    WaitReason,
};
use gramdrive_engine::model::identity::{
    AccountId, AccountKey, AccountScope, ChatId, ChatKey, MessageId, NamespaceVersion,
};
use gramdrive_engine::model::version::MetadataVersion;
use gramdrive_engine::state::StateStore;
use gramdrive_engine::state::repo::{
    AccountRecord, ChatRecord, ChatSyncRecord, ChatType, RetentionMode, SourceKind, SyncWindow,
};

const ACCOUNT_ID: i64 = 7;
const NAMESPACE: u32 = 1;

fn scope() -> AccountScope {
    AccountScope {
        account: AccountKey {
            account_id: AccountId(ACCOUNT_ID),
        },
        namespace_version: NamespaceVersion(NAMESPACE),
    }
}

fn chat_key(chat: i64) -> ChatKey {
    ChatKey {
        scope: scope(),
        chat_id: ChatId(chat),
    }
}

fn metadata(text: &str) -> MetadataVersion {
    MetadataVersion::new(text).expect("valid version")
}

// ---------------------------------------------------------------------------
// Seeding
// ---------------------------------------------------------------------------

fn seed_account(store: &mut StateStore, archive_mode: bool) {
    let tx = store.write_txn().expect("write");
    tx.upsert_account(&AccountRecord {
        account: scope().account,
        source_kind: SourceKind::LocalTdlib,
        display_name: "Test Account".to_owned(),
        auth_state: "authorized".to_owned(),
        namespace_version: scope().namespace_version,
        retention_mode: RetentionMode::Mirror,
        archive_mode,
        secret_ref: None,
        created_at_ms: 1,
        updated_at_ms: 1,
    })
    .expect("account");
    tx.commit().expect("commit");
}

fn add_chat(store: &mut StateStore, chat: i64) {
    let tx = store.write_txn().expect("write");
    tx.upsert_chat(&ChatRecord {
        key: chat_key(chat),
        chat_type: ChatType::Private,
        title: format!("Chat {chat}"),
        username: None,
        is_protected: false,
        archive_mode: false,
        metadata_version: metadata("m1"),
        left_at_ms: None,
        deleted_at_ms: None,
        last_update_at_ms: None,
    })
    .expect("chat");
    tx.commit().expect("commit");
}

/// Records a chat's history-traversal state. `history_complete` decides
/// whether the chat still needs history; `last_sync_at_ms` orders the
/// background backlog (older first).
fn set_sync(store: &mut StateStore, chat: i64, history_complete: bool, last_sync_at_ms: i64) {
    let tx = store.write_txn().expect("write");
    tx.record_chat_sync(
        &chat_key(chat),
        &ChatSyncRecord {
            window: Some(SyncWindow {
                oldest: MessageId(10),
                newest: MessageId(20),
            }),
            history_complete,
            last_sync_at_ms: Some(last_sync_at_ms),
        },
    )
    .expect("sync");
    tx.commit().expect("commit");
}

fn conditions(network: NetworkState, power: PowerState, disk: DiskState) -> HostConditions {
    HostConditions {
        network,
        power,
        disk,
    }
}

fn demand<'a>(visible: &'a [ChatId], requested: &'a [ChatId]) -> BackfillDemand<'a> {
    BackfillDemand { visible, requested }
}

fn scheduler() -> BackfillScheduler {
    BackfillScheduler::with_defaults()
}

// ---------------------------------------------------------------------------
// Scheduling order (task description, visible-item priority): visible >
// requested > background
// ---------------------------------------------------------------------------

#[test]
fn schedules_visible_then_requested_then_background_then_drains() {
    let mut store = StateStore::open_in_memory().expect("open");
    seed_account(&mut store, false);
    for chat in [1, 2, 3] {
        add_chat(&mut store, chat);
    }
    // All three still need history; chat 3 is the least-recently synced, so
    // it leads the background backlog.
    set_sync(&mut store, 1, false, 300);
    set_sync(&mut store, 2, false, 200);
    set_sync(&mut store, 3, false, 100);

    let sched = scheduler();
    let cond = HostConditions::UNCONSTRAINED;
    let visible = [ChatId(1)];
    let requested = [ChatId(2)];

    // Visible wins even though chat 3 is older in the backlog.
    assert_eq!(
        sched
            .plan_next(
                &mut store,
                scope(),
                demand(&visible, &requested),
                cond,
                1_000
            )
            .expect("plan"),
        BackfillStep::AdvanceHistory {
            chat_id: ChatId(1),
            priority: BackfillPriority::Visible,
        },
    );

    // With the visible chat's history complete, the requested chat is next.
    set_sync(&mut store, 1, true, 300);
    assert_eq!(
        sched
            .plan_next(
                &mut store,
                scope(),
                demand(&visible, &requested),
                cond,
                1_000
            )
            .expect("plan"),
        BackfillStep::AdvanceHistory {
            chat_id: ChatId(2),
            priority: BackfillPriority::Requested,
        },
    );

    // With both foreground chats complete, background falls to the
    // least-recently-synced remaining chat (chat 3).
    set_sync(&mut store, 2, true, 200);
    assert_eq!(
        sched
            .plan_next(
                &mut store,
                scope(),
                demand(&visible, &requested),
                cond,
                1_000
            )
            .expect("plan"),
        BackfillStep::AdvanceHistory {
            chat_id: ChatId(3),
            priority: BackfillPriority::Background,
        },
    );

    // Everything complete: the backlog is drained.
    set_sync(&mut store, 3, true, 100);
    assert_eq!(
        sched
            .plan_next(
                &mut store,
                scope(),
                demand(&visible, &requested),
                cond,
                1_000
            )
            .expect("plan"),
        BackfillStep::Idle {
            reason: IdleReason::BacklogDrained,
        },
    );
}

#[test]
fn a_visible_chat_never_synced_still_schedules() {
    let mut store = StateStore::open_in_memory().expect("open");
    seed_account(&mut store, false);
    add_chat(&mut store, 42);
    // No chat_sync_state row at all — a chat the user opened before any
    // crawl anchored it. It still needs history.
    let sched = scheduler();
    let visible = [ChatId(42)];
    assert_eq!(
        sched
            .plan_next(
                &mut store,
                scope(),
                demand(&visible, &[]),
                HostConditions::UNCONSTRAINED,
                1_000,
            )
            .expect("plan"),
        BackfillStep::AdvanceHistory {
            chat_id: ChatId(42),
            priority: BackfillPriority::Visible,
        },
    );
}

// ---------------------------------------------------------------------------
// Device power / network gating
// ---------------------------------------------------------------------------

#[test]
fn background_metadata_defers_on_metered_and_power_saving_but_foreground_runs() {
    let mut store = StateStore::open_in_memory().expect("open");
    seed_account(&mut store, false);
    add_chat(&mut store, 1);
    set_sync(&mut store, 1, false, 100);
    let sched = scheduler();

    // Metered, no foreground demand: background metadata defers.
    assert_eq!(
        sched
            .plan_next(
                &mut store,
                scope(),
                BackfillDemand::NONE,
                conditions(
                    NetworkState::Metered,
                    PowerState::Unconstrained,
                    DiskState::Ample
                ),
                1_000,
            )
            .expect("plan"),
        BackfillStep::Idle {
            reason: IdleReason::Metered,
        },
    );

    // Power saving, no foreground demand: background metadata defers.
    assert_eq!(
        sched
            .plan_next(
                &mut store,
                scope(),
                BackfillDemand::NONE,
                conditions(NetworkState::Online, PowerState::Saving, DiskState::Ample),
                1_000,
            )
            .expect("plan"),
        BackfillStep::Idle {
            reason: IdleReason::PowerSaving,
        },
    );

    // The very same metered condition still serves a visible chat: a user
    // is waiting on foreground work.
    let visible = [ChatId(1)];
    assert_eq!(
        sched
            .plan_next(
                &mut store,
                scope(),
                demand(&visible, &[]),
                conditions(NetworkState::Metered, PowerState::Saving, DiskState::Ample),
                1_000,
            )
            .expect("plan"),
        BackfillStep::AdvanceHistory {
            chat_id: ChatId(1),
            priority: BackfillPriority::Visible,
        },
    );
}

#[test]
fn offline_schedules_nothing() {
    let mut store = StateStore::open_in_memory().expect("open");
    seed_account(&mut store, false);
    add_chat(&mut store, 1);
    set_sync(&mut store, 1, false, 100);
    let sched = scheduler();
    let visible = [ChatId(1)];
    // Even a visible chat cannot be served with no connectivity.
    assert_eq!(
        sched
            .plan_next(
                &mut store,
                scope(),
                demand(&visible, &[]),
                conditions(
                    NetworkState::Offline,
                    PowerState::Unconstrained,
                    DiskState::Ample
                ),
                1_000,
            )
            .expect("plan"),
        BackfillStep::Idle {
            reason: IdleReason::Offline,
        },
    );
}

// ---------------------------------------------------------------------------
// Pacer: request spacing and the flood-wait budget (SEC-031, NFR-033)
// ---------------------------------------------------------------------------

#[test]
fn dispatch_arms_the_request_spacer() {
    let mut store = StateStore::open_in_memory().expect("open");
    seed_account(&mut store, false);
    add_chat(&mut store, 1);
    set_sync(&mut store, 1, false, 100);
    let sched = scheduler();
    let spacing = sched.config().pace.min_spacing_ms;
    let visible = [ChatId(1)];

    // The host issues the request for the planned step at t = 1000.
    sched
        .note_dispatch(&mut store, scope(), 1_000)
        .expect("dispatch");

    // Until the spacer elapses, the next plan is a spacing wait.
    assert_eq!(
        sched
            .plan_next(
                &mut store,
                scope(),
                demand(&visible, &[]),
                HostConditions::UNCONSTRAINED,
                1_000,
            )
            .expect("plan"),
        BackfillStep::Wait {
            until_ms: 1_000 + spacing,
            reason: WaitReason::Spacing,
        },
    );

    // At the spacer deadline, work flows again.
    assert_eq!(
        sched
            .plan_next(
                &mut store,
                scope(),
                demand(&visible, &[]),
                HostConditions::UNCONSTRAINED,
                1_000 + spacing,
            )
            .expect("plan"),
        BackfillStep::AdvanceHistory {
            chat_id: ChatId(1),
            priority: BackfillPriority::Visible,
        },
    );
}

#[test]
fn flood_wait_holds_all_work_until_the_stated_deadline() {
    let mut store = StateStore::open_in_memory().expect("open");
    seed_account(&mut store, false);
    add_chat(&mut store, 1);
    set_sync(&mut store, 1, false, 100);
    let sched = scheduler();
    let visible = [ChatId(1)];

    // A Telegram 429 states a 300-second wait at t = 1000, attempt 1.
    let outcome = sched
        .note_flood_wait(&mut store, scope(), Some(300_000), 1, 1_000)
        .expect("flood");
    assert_eq!(outcome.until_ms, 301_000);
    assert!(!outcome.exhausted, "attempt 1 is within budget");

    // Even a visible chat waits through a flood wait (NFR-033).
    assert_eq!(
        sched
            .plan_next(
                &mut store,
                scope(),
                demand(&visible, &[]),
                HostConditions::UNCONSTRAINED,
                1_000,
            )
            .expect("plan"),
        BackfillStep::Wait {
            until_ms: 301_000,
            reason: WaitReason::FloodWait,
        },
    );

    // Once the wait elapses, work resumes.
    assert_eq!(
        sched
            .plan_next(
                &mut store,
                scope(),
                demand(&visible, &[]),
                HostConditions::UNCONSTRAINED,
                301_000,
            )
            .expect("plan"),
        BackfillStep::AdvanceHistory {
            chat_id: ChatId(1),
            priority: BackfillPriority::Visible,
        },
    );
}

#[test]
fn flood_wait_budget_exhausts_after_the_configured_attempts() {
    let mut store = StateStore::open_in_memory().expect("open");
    seed_account(&mut store, false);
    let sched = scheduler();
    let budget = sched.config().pace.attempt_budget;

    // At the budget the step may retry; one past it, the step is abandoned.
    let within = sched
        .note_flood_wait(&mut store, scope(), None, budget, 1_000)
        .expect("flood");
    assert!(!within.exhausted);
    let past = sched
        .note_flood_wait(&mut store, scope(), None, budget + 1, 1_000)
        .expect("flood");
    assert!(past.exhausted, "one attempt past budget is abandoned");
}

#[test]
fn an_unstated_flood_wait_uses_the_fallback_floor() {
    let mut store = StateStore::open_in_memory().expect("open");
    seed_account(&mut store, false);
    let sched = scheduler();
    let fallback = sched.config().pace.fallback_backoff_ms;
    let outcome = sched
        .note_flood_wait(&mut store, scope(), None, 1, 1_000)
        .expect("flood");
    assert_eq!(outcome.until_ms, 1_000 + fallback);
}

#[test]
fn a_later_flood_wait_never_shortens_an_earlier_one() {
    let mut store = StateStore::open_in_memory().expect("open");
    seed_account(&mut store, false);
    let sched = scheduler();
    // A long wait, then a short one reported before it elapses: the longer
    // deadline stands.
    sched
        .note_flood_wait(&mut store, scope(), Some(300_000), 1, 1_000)
        .expect("long");
    sched
        .note_flood_wait(&mut store, scope(), Some(1_000), 2, 1_500)
        .expect("short");
    let obs = sched.observe(&mut store, scope(), 1_500).expect("observe");
    assert_eq!(obs.flood_wait_until_ms, Some(301_000));
}

// ---------------------------------------------------------------------------
// User pause (task AC user-pausable; SYNC-043/SYNC-005 durable resumable state)
// ---------------------------------------------------------------------------

#[test]
fn pause_halts_all_work_and_resume_restores_it() {
    let mut store = StateStore::open_in_memory().expect("open");
    seed_account(&mut store, false);
    add_chat(&mut store, 1);
    set_sync(&mut store, 1, false, 100);
    let sched = scheduler();
    let visible = [ChatId(1)];

    sched
        .set_paused(&mut store, scope(), true, 1_000)
        .expect("pause");
    assert_eq!(
        sched
            .plan_next(
                &mut store,
                scope(),
                demand(&visible, &[]),
                HostConditions::UNCONSTRAINED,
                1_000,
            )
            .expect("plan"),
        BackfillStep::Paused,
    );
    assert!(
        sched
            .observe(&mut store, scope(), 1_000)
            .expect("observe")
            .paused
    );

    sched
        .set_paused(&mut store, scope(), false, 2_000)
        .expect("resume");
    assert_eq!(
        sched
            .plan_next(
                &mut store,
                scope(),
                demand(&visible, &[]),
                HostConditions::UNCONSTRAINED,
                2_000,
            )
            .expect("plan"),
        BackfillStep::AdvanceHistory {
            chat_id: ChatId(1),
            priority: BackfillPriority::Visible,
        },
    );
}

// ---------------------------------------------------------------------------
// Archive-Mode eager-media policy (POL-2/DEC-014)
// ---------------------------------------------------------------------------

#[test]
fn media_stays_off_demand_when_archive_mode_is_off() {
    let mut store = StateStore::open_in_memory().expect("open");
    seed_account(&mut store, false);
    let sched = scheduler();
    assert_eq!(
        sched
            .media_policy(&mut store, scope(), HostConditions::UNCONSTRAINED, 1_000)
            .expect("policy"),
        MediaPolicy::Suspended {
            reason: MediaSuspend::ArchiveModeOff,
        },
    );
}

#[test]
fn archive_media_is_eager_only_after_metadata_is_complete() {
    let mut store = StateStore::open_in_memory().expect("open");
    seed_account(&mut store, true);
    add_chat(&mut store, 1);
    // History still incomplete: metadata-first holds eager media back.
    set_sync(&mut store, 1, false, 100);
    let sched = scheduler();
    assert_eq!(
        sched
            .media_policy(&mut store, scope(), HostConditions::UNCONSTRAINED, 1_000)
            .expect("policy"),
        MediaPolicy::Suspended {
            reason: MediaSuspend::MetadataPending,
        },
    );

    // Once history is complete, eager media is permitted.
    set_sync(&mut store, 1, true, 100);
    assert_eq!(
        sched
            .media_policy(&mut store, scope(), HostConditions::UNCONSTRAINED, 1_000)
            .expect("policy"),
        MediaPolicy::Eager,
    );
}

#[test]
fn archive_media_honors_disk_warnings_and_device_conditions() {
    let mut store = StateStore::open_in_memory().expect("open");
    seed_account(&mut store, true);
    add_chat(&mut store, 1);
    set_sync(&mut store, 1, true, 100); // metadata complete
    let sched = scheduler();

    // Disk warnings take precedence — the POL-2 headline for Archive Mode.
    assert_eq!(
        sched
            .media_policy(
                &mut store,
                scope(),
                conditions(
                    NetworkState::Online,
                    PowerState::Unconstrained,
                    DiskState::Critical
                ),
                1_000,
            )
            .expect("policy"),
        MediaPolicy::Suspended {
            reason: MediaSuspend::DiskCritical,
        },
    );
    assert_eq!(
        sched
            .media_policy(
                &mut store,
                scope(),
                conditions(
                    NetworkState::Online,
                    PowerState::Unconstrained,
                    DiskState::Low
                ),
                1_000,
            )
            .expect("policy"),
        MediaPolicy::Suspended {
            reason: MediaSuspend::DiskLow,
        },
    );

    // No eager media on a metered link — the "no eager mobile media" rule.
    assert_eq!(
        sched
            .media_policy(
                &mut store,
                scope(),
                conditions(
                    NetworkState::Metered,
                    PowerState::Unconstrained,
                    DiskState::Ample
                ),
                1_000,
            )
            .expect("policy"),
        MediaPolicy::Suspended {
            reason: MediaSuspend::Metered,
        },
    );

    // No eager media while saving power.
    assert_eq!(
        sched
            .media_policy(
                &mut store,
                scope(),
                conditions(NetworkState::Online, PowerState::Saving, DiskState::Ample),
                1_000,
            )
            .expect("policy"),
        MediaPolicy::Suspended {
            reason: MediaSuspend::PowerSaving,
        },
    );
}

#[test]
fn archive_media_honors_pause_and_flood_and_offline() {
    let mut store = StateStore::open_in_memory().expect("open");
    seed_account(&mut store, true);
    add_chat(&mut store, 1);
    set_sync(&mut store, 1, true, 100);
    let sched = scheduler();

    // Offline.
    assert_eq!(
        sched
            .media_policy(
                &mut store,
                scope(),
                conditions(
                    NetworkState::Offline,
                    PowerState::Unconstrained,
                    DiskState::Ample
                ),
                1_000,
            )
            .expect("policy"),
        MediaPolicy::Suspended {
            reason: MediaSuspend::Offline,
        },
    );

    // A flood wait suspends eager media too.
    sched
        .note_flood_wait(&mut store, scope(), Some(300_000), 1, 1_000)
        .expect("flood");
    assert_eq!(
        sched
            .media_policy(&mut store, scope(), HostConditions::UNCONSTRAINED, 1_000)
            .expect("policy"),
        MediaPolicy::Suspended {
            reason: MediaSuspend::FloodWait,
        },
    );

    // Pause dominates once set.
    sched
        .set_paused(&mut store, scope(), true, 2_000)
        .expect("pause");
    assert_eq!(
        sched
            .media_policy(&mut store, scope(), HostConditions::UNCONSTRAINED, 400_000)
            .expect("policy"),
        MediaPolicy::Suspended {
            reason: MediaSuspend::Paused,
        },
    );
}

// ---------------------------------------------------------------------------
// Observability
// ---------------------------------------------------------------------------

#[test]
fn observe_reports_pause_pending_deadline_and_backlog() {
    let mut store = StateStore::open_in_memory().expect("open");
    seed_account(&mut store, false);
    for chat in [1, 2] {
        add_chat(&mut store, chat);
        set_sync(&mut store, chat, false, 100);
    }
    let sched = scheduler();

    let fresh = sched.observe(&mut store, scope(), 1_000).expect("observe");
    assert!(!fresh.paused);
    assert_eq!(fresh.pending_until_ms, None);
    assert_eq!(fresh.history_backlog, 2);
    assert!(!fresh.backlog_capped);

    sched
        .note_flood_wait(&mut store, scope(), Some(300_000), 1, 1_000)
        .expect("flood");
    let waiting = sched.observe(&mut store, scope(), 1_000).expect("observe");
    assert_eq!(waiting.flood_wait_until_ms, Some(301_000));
    assert_eq!(waiting.pending_until_ms, Some(301_000));
}

// ---------------------------------------------------------------------------
// Durability across a process restart (NFR-031, SYNC-070, NFR-033)
// ---------------------------------------------------------------------------

#[test]
fn pause_and_flood_wait_survive_a_restart() {
    let db = TempDb::new();
    let sched = scheduler();

    // One process pauses backfill and records a flood wait, then dies.
    {
        let mut store = StateStore::open(&db.path).expect("open");
        seed_account(&mut store, false);
        add_chat(&mut store, 1);
        set_sync(&mut store, 1, false, 100);
        sched
            .note_flood_wait(&mut store, scope(), Some(300_000), 1, 1_000)
            .expect("flood");
        sched
            .set_paused(&mut store, scope(), true, 1_000)
            .expect("pause");
        // store drops here — the crash.
    }

    // The next process reopens the same file: the pause and the flood
    // deadline are both still in force. A restart must not resume paused
    // work, nor forget a flood wait and re-hammer the account.
    let mut store = StateStore::open(&db.path).expect("reopen");
    let visible = [ChatId(1)];
    assert_eq!(
        sched
            .plan_next(
                &mut store,
                scope(),
                demand(&visible, &[]),
                HostConditions::UNCONSTRAINED,
                1_000,
            )
            .expect("plan"),
        BackfillStep::Paused,
    );

    // After resume, the durable flood wait still holds until its deadline.
    sched
        .set_paused(&mut store, scope(), false, 1_000)
        .expect("resume");
    assert_eq!(
        sched
            .plan_next(
                &mut store,
                scope(),
                demand(&visible, &[]),
                HostConditions::UNCONSTRAINED,
                1_000,
            )
            .expect("plan"),
        BackfillStep::Wait {
            until_ms: 301_000,
            reason: WaitReason::FloodWait,
        },
    );
}

// ---------------------------------------------------------------------------
// Custom tuning
// ---------------------------------------------------------------------------

#[test]
fn custom_pace_config_is_honored() {
    let mut store = StateStore::open_in_memory().expect("open");
    seed_account(&mut store, false);
    add_chat(&mut store, 1);
    set_sync(&mut store, 1, false, 100);
    let sched = BackfillScheduler::new(BackfillConfig {
        pace: PaceConfig {
            min_spacing_ms: 1_000,
            fallback_backoff_ms: 5_000,
            attempt_budget: 2,
        },
        backlog_scan: 8,
    });
    let visible = [ChatId(1)];

    sched
        .note_dispatch(&mut store, scope(), 1_000)
        .expect("dispatch");
    assert_eq!(
        sched
            .plan_next(
                &mut store,
                scope(),
                demand(&visible, &[]),
                HostConditions::UNCONSTRAINED,
                1_000,
            )
            .expect("plan"),
        BackfillStep::Wait {
            until_ms: 2_000,
            reason: WaitReason::Spacing,
        },
    );
    assert!(
        sched
            .note_flood_wait(&mut store, scope(), None, 3, 1_000)
            .expect("flood")
            .exhausted,
        "attempt 3 exceeds the custom budget of 2",
    );
}

// ---------------------------------------------------------------------------
// A unique temp database path per test, cleaned up on drop (mirrors the
// transfer-machine suite's crash-resume fixture).
// ---------------------------------------------------------------------------

struct TempDb {
    path: PathBuf,
}

impl TempDb {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "gramdrive-backfill-test-{}-{n}.sqlite3",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        Self { path }
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let mut name = self.path.as_os_str().to_owned();
            name.push(suffix);
            let _ = std::fs::remove_file(PathBuf::from(name));
        }
    }
}
