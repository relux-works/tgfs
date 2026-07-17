//! Incremental render planning (TASK-260715-22l8zy; SYNC-024, SYNC-030..033).
//!
//! The planner turns a batch of normalized changes into exactly the set of
//! generated documents that went stale — the whole-chat NDJSON and only the
//! transcripts of the touched months — and into a plan against the current
//! event watermark that skips anything already current. These tests pin the
//! affected-partition mapping, the timezone-correct month boundaries, the
//! staleness verdicts (new, version bump, dirty, watermark behind), and the
//! interrupt/resume behaviour the planner inherits from the watermark protocol:
//! a regeneration that never publishes leaves the previous version readable and
//! the work on the durable worklist.

// clippy.toml exempts test code on the grounds that a panicking test is just a
// failing test. That exemption keys on `#[test]` functions, and the shared
// fixture helpers below sit at module level in an integration-test binary. The
// rationale still applies in full — this file links into no product artifact —
// so the exemption is restated here, matching the established test-suite pattern.
#![allow(clippy::expect_used, clippy::panic)]

use gramdrive_engine::model::identity::{
    AccountId, AccountKey, AccountScope, ChatId, ChatKey, ContentHash, DocFormat, DocPartition,
    GeneratedDocKey, ItemId, NamespaceVersion, SchemaFamily,
};
use gramdrive_engine::model::version::{ContentVersion, MetadataVersion};
use gramdrive_engine::render::markdown::UtcOffset;
use gramdrive_engine::render_plan::{
    DocClass, RenderReason, affected_documents, dirty_affected, plan_for_changes, plan_worklist,
};
use gramdrive_engine::state::repo::{
    AccountRecord, ChatRecord, ChatType, FileFacts, ItemAvailability, ItemRecord, MessageChange,
    MessageRevision, RenderOutput, RenderStateRecord, RetentionMode, SourceKind,
};
use gramdrive_engine::state::{StateStore, WriteTxn};

const ACCOUNT_ID: i64 = 7;
const NAMESPACE: u32 = 1;
const CHAT: i64 = 100;

// Fixed instants, each with an independently known civil month (UTC).
const JULY_15: i64 = 1_784_116_800_000; // 2026-07-15 12:00Z
const JULY_1: i64 = 1_782_907_200_000; // 2026-07-01 12:00Z
const AUGUST_3: i64 = 1_785_758_400_000; // 2026-08-03 12:00Z
const DECEMBER_31: i64 = 1_798_718_400_000; // 2026-12-31 12:00Z
const FEB_2025: i64 = 1_739_188_800_000; // 2025-02-10 12:00Z
const JULY_END_2330Z: i64 = 1_785_540_600_000; // 2026-07-31 23:30Z
const JUNE_END_2230Z: i64 = 1_782_858_600_000; // 2026-06-30 22:30Z

fn scope() -> AccountScope {
    AccountScope {
        account: AccountKey {
            account_id: AccountId(ACCOUNT_ID),
        },
        namespace_version: NamespaceVersion(NAMESPACE),
    }
}

fn chat_key() -> ChatKey {
    ChatKey {
        scope: scope(),
        chat_id: ChatId(CHAT),
    }
}

fn ndjson_key() -> GeneratedDocKey {
    DocClass::Ndjson.document_key(chat_key(), DocPartition::Chat)
}

fn month_key(year: u16, month: u8) -> GeneratedDocKey {
    DocClass::MarkdownMonth.document_key(chat_key(), DocPartition::Month { year, month })
}

fn ndjson_id() -> ItemId {
    DocClass::Ndjson.document_id(chat_key(), DocPartition::Chat)
}

fn month_id(year: u16, month: u8) -> ItemId {
    DocClass::MarkdownMonth.document_id(chat_key(), DocPartition::Month { year, month })
}

fn account_root_id() -> ItemId {
    use gramdrive_engine::model::identity::{CanonicalKey, ItemKey};
    ItemKey::Canonical(CanonicalKey::Account(scope().account)).id()
}

fn metadata(text: &str) -> MetadataVersion {
    MetadataVersion::new(text).expect("valid metadata version")
}

/// Seeds the account, chat, and account-root item — enough for the change log
/// and watermark reads; generated-document items are projected per test.
fn base_store() -> StateStore {
    let mut store = StateStore::open_in_memory().expect("open");
    let tx = store.write_txn().expect("write");
    tx.upsert_account(&AccountRecord {
        account: scope().account,
        source_kind: SourceKind::LocalTdlib,
        display_name: "Test Account".to_owned(),
        auth_state: "authorized".to_owned(),
        namespace_version: scope().namespace_version,
        retention_mode: RetentionMode::Mirror,
        archive_mode: false,
        secret_ref: None,
        created_at_ms: 1_000,
        updated_at_ms: 1_000,
    })
    .expect("account");
    tx.upsert_chat(&ChatRecord {
        key: chat_key(),
        chat_type: ChatType::Private,
        title: format!("Chat {CHAT}"),
        username: None,
        is_protected: false,
        archive_mode: false,
        metadata_version: metadata("m1"),
        left_at_ms: None,
        deleted_at_ms: None,
        last_update_at_ms: None,
    })
    .expect("chat");
    tx.upsert_item(&ItemRecord {
        id: account_root_id(),
        parent: None,
        display_name: "Root".to_owned(),
        safe_name: "Root".to_owned(),
        metadata_version: metadata("m1"),
        content: None,
        availability: ItemAvailability::Fetchable,
        created_at_ms: None,
        modified_at_ms: None,
        deleted_at_ms: None,
    })
    .expect("root");
    tx.commit().expect("commit");
    store
}

/// Projects a generated-document item under the account root, so its render
/// state can be created (the item foreign key) — the tree builder's job in the
/// real system.
fn project_doc(tx: &WriteTxn<'_>, id: &ItemId, name: &str) {
    tx.upsert_item(&ItemRecord {
        id: id.clone(),
        parent: Some(account_root_id()),
        display_name: name.to_owned(),
        safe_name: name.to_owned(),
        metadata_version: metadata("m1"),
        content: Some(FileFacts::default()),
        availability: ItemAvailability::Fetchable,
        created_at_ms: None,
        modified_at_ms: None,
        deleted_at_ms: None,
    })
    .expect("project doc");
}

fn observe(message: i64, sent_at_ms: i64) -> MessageChange {
    MessageChange::Observed(MessageRevision {
        message_id: gramdrive_engine::model::identity::MessageId(message),
        sender_id: Some(500),
        sent_at_ms,
        edited_at_ms: None,
        observed_at_ms: sent_at_ms + 5,
        payload_schema: SchemaFamily(1),
        payload: format!("payload-{message}").into_bytes(),
    })
}

fn edit(message: i64, sent_at_ms: i64, edited_at_ms: i64) -> MessageChange {
    MessageChange::Observed(MessageRevision {
        message_id: gramdrive_engine::model::identity::MessageId(message),
        sender_id: Some(500),
        sent_at_ms,
        edited_at_ms: Some(edited_at_ms),
        observed_at_ms: edited_at_ms + 5,
        payload_schema: SchemaFamily(1),
        payload: format!("payload-{message}-edit").into_bytes(),
    })
}

fn delete(message: i64, observed_at_ms: i64) -> MessageChange {
    MessageChange::Deleted {
        message_id: gramdrive_engine::model::identity::MessageId(message),
        observed_at_ms,
    }
}

/// Applies a change batch to the chat's event log in its own transaction and
/// returns the resulting watermark.
fn apply(store: &mut StateStore, changes: &[MessageChange]) -> i64 {
    let tx = store.write_txn().expect("write");
    tx.apply_message_changes(&chat_key(), changes)
        .expect("apply changes");
    let watermark = tx.read().latest_event_seq(&chat_key()).expect("watermark");
    tx.commit().expect("commit");
    watermark
}

/// The chat's current event watermark.
fn watermark(store: &mut StateStore) -> i64 {
    let tx = store.read_txn().expect("read");
    tx.latest_event_seq(&chat_key()).expect("watermark")
}

/// Publishes a complete render output for a document at `watermark`, as a real
/// renderer would after building the bytes — returns whether it landed clean.
fn publish(store: &mut StateStore, id: &ItemId, watermark: i64, token: &str) -> bool {
    let tx = store.write_txn().expect("write");
    let outcome = tx
        .publish_render(
            id,
            &chat_key(),
            watermark,
            &RenderOutput {
                content_version: ContentVersion::new(token).expect("valid token"),
                content_hash: Some(ContentHash::Sha256([1u8; 32])),
                logical_size: 128,
            },
            10_000 + watermark,
        )
        .expect("publish");
    tx.commit().expect("commit");
    outcome.clean
}

fn render_state(store: &mut StateStore, id: &ItemId) -> Option<RenderStateRecord> {
    let tx = store.read_txn().expect("read");
    tx.render_state(id).expect("render state")
}

/// Documents as an order-independent set of their opaque bytes — the worklist's
/// drain order is unspecified, so membership is what tests assert.
fn doc_set(ids: &[ItemId]) -> std::collections::BTreeSet<Vec<u8>> {
    ids.iter().map(|id| id.as_bytes().to_vec()).collect()
}

// ---------------------------------------------------------------------------
// Affected-document mapping — the pure "only affected partitions" core.
// ---------------------------------------------------------------------------

#[test]
fn a_change_batch_maps_to_the_whole_chat_ndjson_and_its_months() {
    let chat = chat_key();

    // No changes regenerate nothing.
    assert!(affected_documents(chat, &[], UtcOffset::UTC).is_empty());

    // One July message: the lossless NDJSON plus the July transcript, only.
    assert_eq!(
        affected_documents(chat, &[JULY_15], UtcOffset::UTC),
        vec![ndjson_key(), month_key(2026, 7)],
    );

    // Two messages in the same month collapse to one transcript.
    assert_eq!(
        affected_documents(chat, &[JULY_15, JULY_1], UtcOffset::UTC),
        vec![ndjson_key(), month_key(2026, 7)],
    );

    // Distinct months each get their own transcript, in ascending order, and
    // the NDJSON appears exactly once regardless of how many months moved.
    assert_eq!(
        affected_documents(chat, &[AUGUST_3, JULY_15, DECEMBER_31], UtcOffset::UTC),
        vec![
            ndjson_key(),
            month_key(2026, 7),
            month_key(2026, 8),
            month_key(2026, 12),
        ],
    );

    // Ascending order holds across years.
    assert_eq!(
        affected_documents(chat, &[DECEMBER_31, FEB_2025], UtcOffset::UTC),
        vec![ndjson_key(), month_key(2025, 2), month_key(2026, 12)],
    );
}

#[test]
fn month_boundaries_follow_the_render_timezone() {
    let chat = chat_key();
    let plus3 = UtcOffset::from_seconds(3 * 3_600).expect("offset");

    // 2026-07-31 23:30Z is July at UTC, but +03:00 tips it into August — the
    // planner picks the transcript the renderer would group it under.
    assert_eq!(
        affected_documents(chat, &[JULY_END_2330Z], UtcOffset::UTC),
        vec![ndjson_key(), month_key(2026, 7)],
    );
    assert_eq!(
        affected_documents(chat, &[JULY_END_2330Z], plus3),
        vec![ndjson_key(), month_key(2026, 8)],
    );

    // 2026-06-30 22:30Z is June at UTC, July at +03:00.
    assert_eq!(
        affected_documents(chat, &[JUNE_END_2230Z], UtcOffset::UTC),
        vec![ndjson_key(), month_key(2026, 6)],
    );
    assert_eq!(
        affected_documents(chat, &[JUNE_END_2230Z], plus3),
        vec![ndjson_key(), month_key(2026, 7)],
    );
}

// ---------------------------------------------------------------------------
// Planning against real state.
// ---------------------------------------------------------------------------

#[test]
fn plan_for_changes_reports_only_affected_partitions_as_new() {
    let mut store = base_store();
    let watermark = apply(&mut store, &[observe(1, JULY_15)]);

    let tx = store.read_txn().expect("read");
    let plan = plan_for_changes(&tx, chat_key(), &[JULY_15], UtcOffset::UTC).expect("plan");
    drop(tx);

    // Exactly the two affected documents, both never rendered, at the current
    // watermark — and no unrelated month.
    assert_eq!(plan.len(), 2);
    let ndjson = &plan.jobs[0];
    assert_eq!(ndjson.document, ndjson_id());
    assert_eq!(ndjson.format, DocFormat::Ndjson);
    assert_eq!(ndjson.partition, DocPartition::Chat);
    assert_eq!(ndjson.reason, RenderReason::New);
    assert_eq!(ndjson.target_watermark_seq, watermark);

    let july = &plan.jobs[1];
    assert_eq!(july.document, month_id(2026, 7));
    assert_eq!(july.format, DocFormat::Markdown);
    assert_eq!(
        july.partition,
        DocPartition::Month {
            year: 2026,
            month: 7
        }
    );
    assert_eq!(july.reason, RenderReason::New);

    // The content version each job carries is the format's token at the target
    // watermark — what the published bytes will bear.
    assert_eq!(
        ndjson.content_version.as_str(),
        gramdrive_engine::render::ndjson::content_version_token(watermark),
    );
    assert_eq!(
        july.content_version.as_str(),
        gramdrive_engine::render::markdown::content_version_token(watermark),
    );

    assert!(plan.jobs.iter().all(|job| job.partition
        != DocPartition::Month {
            year: 2026,
            month: 8
        }));
}

#[test]
fn dirty_affected_feeds_the_worklist_and_a_clean_publish_converges() {
    let mut store = base_store();
    let watermark = apply(&mut store, &[observe(1, JULY_15)]);

    // Project the two affected documents and mark them, as the applier would in
    // the same transaction as the change.
    let tx = store.write_txn().expect("write");
    project_doc(&tx, &ndjson_id(), "messages.ndjson");
    project_doc(&tx, &month_id(2026, 7), "07.md");
    let marked = dirty_affected(&tx, chat_key(), &[JULY_15], UtcOffset::UTC).expect("mark");
    tx.commit().expect("commit");
    assert_eq!(marked, vec![ndjson_id(), month_id(2026, 7)]);

    // The worklist drains to exactly those documents at the current watermark.
    let tx = store.read_txn().expect("read");
    let plan = plan_worklist(&tx, 100).expect("plan");
    drop(tx);
    assert_eq!(plan.len(), 2);
    assert!(
        plan.jobs
            .iter()
            .all(|job| job.reason == RenderReason::Dirty)
    );
    assert!(
        plan.jobs
            .iter()
            .all(|job| job.target_watermark_seq == watermark)
    );

    // Rendering and publishing each job at its watermark lands clean and clears
    // the worklist — re-planning converges to nothing.
    for job in &plan.jobs {
        assert!(publish(
            &mut store,
            &job.document,
            job.target_watermark_seq,
            job.content_version.as_str(),
        ));
    }
    let tx = store.read_txn().expect("read");
    assert!(plan_worklist(&tx, 100).expect("plan").is_empty());
    assert!(
        plan_for_changes(&tx, chat_key(), &[JULY_15], UtcOffset::UTC)
            .expect("plan")
            .is_empty()
    );
}

#[test]
fn edits_and_deletes_regenerate_only_their_month() {
    let mut store = base_store();
    // Two messages in different months, both rendered current.
    apply(&mut store, &[observe(1, JULY_15), observe(2, AUGUST_3)]);
    let tx = store.write_txn().expect("write");
    project_doc(&tx, &ndjson_id(), "messages.ndjson");
    project_doc(&tx, &month_id(2026, 7), "07.md");
    project_doc(&tx, &month_id(2026, 8), "08.md");
    dirty_affected(&tx, chat_key(), &[JULY_15, AUGUST_3], UtcOffset::UTC).expect("mark");
    tx.commit().expect("commit");
    let seeded = watermark(&mut store);
    for id in [&ndjson_id(), &month_id(2026, 7), &month_id(2026, 8)] {
        publish(&mut store, id, seeded, "seed");
    }

    // Editing the July message dirties only July's transcript (and the
    // whole-chat NDJSON); August is untouched.
    let after_edit = apply(&mut store, &[edit(1, JULY_15, JULY_15 + 1_000)]);
    let tx = store.write_txn().expect("write");
    let marked = dirty_affected(&tx, chat_key(), &[JULY_15], UtcOffset::UTC).expect("mark");
    tx.commit().expect("commit");
    assert_eq!(marked, vec![ndjson_id(), month_id(2026, 7)]);
    assert!(
        render_state(&mut store, &month_id(2026, 7))
            .expect("state")
            .dirty
    );
    assert!(
        !render_state(&mut store, &month_id(2026, 8))
            .expect("state")
            .dirty
    );

    let tx = store.read_txn().expect("read");
    let plan = plan_worklist(&tx, 100).expect("plan");
    drop(tx);
    let planned = doc_set(
        &plan
            .jobs
            .iter()
            .map(|job| job.document.clone())
            .collect::<Vec<_>>(),
    );
    assert_eq!(planned, doc_set(&[ndjson_id(), month_id(2026, 7)]));
    assert!(
        plan.jobs
            .iter()
            .all(|job| job.target_watermark_seq == after_edit)
    );

    // Re-publish July, then delete the August message: now only August moves.
    publish(&mut store, &ndjson_id(), after_edit, "ndjson-2");
    publish(&mut store, &month_id(2026, 7), after_edit, "july-2");
    apply(&mut store, &[delete(2, AUGUST_3 + 2_000)]);
    let tx = store.write_txn().expect("write");
    let marked = dirty_affected(&tx, chat_key(), &[AUGUST_3], UtcOffset::UTC).expect("mark");
    tx.commit().expect("commit");
    assert_eq!(marked, vec![ndjson_id(), month_id(2026, 8)]);
    assert!(
        !render_state(&mut store, &month_id(2026, 7))
            .expect("state")
            .dirty
    );
    assert!(
        render_state(&mut store, &month_id(2026, 8))
            .expect("state")
            .dirty
    );
}

#[test]
fn a_new_month_is_added_without_disturbing_existing_months() {
    let mut store = base_store();
    apply(&mut store, &[observe(1, JULY_15)]);
    let tx = store.write_txn().expect("write");
    project_doc(&tx, &ndjson_id(), "messages.ndjson");
    project_doc(&tx, &month_id(2026, 7), "07.md");
    dirty_affected(&tx, chat_key(), &[JULY_15], UtcOffset::UTC).expect("mark");
    tx.commit().expect("commit");

    // July rendered current.
    let target = watermark(&mut store);
    publish(&mut store, &ndjson_id(), target, "ndjson-1");
    publish(&mut store, &month_id(2026, 7), target, "july-1");
    assert!(
        !render_state(&mut store, &month_id(2026, 7))
            .expect("state")
            .dirty
    );

    // A message in a brand-new month: its transcript is created dirty (a
    // partition change), July's render state is untouched.
    apply(&mut store, &[observe(2, AUGUST_3)]);
    let tx = store.write_txn().expect("write");
    project_doc(&tx, &month_id(2026, 8), "08.md");
    let marked = dirty_affected(&tx, chat_key(), &[AUGUST_3], UtcOffset::UTC).expect("mark");
    tx.commit().expect("commit");
    assert_eq!(marked, vec![ndjson_id(), month_id(2026, 8)]);

    // August exists and is dirty; July stays clean at its old watermark.
    let august = render_state(&mut store, &month_id(2026, 8)).expect("state");
    assert!(august.dirty);
    let july = render_state(&mut store, &month_id(2026, 7)).expect("state");
    assert!(!july.dirty);
    assert_eq!(july.input_watermark_seq, target);
}

#[test]
fn a_stale_renderer_version_replans_the_document() {
    let mut store = base_store();
    let watermark = apply(&mut store, &[observe(1, JULY_15)]);

    // A document last rendered by an older renderer (version 0), published clean.
    let tx = store.write_txn().expect("write");
    project_doc(&tx, &ndjson_id(), "messages.ndjson");
    tx.ensure_render_state(&ndjson_id(), 0, DocClass::Ndjson.schema_version())
        .expect("ensure");
    tx.commit().expect("commit");
    publish(&mut store, &ndjson_id(), watermark, "old-renderer");
    assert!(!render_state(&mut store, &ndjson_id()).expect("state").dirty);

    // Even though it is clean and at the current watermark, the version gap
    // makes it stale — the planner re-plans it as a renderer upgrade.
    let tx = store.read_txn().expect("read");
    let plan = plan_for_changes(&tx, chat_key(), &[JULY_15], UtcOffset::UTC).expect("plan");
    drop(tx);
    let ndjson = plan
        .jobs
        .iter()
        .find(|job| job.document == ndjson_id())
        .expect("ndjson job");
    assert_eq!(ndjson.reason, RenderReason::RendererUpgrade);
}

#[test]
fn a_change_beyond_the_published_watermark_replans_the_document() {
    let mut store = base_store();
    let first = apply(&mut store, &[observe(1, JULY_15)]);

    // Publish the NDJSON current as of the first change, clean, at v1 versions.
    let tx = store.write_txn().expect("write");
    project_doc(&tx, &ndjson_id(), "messages.ndjson");
    tx.ensure_render_state(
        &ndjson_id(),
        DocClass::Ndjson.renderer_version(),
        DocClass::Ndjson.schema_version(),
    )
    .expect("ensure");
    tx.commit().expect("commit");
    publish(&mut store, &ndjson_id(), first, "ndjson-w1");
    assert!(!render_state(&mut store, &ndjson_id()).expect("state").dirty);

    // A later change advances the chat watermark. The document's bytes reflect
    // an older watermark, so the change-driven plan flags it even without the
    // dirty bit having been set.
    let second = apply(&mut store, &[observe(2, JULY_1)]);
    assert!(second > first);
    let tx = store.read_txn().expect("read");
    let plan = plan_for_changes(&tx, chat_key(), &[JULY_1], UtcOffset::UTC).expect("plan");
    drop(tx);
    let ndjson = plan
        .jobs
        .iter()
        .find(|job| job.document == ndjson_id())
        .expect("ndjson job");
    assert_eq!(ndjson.reason, RenderReason::WatermarkBehind);
    assert_eq!(ndjson.target_watermark_seq, second);
}

#[test]
fn an_interrupted_regeneration_keeps_the_previous_version_and_resumes() {
    let mut store = base_store();
    let first = apply(&mut store, &[observe(1, JULY_15)]);

    // First render published clean: a complete, valid file at watermark `first`.
    let tx = store.write_txn().expect("write");
    project_doc(&tx, &ndjson_id(), "messages.ndjson");
    project_doc(&tx, &month_id(2026, 7), "07.md");
    dirty_affected(&tx, chat_key(), &[JULY_15], UtcOffset::UTC).expect("mark");
    tx.commit().expect("commit");
    let v1 = gramdrive_engine::render::ndjson::content_version_token(first);
    publish(&mut store, &ndjson_id(), first, &v1);

    // A new change arrives and the document is marked dirty again; its published
    // bytes still reflect the first watermark.
    let second = apply(&mut store, &[observe(2, JULY_1)]);
    let tx = store.write_txn().expect("write");
    dirty_affected(&tx, chat_key(), &[JULY_1], UtcOffset::UTC).expect("mark");
    tx.commit().expect("commit");

    // The planner names the resume job at the new watermark.
    let tx = store.read_txn().expect("read");
    let job = plan_worklist(&tx, 100)
        .expect("plan")
        .jobs
        .into_iter()
        .find(|job| job.document == ndjson_id())
        .expect("ndjson job");
    drop(tx);
    assert_eq!(job.target_watermark_seq, second);

    // Simulate a crash during regeneration: the renderer never publishes. The
    // previous valid version is still what a reader sees, and the work is still
    // on the durable worklist.
    let state = render_state(&mut store, &ndjson_id()).expect("state");
    assert_eq!(
        state.content_version.as_ref().map(ContentVersion::as_str),
        Some(v1.as_str()),
        "the previous version stays readable after an interrupted render",
    );
    assert_eq!(state.input_watermark_seq, first);
    assert!(state.dirty, "the resume job stays on the worklist");

    // Resume: re-plan, render, publish at the target watermark. It lands clean
    // and the document converges to the new version.
    let tx = store.read_txn().expect("read");
    let resumed = plan_worklist(&tx, 100)
        .expect("plan")
        .jobs
        .into_iter()
        .find(|job| job.document == ndjson_id())
        .expect("ndjson job");
    drop(tx);
    let v2 = resumed.content_version.as_str().to_owned();
    assert!(publish(
        &mut store,
        &ndjson_id(),
        resumed.target_watermark_seq,
        &v2
    ));
    let state = render_state(&mut store, &ndjson_id()).expect("state");
    assert!(!state.dirty);
    assert_eq!(
        state.content_version.as_ref().map(ContentVersion::as_str),
        Some(v2.as_str()),
    );
    // The resumed document has converged and left the worklist (the July
    // transcript that shares its months is a separate, still-pending job).
    let tx = store.read_txn().expect("read");
    assert!(
        plan_worklist(&tx, 100)
            .expect("plan")
            .jobs
            .iter()
            .all(|job| job.document != ndjson_id()),
    );
}

#[test]
fn a_render_that_races_newer_events_stays_on_the_worklist() {
    let mut store = base_store();
    let first = apply(&mut store, &[observe(1, JULY_15)]);
    let tx = store.write_txn().expect("write");
    project_doc(&tx, &ndjson_id(), "messages.ndjson");
    project_doc(&tx, &month_id(2026, 7), "07.md");
    dirty_affected(&tx, chat_key(), &[JULY_15], UtcOffset::UTC).expect("mark");
    tx.commit().expect("commit");

    // A second change lands while the renderer is working from `first`.
    let second = apply(&mut store, &[observe(2, JULY_1)]);

    // Publishing at the stale watermark is accepted but does not clear the dirty
    // bit: events beyond it exist, so the document is re-planned at the newer
    // watermark rather than silently claiming to reflect them (SYNC-024).
    let clean = publish(
        &mut store,
        &ndjson_id(),
        first,
        &gramdrive_engine::render::ndjson::content_version_token(first),
    );
    assert!(!clean, "a raced publish must not land clean");
    let tx = store.read_txn().expect("read");
    let job = plan_worklist(&tx, 100)
        .expect("plan")
        .jobs
        .into_iter()
        .find(|job| job.document == ndjson_id())
        .expect("ndjson job");
    drop(tx);
    assert_eq!(job.target_watermark_seq, second);
}

#[test]
fn the_catalog_ids_match_the_renderers() {
    // The planner's document ids must be byte-identical to the renderers' own,
    // or a plan would target a different item than the one it renders.
    assert_eq!(
        DocClass::Ndjson
            .document_id(chat_key(), DocPartition::Chat)
            .as_bytes(),
        gramdrive_engine::render::ndjson::document_id(chat_key(), DocPartition::Chat).as_bytes(),
    );
    let month = DocPartition::Month {
        year: 2026,
        month: 7,
    };
    assert_eq!(
        DocClass::MarkdownMonth
            .document_id(chat_key(), month)
            .as_bytes(),
        gramdrive_engine::render::markdown::document_id(chat_key(), month).as_bytes(),
    );

    // A format the planner has no renderer for (chat.json) is not planned.
    let json = GeneratedDocKey {
        chat: chat_key(),
        partition: DocPartition::Chat,
        format: DocFormat::Json,
        schema_family: SchemaFamily(1),
    };
    assert!(DocClass::for_key(&json).is_none());
    assert_eq!(DocClass::for_key(&ndjson_key()), Some(DocClass::Ndjson));
    assert_eq!(
        DocClass::for_key(&month_key(2026, 7)),
        Some(DocClass::MarkdownMonth),
    );
}
