//! End-to-end monthly snapshot composition, atomic staging, and publication.

#![allow(clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use gramdrive_engine::model::identity::{
    AccountId, AccountKey, AccountScope, AppearanceKey, AttachmentIndex, CanonicalKey, ChatId,
    ChatKey, ChatListKey, ChatListKind, DocFormat, DocPartition, GeneratedDocKey, ItemId, ItemKey,
    MessageId, MonthDirKey, NamespaceVersion, SchemaFamily,
};
use gramdrive_engine::model::version::MetadataVersion;
use gramdrive_engine::render::markdown::{
    Attachment, AttachmentFidelity, Availability, DisplayTimeZone, Entity, MediaKind, MessageBody,
    Reaction, ServiceAction, TelegramRepresentation,
};
use gramdrive_engine::render_pipeline::{
    DecodedRevision, GeneratedFileLease, MessagePayloadDecoder, RenderPipelineError,
    compose_chat_metadata, compose_month, publish_chat_metadata, publish_month,
    stage_chat_metadata, stage_month,
};
use gramdrive_engine::state::StateStore;
use gramdrive_engine::state::repo::{
    AccountRecord, ChatRecord, ChatType, FileFacts, ItemAvailability, ItemRecord, MessageChange,
    MessageEventKind, MessagePayload, MessageRevision, RetentionMode, SourceKind,
};

fn scope() -> AccountScope {
    AccountScope {
        account: AccountKey {
            account_id: AccountId(17),
        },
        namespace_version: NamespaceVersion(2),
    }
}

fn chat() -> ChatKey {
    ChatKey {
        scope: scope(),
        chat_id: ChatId(900),
    }
}

fn version(text: &str) -> MetadataVersion {
    MetadataVersion::new(text).expect("version")
}

fn appearance(item: CanonicalKey) -> ItemId {
    appearance_in(ChatListKind::Main, item)
}

fn appearance_in(view: ChatListKind, item: CanonicalKey) -> ItemId {
    ItemKey::Appearance(AppearanceKey { view, item }).id()
}

fn doc(format: DocFormat) -> ItemId {
    appearance(CanonicalKey::GeneratedDoc(GeneratedDocKey {
        chat: chat(),
        partition: DocPartition::Month {
            year: 2026,
            month: 7,
        },
        format,
        schema_family: SchemaFamily(1),
    }))
}

fn chat_doc() -> ItemId {
    appearance(CanonicalKey::GeneratedDoc(GeneratedDocKey {
        chat: chat(),
        partition: DocPartition::Chat,
        format: DocFormat::Json,
        schema_family: gramdrive_engine::render::chat_json::CHAT_SCHEMA_FAMILY,
    }))
}

fn item(id: ItemId, parent: Option<ItemId>, name: &str, file: bool) -> ItemRecord {
    ItemRecord {
        aggregate_size: None,
        id,
        parent,
        display_name: name.to_owned(),
        safe_name: name.to_owned(),
        metadata_version: version("m1"),
        content: file.then(FileFacts::default),
        availability: ItemAvailability::Fetchable,
        created_at_ms: None,
        modified_at_ms: None,
        deleted_at_ms: None,
    }
}

fn seeded_store() -> StateStore {
    let mut store = StateStore::open_in_memory().expect("open");
    let tx = store.write_txn().expect("write");
    tx.upsert_account(&AccountRecord {
        account: scope().account,
        source_kind: SourceKind::LocalTdlib,
        display_name: "Account".to_owned(),
        auth_state: "authorized".to_owned(),
        namespace_version: scope().namespace_version,
        display_timezone: "Asia/Tbilisi".to_owned(),
        retention_mode: RetentionMode::Audit,
        archive_mode: false,
        secret_ref: None,
        created_at_ms: 1,
        updated_at_ms: 1,
    })
    .expect("account");
    tx.upsert_chat(&ChatRecord {
        key: chat(),
        chat_type: ChatType::Private,
        title: "Chat".to_owned(),
        username: None,
        is_protected: false,
        archive_mode: false,
        metadata_version: version("m1"),
        left_at_ms: None,
        deleted_at_ms: None,
        last_update_at_ms: None,
    })
    .expect("chat");
    let root = ItemKey::Canonical(CanonicalKey::Account(scope().account)).id();
    let list = ItemKey::Canonical(CanonicalKey::ChatList(ChatListKey {
        scope: scope(),
        kind: ChatListKind::Main,
    }))
    .id();
    let chat_dir = appearance(CanonicalKey::Chat(chat()));
    let month = appearance(CanonicalKey::MonthDir(MonthDirKey {
        chat: chat(),
        year: 2026,
        month: 7,
    }));
    tx.upsert_item(&item(root.clone(), None, "Account", false))
        .expect("root");
    tx.upsert_item(&item(list.clone(), Some(root), "Chats", false))
        .expect("list");
    tx.upsert_item(&item(chat_dir.clone(), Some(list), "Chat", false))
        .expect("chat item");
    tx.upsert_item(&item(
        chat_doc(),
        Some(chat_dir.clone()),
        ".chat.json",
        true,
    ))
    .expect("chat json");
    tx.upsert_item(&item(month.clone(), Some(chat_dir), "2026-07", false))
        .expect("month");
    tx.upsert_item(&item(
        doc(DocFormat::Markdown),
        Some(month.clone()),
        "Messages.md",
        true,
    ))
    .expect("markdown");
    tx.upsert_item(&item(
        doc(DocFormat::Ndjson),
        Some(month),
        "Messages.ndjson",
        true,
    ))
    .expect("ndjson");
    tx.commit().expect("commit");
    store
}

#[test]
fn chat_metadata_is_privacy_bounded_and_publishes_stable_fetch_facts() {
    let mut store = seeded_store();
    let source = store
        .read_txn()
        .expect("read")
        .chat(&chat())
        .expect("chat query")
        .expect("chat");
    let rendered = compose_chat_metadata(&source).expect("compose");
    let text = std::str::from_utf8(&rendered.bytes).expect("UTF-8 JSON");
    assert!(text.contains("\"title\":\"Chat\""));
    assert!(text.contains("\"type\":\"private\""));
    for forbidden in [
        "account_id",
        "chat_id",
        "namespace",
        "authorization",
        "secret",
        "path",
        "message",
    ] {
        assert!(!text.contains(forbidden));
    }

    let root = temp_root();
    let _ = std::fs::remove_dir_all(&root);
    let staged = stage_chat_metadata(&root, &rendered).expect("stage");
    assert_eq!(
        stage_chat_metadata(&root, &rendered).expect("stage replay"),
        staged
    );
    let publication =
        publish_chat_metadata(&mut store, &rendered, &staged, 5_000).expect("publish");
    assert_eq!(publication.published_items, 1);

    let first_modified = {
        let read = store.read_txn().expect("read");
        let item = read.item(&chat_doc()).expect("item query").expect("item");
        let facts = item.content.expect("file facts");
        assert_eq!(facts.mime_type.as_deref(), Some("application/json"));
        assert_eq!(
            facts.logical_size,
            Some(u64::try_from(rendered.bytes.len()).expect("size"))
        );
        assert_eq!(
            facts.content_version.as_ref(),
            Some(&rendered.content_version)
        );
        let cache = read
            .cache_entry(&chat_doc())
            .expect("cache query")
            .expect("cache");
        assert_eq!(cache.materialization_ref.as_deref(), staged.path.to_str());
        let render = read
            .render_state(&chat_doc())
            .expect("render query")
            .expect("render");
        assert!(!render.dirty);
        assert_eq!(
            render.content_version.as_ref(),
            Some(&rendered.content_version)
        );
        item.modified_at_ms
    };
    publish_chat_metadata(&mut store, &rendered, &staged, 9_000).expect("replay publication");
    assert_eq!(
        store
            .read_txn()
            .expect("read")
            .item(&chat_doc())
            .expect("item query")
            .expect("item")
            .modified_at_ms,
        first_modified,
        "an equal content generation keeps its modification date"
    );

    let tx = store.write_txn().expect("write");
    let mut changed = source;
    changed.title = "Changed".to_owned();
    changed.metadata_version = version("m2");
    tx.upsert_chat(&changed).expect("chat update");
    tx.commit().expect("commit");
    let error = publish_chat_metadata(&mut store, &rendered, &staged, 10_000)
        .expect_err("stale metadata must not publish");
    assert!(matches!(error, RenderPipelineError::MetadataChanged));
    let changed_rendered = compose_chat_metadata(&changed).expect("compose changed metadata");
    let changed_staged =
        stage_chat_metadata(&root, &changed_rendered).expect("stage changed metadata");
    publish_chat_metadata(&mut store, &changed_rendered, &changed_staged, 11_000)
        .expect("publish changed metadata");
    assert!(changed_staged.path.exists());
    assert!(
        !staged.path.exists(),
        "the replaced unclaimed chat generation is reclaimed immediately"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn publication_defers_generated_version_churn_until_the_hydration_lease_releases() {
    let root = temp_root();
    let mut store = seeded_store();
    let source = store
        .read_txn()
        .expect("read")
        .chat(&chat())
        .expect("chat")
        .expect("source");
    let first = compose_chat_metadata(&source).expect("compose first");
    let first_staged = stage_chat_metadata(&root, &first).expect("stage first");
    publish_chat_metadata(&mut store, &first, &first_staged, 5_000).expect("publish first");
    let first_bytes = std::fs::read(&first_staged.path).expect("first bytes");
    let lease = GeneratedFileLease::acquire(&first_staged.path).expect("claim staged file");
    let first_version = first.content_version.clone();

    let tx = store.write_txn().expect("write");
    let mut changed = source;
    changed.title = "Changed while File Provider clones".to_owned();
    changed.metadata_version = version("m2");
    tx.upsert_chat(&changed).expect("chat update");
    tx.commit().expect("commit");
    let replacement = compose_chat_metadata(&changed).expect("compose replacement");
    let replacement_staged = stage_chat_metadata(&root, &replacement).expect("stage replacement");
    let error = publish_chat_metadata(&mut store, &replacement, &replacement_staged, 6_000)
        .expect_err("an active File Provider clone defers version publication");
    assert!(matches!(error, RenderPipelineError::PublicationLeased));

    assert!(
        first_staged.path.exists(),
        "the active hand-off path survives deferred publication"
    );
    assert_eq!(
        std::fs::read(&first_staged.path).expect("leased bytes"),
        first_bytes,
        "the clone source remains exact while its lease is active"
    );
    let still_published = store
        .read_txn()
        .expect("read deferred publication")
        .cache_entry(&chat_doc())
        .expect("cache query")
        .expect("published cache row");
    assert_eq!(still_published.content_version, first_version);
    assert_eq!(
        still_published.materialization_ref.as_deref(),
        first_staged.path.to_str(),
        "the provider-visible generation must not move during its clone"
    );

    drop(lease);
    publish_chat_metadata(&mut store, &replacement, &replacement_staged, 7_000)
        .expect("publish replacement after hand-off");
    assert!(
        !first_staged.path.exists(),
        "the first post-release publication reclaims the obsolete generation"
    );
    assert!(
        replacement_staged.path.exists(),
        "the published generation remains claimed"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[derive(Debug)]
struct Decoder;

impl MessagePayloadDecoder for Decoder {
    type Error = &'static str;

    fn decode(&self, payload: &MessagePayload) -> Result<DecodedRevision, Self::Error> {
        let text = std::str::from_utf8(&payload.bytes).map_err(|_| "utf8")?;
        let attachments = if text == "edited" {
            vec![Attachment {
                index: AttachmentIndex(0),
                media_kind: MediaKind::Document,
                telegram_representation: TelegramRepresentation::OriginalDocument,
                fidelity: AttachmentFidelity::Original,
                source_name: Some("source.pdf".to_owned()),
                mime_type: Some("application/pdf".to_owned()),
                exact_size: Some(123),
                availability: Availability::Fetchable,
                content_hash: None,
                media_name: Some("2026-07-15 16-00-00 source.pdf".to_owned()),
            }]
        } else {
            Vec::new()
        };
        Ok(DecodedRevision {
            edited_at_ms: (text == "edited").then_some(1_784_116_801_000),
            body: MessageBody {
                text: Some(text.to_owned()),
                entities: Vec::<Entity>::new(),
                reply_to: None,
                thread_top: None,
                topic_id: None,
                album_id: None,
                reactions: Vec::<Reaction>::new(),
                attachments,
                service: None::<ServiceAction>,
                protected: false,
            },
        })
    }
}

fn observe(payload: &[u8], edited_at_ms: Option<i64>) -> MessageChange {
    MessageChange::Observed(MessageRevision {
        message_id: MessageId(44),
        sender_id: Some(77),
        sent_at_ms: 1_784_116_800_000,
        edited_at_ms,
        observed_at_ms: edited_at_ms.unwrap_or(1_784_116_800_500),
        payload_schema: SchemaFamily(1),
        payload: payload.to_vec(),
    })
}

fn temp_root() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "gramdrive-render-pipeline-{}-{sequence}",
        std::process::id()
    ))
}

#[test]
fn one_snapshot_replays_identically_and_publishes_the_pair_atomically() {
    let mut store = seeded_store();
    let tx = store.write_txn().expect("write");
    tx.apply_message_changes(
        &chat(),
        &[
            observe(b"original", None),
            observe(b"edited", Some(1_784_116_801_000)),
        ],
    )
    .expect("changes");
    tx.commit().expect("commit");

    let timezone = DisplayTimeZone::named("Asia/Tbilisi").expect("timezone");
    let (start_ms, end_ms) = timezone.month_bounds_ms(2026, 7).expect("bounds");
    let snapshot = {
        let tx = store.read_txn().expect("read");
        tx.month_render_snapshot(chat(), start_ms, end_ms)
            .expect("snapshot")
    };
    let first = compose_month(&snapshot, 2026, 7, &Decoder).expect("compose");
    let replay = compose_month(&snapshot, 2026, 7, &Decoder).expect("replay");
    assert_eq!(first, replay);
    let markdown = String::from_utf8(first.markdown.clone()).expect("markdown utf8");
    let ndjson = String::from_utf8(first.ndjson.clone()).expect("ndjson utf8");
    assert!(markdown.contains("timezone: Asia/Tbilisi"));
    assert!(markdown.contains(&format!(
        "input_watermark_seq: {}",
        snapshot.input_watermark_seq
    )));
    assert!(ndjson.contains(&format!(
        "\"input_watermark_seq\":{}",
        snapshot.input_watermark_seq
    )));
    assert!(ndjson.contains("\"date_ms\":1784116800000"));
    assert!(ndjson.contains("\"state\":\"superseded\""));
    assert!(ndjson.contains("\"telegram_representation\":\"original_document\""));
    assert!(ndjson.contains("\"fidelity\":\"original\""));

    // A newer event in another month must not make July's publication dirty.
    let tx = store.write_txn().expect("write");
    tx.apply_message_changes(
        &chat(),
        &[MessageChange::Observed(MessageRevision {
            message_id: MessageId(45),
            sender_id: Some(77),
            sent_at_ms: 1_785_758_400_000,
            edited_at_ms: None,
            observed_at_ms: 1_785_758_400_500,
            payload_schema: SchemaFamily(1),
            payload: b"august".to_vec(),
        })],
    )
    .expect("unrelated change");
    tx.commit().expect("commit unrelated");

    let root = temp_root();
    let _ = std::fs::remove_dir_all(&root);
    let staged = stage_month(&root, &snapshot, &first).expect("stage");
    assert_eq!(
        stage_month(&root, &snapshot, &replay).expect("stage replay"),
        staged
    );
    let catalog = {
        let tx = store.read_txn().expect("read");
        tx.month_render_catalog(chat(), 2026, 7).expect("catalog")
    };
    assert_eq!(catalog.len(), 2);
    let before = {
        let tx = store.read_txn().expect("read");
        tx.change_journal_state().expect("journal").latest_sequence
    };
    let publication =
        publish_month(&mut store, &snapshot, &first, &staged, 5_000).expect("publish");
    assert!(publication.clean);
    assert_eq!(publication.published_items, 2);
    let tx = store.read_txn().expect("read");
    let changes = tx
        .item_changes_since(scope().account, before, 10)
        .expect("changes");
    // The two documents, plus the two directories whose published size the
    // publication just moved: the month and its chat (BUG-260728-2qfzbd).
    assert_eq!(changes.len(), 4);
    assert_eq!(
        changes
            .iter()
            .filter(|change| change.item.aggregate_size.is_some())
            .count(),
        2,
        "exactly the two ancestor directories carry a refreshed rollup"
    );
    for entry in &catalog {
        let state = tx
            .render_state(&entry.item)
            .expect("render state")
            .expect("state");
        assert_eq!(state.input_watermark_seq, snapshot.input_watermark_seq);
        assert!(!state.dirty);
        let cache = tx
            .cache_entry(&entry.item)
            .expect("cache")
            .expect("cache row");
        assert!(cache.materialization_ref.is_some());
    }
    let published_cache = catalog
        .iter()
        .map(|entry| {
            let cache = tx
                .cache_entry(&entry.item)
                .expect("cache")
                .expect("cache row");
            (
                entry.item.clone(),
                cache.content_version,
                cache.materialization_ref,
            )
        })
        .collect::<Vec<_>>();
    drop(tx);

    let tx = store.write_txn().expect("write changed July");
    tx.apply_message_changes(&chat(), &[observe(b"second edit", Some(1_784_116_802_000))])
        .expect("changed July");
    tx.commit().expect("commit changed July");
    let changed_snapshot = store
        .read_txn()
        .expect("read changed snapshot")
        .month_render_snapshot(chat(), start_ms, end_ms)
        .expect("changed snapshot");
    let changed_rendered =
        compose_month(&changed_snapshot, 2026, 7, &Decoder).expect("compose changed month");
    let changed_staged =
        stage_month(&root, &changed_snapshot, &changed_rendered).expect("stage changed month");
    let markdown_bytes = std::fs::read(&staged.markdown).expect("published markdown bytes");
    let lease = GeneratedFileLease::acquire(&staged.markdown).expect("markdown hydration lease");
    assert!(matches!(
        publish_month(
            &mut store,
            &changed_snapshot,
            &changed_rendered,
            &changed_staged,
            6_000,
        ),
        Err(RenderPipelineError::PublicationLeased)
    ));
    {
        let tx = store.read_txn().expect("read deferred monthly publication");
        for (item, content_version, materialization_ref) in &published_cache {
            let cache = tx.cache_entry(item).expect("cache").expect("cache row");
            assert_eq!(&cache.content_version, content_version);
            assert_eq!(&cache.materialization_ref, materialization_ref);
        }
    }
    assert_eq!(
        std::fs::read(&staged.markdown).expect("leased markdown bytes"),
        markdown_bytes,
        "leasing either monthly format keeps the provider-visible pair stable"
    );
    drop(lease);
    publish_month(
        &mut store,
        &changed_snapshot,
        &changed_rendered,
        &changed_staged,
        6_000,
    )
    .expect("publish changed month");
    assert!(changed_staged.markdown.exists());
    assert!(changed_staged.ndjson.exists());
    assert!(!staged.markdown.exists());
    assert!(!staged.ndjson.exists());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn monthly_snapshot_keeps_multi_revision_delete_order_deterministic() {
    let mut store = seeded_store();
    let tx = store.write_txn().expect("write");
    tx.apply_message_changes(
        &chat(),
        &[
            observe(b"original", None),
            observe(b"first edit", Some(1_784_116_801_000)),
            observe(b"second edit", Some(1_784_116_802_000)),
            MessageChange::Deleted {
                message_id: MessageId(44),
                observed_at_ms: 1_784_116_803_000,
            },
        ],
    )
    .expect("revision history");
    tx.commit().expect("commit");

    let timezone = DisplayTimeZone::named("Asia/Tbilisi").expect("timezone");
    let (start_ms, end_ms) = timezone.month_bounds_ms(2026, 7).expect("bounds");
    let snapshot = store
        .read_txn()
        .expect("read")
        .month_render_snapshot(chat(), start_ms, end_ms)
        .expect("snapshot");
    assert_eq!(snapshot.messages.len(), 1);
    let events = &snapshot.messages[0].events;
    assert_eq!(
        events.iter().map(|event| event.kind).collect::<Vec<_>>(),
        vec![
            MessageEventKind::Observed,
            MessageEventKind::Edited,
            MessageEventKind::Edited,
            MessageEventKind::Deleted,
        ]
    );
    assert!(
        events
            .windows(2)
            .all(|pair| pair[0].event_seq < pair[1].event_seq)
    );
    assert!(events.last().expect("deletion").payload.is_none());

    let first = compose_month(&snapshot, 2026, 7, &Decoder).expect("compose");
    let replay = compose_month(&snapshot, 2026, 7, &Decoder).expect("replay");
    assert_eq!(
        first, replay,
        "ordered revisions must render deterministically"
    );
}

#[test]
fn publication_rejects_an_incomplete_live_view_loaded_inside_its_transaction() {
    let mut store = seeded_store();
    let tx = store.write_txn().expect("write");
    tx.apply_message_changes(&chat(), &[observe(b"original", None)])
        .expect("changes");
    let archive_chat = appearance_in(ChatListKind::Archive, CanonicalKey::Chat(chat()));
    let archive_month = appearance_in(
        ChatListKind::Archive,
        CanonicalKey::MonthDir(MonthDirKey {
            chat: chat(),
            year: 2026,
            month: 7,
        }),
    );
    let archive_list = ItemKey::Canonical(CanonicalKey::ChatList(ChatListKey {
        scope: scope(),
        kind: ChatListKind::Archive,
    }))
    .id();
    tx.upsert_item(&item(
        archive_list.clone(),
        Some(ItemKey::Canonical(CanonicalKey::Account(scope().account)).id()),
        "Archive",
        false,
    ))
    .expect("archive list");
    tx.upsert_item(&item(
        archive_chat.clone(),
        Some(archive_list),
        "Chat",
        false,
    ))
    .expect("archive chat");
    tx.upsert_item(&item(
        archive_month.clone(),
        Some(archive_chat),
        "2026-07",
        false,
    ))
    .expect("archive month");
    let archive_markdown = appearance_in(
        ChatListKind::Archive,
        CanonicalKey::GeneratedDoc(GeneratedDocKey {
            chat: chat(),
            partition: DocPartition::Month {
                year: 2026,
                month: 7,
            },
            format: DocFormat::Markdown,
            schema_family: SchemaFamily(1),
        }),
    );
    tx.upsert_item(&item(
        archive_markdown,
        Some(archive_month),
        "Messages.md",
        true,
    ))
    .expect("archive markdown");
    tx.commit().expect("commit");

    let timezone = DisplayTimeZone::named("Asia/Tbilisi").expect("timezone");
    let (start_ms, end_ms) = timezone.month_bounds_ms(2026, 7).expect("bounds");
    let snapshot = store
        .read_txn()
        .expect("read")
        .month_render_snapshot(chat(), start_ms, end_ms)
        .expect("snapshot");
    let rendered = compose_month(&snapshot, 2026, 7, &Decoder).expect("compose");
    let root = temp_root().join("incomplete-view");
    let _ = std::fs::remove_dir_all(&root);
    let staged = stage_month(&root, &snapshot, &rendered).expect("stage");
    let before = store
        .read_txn()
        .expect("read")
        .change_journal_state()
        .expect("journal")
        .latest_sequence;
    assert!(matches!(
        publish_month(&mut store, &snapshot, &rendered, &staged, 5_000),
        Err(RenderPipelineError::IncompleteCatalog)
    ));
    assert_eq!(
        store
            .read_txn()
            .expect("read")
            .change_journal_state()
            .expect("journal")
            .latest_sequence,
        before
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn publication_rejects_a_policy_generation_race_at_the_same_message_watermark() {
    let mut store = seeded_store();
    let tx = store.write_txn().expect("write");
    tx.apply_message_changes(&chat(), &[observe(b"original", None)])
        .expect("changes");
    tx.commit().expect("commit");

    let timezone = DisplayTimeZone::named("Asia/Tbilisi").expect("timezone");
    let (start_ms, end_ms) = timezone.month_bounds_ms(2026, 7).expect("bounds");
    let snapshot = store
        .read_txn()
        .expect("read")
        .month_render_snapshot(chat(), start_ms, end_ms)
        .expect("snapshot");
    let rendered = compose_month(&snapshot, 2026, 7, &Decoder).expect("compose");
    let root = temp_root().join("policy-race");
    let _ = std::fs::remove_dir_all(&root);
    let staged = stage_month(&root, &snapshot, &rendered).expect("stage");

    let tx = store.write_txn().expect("policy write");
    tx.set_retention_mode(
        scope().account,
        RetentionMode::Mirror,
        Some(
            gramdrive_state::repo::AuditToMirrorConfirmation::parse(
                scope().account,
                &gramdrive_state::repo::AuditToMirrorConfirmation::expected_phrase(scope().account),
            )
            .expect("confirmation"),
        ),
        4_000,
    )
    .expect("policy transition");
    tx.commit().expect("policy commit");
    assert_eq!(
        store
            .read_txn()
            .expect("read watermark")
            .latest_event_seq(&chat())
            .expect("watermark"),
        snapshot.input_watermark_seq
    );
    assert!(matches!(
        publish_month(&mut store, &snapshot, &rendered, &staged, 5_000),
        Err(RenderPipelineError::PolicyChanged)
    ));
    let _ = std::fs::remove_dir_all(&root);
}
