//! Versioned date-first content facts: timezone, attachment fidelity, stories.

#![allow(clippy::expect_used, clippy::panic)]

mod common;

use gramdrive_state::StateError;
use gramdrive_state::model::identity::{
    AttachmentIndex, AttachmentKey, ContentHash, MessageId, MessageKey, StoryAppearanceLocation,
    StoryId, StoryKey,
};
use gramdrive_state::model::version::ContentVersion;
use gramdrive_state::repo::{
    AttachmentAvailability, AttachmentFacts, AttachmentFidelity, AttachmentLogicalKind,
    AuditToMirrorConfirmation, RetentionMode, StoryAppearanceRecord, StoryArchiveEligibility,
    StoryContentLocatorRecord, StoryContentState, StoryFacts, StoryLocatorFileType, StorySyncPhase,
    TelegramRepresentation,
};

fn content_version(value: &str) -> ContentVersion {
    ContentVersion::new(value).expect("valid content version")
}

fn story_locator(key: StoryKey, version: &ContentVersion) -> StoryContentLocatorRecord {
    StoryContentLocatorRecord {
        story: key,
        role: "video-primary".to_owned(),
        file_type: StoryLocatorFileType::VideoStory,
        is_primary: true,
        local_file_id: Some(700),
        remote_file_id: Some("remote-story".to_owned()),
        remote_unique_id: Some("unique-story".to_owned()),
        size: Some(4_096),
        expected_size: Some(4_096),
        content_version: version.clone(),
    }
}

#[test]
fn account_display_timezone_round_trips_separately_from_source_timestamps() {
    let mut store = gramdrive_state::StateStore::open_in_memory().expect("open");
    let mut account = common::account_record();
    account.display_timezone = "Asia/Tbilisi".to_owned();
    let tx = store.write_txn().expect("write");
    tx.upsert_account(&account).expect("account");
    tx.commit().expect("commit");

    let read = store.read_txn().expect("read");
    let stored = read
        .account(account.account)
        .expect("read account")
        .expect("account exists");
    assert_eq!(stored.display_timezone, "Asia/Tbilisi");
}

#[test]
fn existing_account_timezone_changes_only_through_the_policy_transition() {
    let mut store = common::store_with_account();
    let mut replay = common::account_record();
    replay.display_timezone = "Asia/Tbilisi".to_owned();
    let tx = store.write_txn().expect("write replay");
    tx.upsert_account(&replay).expect("account replay");
    tx.commit().expect("commit replay");
    assert_eq!(
        store
            .read_txn()
            .expect("read")
            .display_timezone(replay.account)
            .expect("timezone")
            .as_deref(),
        Some("UTC"),
        "generic account refresh cannot bypass repartitioning"
    );

    let tx = store.write_txn().expect("write transition");
    let change = tx
        .set_display_timezone(replay.account, "Asia/Tbilisi", 2_000)
        .expect("transition");
    tx.commit().expect("commit transition");
    assert!(change.changed());
    assert_eq!(change.previous, "UTC");
    assert_eq!(change.current, "Asia/Tbilisi");
    let read = store.read_txn().expect("read transition");
    assert_eq!(
        read.display_timezone(replay.account)
            .expect("timezone")
            .as_deref(),
        Some("Asia/Tbilisi")
    );
    assert_eq!(
        read.render_generation(replay.account).expect("generation"),
        Some(1)
    );
    drop(read);

    let tx = store.write_txn().expect("write noop");
    let noop = tx
        .set_display_timezone(replay.account, "Asia/Tbilisi", 3_000)
        .expect("noop");
    tx.commit().expect("commit noop");
    assert!(!noop.changed());
    assert_eq!(
        store
            .read_txn()
            .expect("read generation")
            .render_generation(replay.account)
            .expect("generation"),
        Some(1)
    );
}

#[test]
fn attachment_kind_representation_and_fidelity_are_orthogonal() {
    let mut store = common::store_with_account();
    common::insert_chat(store.connection(), 100);
    let event = common::insert_observed_event(store.connection(), 100, 500);
    common::insert_message(store.connection(), 100, 500, event);
    let key = AttachmentKey {
        message: MessageKey {
            chat: common::chat_key(100),
            message_id: MessageId(500),
        },
        index: AttachmentIndex(0),
    };
    let facts = AttachmentFacts {
        key,
        logical_kind: AttachmentLogicalKind::Photo,
        telegram_representation: TelegramRepresentation::OriginalDocument,
        fidelity: AttachmentFidelity::Original,
        source_name: Some("sender-original.png".to_owned()),
        mime_type: Some("image/png".to_owned()),
        exact_size: Some(12_345),
        content_version: content_version("attachment-v4"),
        telegram_unique_id: Some("unique".to_owned()),
        telegram_local_file_id: Some(517),
        telegram_file_id: Some("refreshable".to_owned()),
        file_reference: Some(vec![1, 2, 3]),
        availability: AttachmentAvailability::Fetchable,
        can_be_saved: true,
    };
    let tx = store.write_txn().expect("write");
    tx.upsert_attachment(&facts).expect("attachment");
    tx.commit().expect("commit");

    let read = store.read_txn().expect("read");
    let stored = read
        .attachment(&key)
        .expect("read attachment")
        .expect("attachment exists");
    assert_eq!(stored.facts, facts);
    assert_eq!(stored.blob_hash, None);
}

#[test]
fn attachment_repository_rejects_false_processed_media_claims() {
    let mut store = common::store_with_account();
    common::insert_chat(store.connection(), 100);
    let event = common::insert_observed_event(store.connection(), 100, 501);
    common::insert_message(store.connection(), 100, 501, event);
    let base = AttachmentFacts {
        key: AttachmentKey {
            message: MessageKey {
                chat: common::chat_key(100),
                message_id: MessageId(501),
            },
            index: AttachmentIndex(0),
        },
        logical_kind: AttachmentLogicalKind::Photo,
        telegram_representation: TelegramRepresentation::Photo,
        fidelity: AttachmentFidelity::TelegramVariant,
        source_name: None,
        mime_type: Some("image/jpeg".to_owned()),
        exact_size: Some(12_345),
        content_version: content_version("processed-photo-v1"),
        telegram_unique_id: Some("unique".to_owned()),
        telegram_local_file_id: Some(518),
        telegram_file_id: Some("refreshable".to_owned()),
        file_reference: None,
        availability: AttachmentAvailability::Fetchable,
        can_be_saved: true,
    };

    let tx = store.write_txn().expect("write");
    tx.upsert_attachment(&base).expect("truthful attachment");
    for invalid in [
        AttachmentFacts {
            fidelity: AttachmentFidelity::Original,
            ..base.clone()
        },
        AttachmentFacts {
            source_name: Some("claimed-original.jpg".to_owned()),
            ..base.clone()
        },
    ] {
        assert!(matches!(
            tx.upsert_attachment(&invalid),
            Err(StateError::InvalidArgument { .. })
        ));
    }
    tx.commit().expect("commit");

    let stored = store
        .read_txn()
        .expect("read")
        .attachment(&base.key)
        .expect("attachment")
        .expect("stored");
    assert_eq!(stored.facts, base);
}

#[test]
fn current_attachment_projection_joins_the_telegram_timestamp_and_reconciles_removal() {
    let mut store = common::store_with_account();
    common::insert_chat(store.connection(), 100);
    let event = common::insert_observed_event(store.connection(), 100, 502);
    common::insert_message(store.connection(), 100, 502, event);
    let message = MessageKey {
        chat: common::chat_key(100),
        message_id: MessageId(502),
    };
    let facts = AttachmentFacts {
        key: AttachmentKey {
            message,
            index: AttachmentIndex(0),
        },
        logical_kind: AttachmentLogicalKind::Video,
        telegram_representation: TelegramRepresentation::OriginalDocument,
        fidelity: AttachmentFidelity::Original,
        source_name: Some("master.mov".to_owned()),
        mime_type: Some("video/quicktime".to_owned()),
        exact_size: Some(9_001),
        content_version: content_version("video-original-v1"),
        telegram_unique_id: Some("video-unique".to_owned()),
        telegram_local_file_id: Some(700),
        telegram_file_id: Some("video-remote".to_owned()),
        file_reference: None,
        availability: AttachmentAvailability::Fetchable,
        can_be_saved: true,
    };

    let tx = store.write_txn().expect("write");
    tx.replace_message_attachments(&message, std::slice::from_ref(&facts), 1_100)
        .expect("replace");
    tx.commit().expect("commit");

    let read = store.read_txn().expect("read");
    let projections = read
        .attachment_projections_of_chat(&message.chat)
        .expect("projection");
    assert_eq!(projections.len(), 1);
    assert_eq!(projections[0].attachment.facts, facts);
    assert_eq!(projections[0].telegram_message_timestamp_ms, 1_000);
    drop(read);

    let tx = store.write_txn().expect("write removal");
    tx.replace_message_attachments(&message, &[], 1_200)
        .expect("remove current attachment");
    tx.commit().expect("commit removal");
    assert!(
        store
            .read_txn()
            .expect("read removal")
            .attachment_projections_of_chat(&message.chat)
            .expect("projection removal")
            .is_empty()
    );
}

#[test]
fn story_transition_keeps_one_canonical_byte_link_and_no_active_copy() {
    let mut store = common::store_with_account();
    common::insert_chat(store.connection(), 100);
    let key = StoryKey {
        poster: common::chat_key(100),
        story_id: StoryId(77),
    };
    let facts = StoryFacts {
        key,
        source_timestamp_ms: 1_721_555_200_000,
        mime_type: Some("video/mp4".to_owned()),
        exact_size: Some(4096),
        content_version: content_version("story-v4"),
        availability: AttachmentAvailability::Fetchable,
        can_be_forwarded: true,
        content_state: StoryContentState::Available,
    };
    let hash = ContentHash::Sha256([0x77; 32]);
    let tx = store.write_txn().expect("write");
    tx.upsert_story_with_locators(&facts, &[story_locator(key, &facts.content_version)])
        .expect("story");
    tx.record_blob(key.poster.scope.account, &hash, 4096, 1_000)
        .expect("blob");
    tx.link_story_blob(&key, &hash, 1_100).expect("link");
    tx.set_story_appearance(&StoryAppearanceRecord {
        story: key,
        location: StoryAppearanceLocation::Active,
        display_name: "Story 77.mp4".to_owned(),
        posted_at_ms: facts.source_timestamp_ms,
        expires_at_ms: Some(facts.source_timestamp_ms + 86_400_000),
        removed_at_ms: None,
        profile_scan_generation: None,
        profile_pin_order: None,
    })
    .expect("active");
    tx.set_story_appearance(&StoryAppearanceRecord {
        story: key,
        location: StoryAppearanceLocation::Month {
            year: 2024,
            month: 7,
        },
        display_name: "2024-07-21 12-00-00 Story 77.mp4".to_owned(),
        posted_at_ms: facts.source_timestamp_ms,
        expires_at_ms: None,
        removed_at_ms: None,
        profile_scan_generation: Some(1),
        profile_pin_order: None,
    })
    .expect("persistent");
    tx.commit().expect("commit");

    let read = store.read_txn().expect("read");
    let story = read.story(&key).expect("story").expect("exists");
    assert_eq!(story.blob_hash, Some(hash));
    let appearances = read.story_appearances(&key).expect("appearances");
    assert_eq!(appearances.len(), 1);
    assert_eq!(
        appearances[0].location,
        StoryAppearanceLocation::Month {
            year: 2024,
            month: 7
        }
    );
    drop(read);

    let columns: Vec<String> = store
        .connection()
        .prepare("PRAGMA table_info(story_appearances)")
        .expect("prepare")
        .query_map([], |row| row.get(1))
        .expect("columns")
        .collect::<Result<_, _>>()
        .expect("collect");
    assert!(!columns.iter().any(|name| name.contains("blob")));
}

#[test]
fn restricted_story_can_never_link_materialized_bytes() {
    let mut store = common::store_with_account();
    common::insert_chat(store.connection(), 100);
    let key = StoryKey {
        poster: common::chat_key(100),
        story_id: StoryId(88),
    };
    let hash = ContentHash::Sha256([0x88; 32]);
    let tx = store.write_txn().expect("write");
    tx.upsert_story(&StoryFacts {
        key,
        source_timestamp_ms: 1_000,
        mime_type: None,
        exact_size: None,
        content_version: content_version("restricted-story-v4"),
        availability: AttachmentAvailability::Restricted,
        can_be_forwarded: false,
        content_state: StoryContentState::Protected,
    })
    .expect("story");
    tx.record_blob(key.poster.scope.account, &hash, 1, 1_000)
        .expect("blob");
    assert!(matches!(
        tx.link_story_blob(&key, &hash, 1_100),
        Err(StateError::InvalidArgument { .. })
    ));
}

fn story_facts(key: StoryKey, state: StoryContentState) -> StoryFacts {
    let (availability, can_be_forwarded, mime_type, exact_size) = match state {
        StoryContentState::Available => (
            AttachmentAvailability::Fetchable,
            true,
            Some("video/mp4".to_owned()),
            Some(4096),
        ),
        StoryContentState::Protected => (AttachmentAvailability::Restricted, false, None, None),
        _ => (AttachmentAvailability::Unavailable, false, None, None),
    };
    StoryFacts {
        key,
        source_timestamp_ms: 1_721_555_200_000,
        mime_type,
        exact_size,
        content_version: content_version("story-ingest-v1"),
        availability,
        can_be_forwarded,
        content_state: state,
    }
}

fn active_appearance(key: StoryKey) -> StoryAppearanceRecord {
    StoryAppearanceRecord {
        story: key,
        location: StoryAppearanceLocation::Active,
        display_name: format!("Story {}", key.story_id.0),
        posted_at_ms: 1_721_555_200_000,
        expires_at_ms: None,
        removed_at_ms: None,
        profile_scan_generation: None,
        profile_pin_order: None,
    }
}

#[test]
fn duplicate_active_replay_is_one_row_and_unprofiled_expiry_purges_it() {
    let mut store = common::store_with_account();
    common::insert_chat(store.connection(), 100);
    let chat = common::chat_key(100);
    let key = StoryKey {
        poster: chat,
        story_id: StoryId(91),
    };
    let record = (
        story_facts(key, StoryContentState::MetadataPending),
        active_appearance(key),
    );
    for _ in 0..2 {
        let tx = store.write_txn().expect("write replay");
        tx.replace_active_stories(&chat, std::slice::from_ref(&record))
            .expect("active snapshot");
        tx.commit().expect("commit replay");
    }
    assert_eq!(
        store
            .connection()
            .query_row("SELECT count(*) FROM stories", [], |row| row
                .get::<_, i64>(0))
            .expect("count"),
        1
    );

    let tx = store.write_txn().expect("write expiry");
    tx.replace_active_stories(&chat, &[])
        .expect("authoritative empty active snapshot");
    tx.commit().expect("commit expiry");
    assert!(
        store
            .read_txn()
            .expect("read")
            .story(&key)
            .expect("story")
            .is_none()
    );
}

#[test]
fn nonprofile_observation_without_active_membership_leaves_no_orphan() {
    let mut store = common::store_with_account();
    common::insert_chat(store.connection(), 100);
    let key = StoryKey {
        poster: common::chat_key(100),
        story_id: StoryId(90),
    };
    let tx = store.write_txn().expect("write observation");
    tx.upsert_story(&story_facts(key, StoryContentState::Available))
        .expect("story");
    tx.remove_profile_story(&key, RetentionMode::Mirror, 1_000)
        .expect("no profile");
    tx.commit().expect("commit");
    assert!(
        store
            .read_txn()
            .expect("read")
            .story(&key)
            .expect("story")
            .is_none()
    );
}

#[test]
fn profile_transition_survives_active_expiry_and_mirror_removal_tombstones() {
    let mut store = common::store_with_account();
    common::insert_chat(store.connection(), 100);
    let chat = common::chat_key(100);
    let key = StoryKey {
        poster: chat,
        story_id: StoryId(92),
    };
    let tx = store.write_txn().expect("write transition");
    tx.replace_active_stories(
        &chat,
        &[(
            story_facts(key, StoryContentState::Available),
            active_appearance(key),
        )],
    )
    .expect("active");
    tx.set_story_appearance(&StoryAppearanceRecord {
        story: key,
        location: StoryAppearanceLocation::Month {
            year: 2024,
            month: 7,
        },
        display_name: "Story 92.mp4".to_owned(),
        posted_at_ms: 1_721_555_200_000,
        expires_at_ms: None,
        removed_at_ms: None,
        profile_scan_generation: Some(4),
        profile_pin_order: None,
    })
    .expect("profile");
    tx.commit().expect("commit transition");

    let tx = store.write_txn().expect("write active expiry");
    tx.replace_active_stories(&chat, &[])
        .expect("active expiry");
    tx.commit().expect("commit active expiry");
    let read = store.read_txn().expect("read persistent");
    assert!(read.story(&key).expect("story").is_some());
    assert_eq!(read.story_appearances(&key).expect("appearances").len(), 1);
    drop(read);

    let tx = store.write_txn().expect("write profile removal");
    tx.finish_profile_scan(&chat, 5, RetentionMode::Mirror, 2_000)
        .expect("finish newer empty scan");
    tx.commit().expect("commit profile removal");
    let read = store.read_txn().expect("read tombstone");
    assert!(read.story(&key).expect("story").is_none());
    assert!(
        read.story_tombstone(&key)
            .expect("tombstone")
            .expect("exists")
            .had_profile
    );
}

#[test]
fn protection_flip_destructively_removes_metadata_and_blob_link() {
    let mut store = common::store_with_account();
    common::insert_chat(store.connection(), 100);
    let key = StoryKey {
        poster: common::chat_key(100),
        story_id: StoryId(93),
    };
    let hash = ContentHash::Sha256([0x93; 32]);
    let tx = store.write_txn().expect("write allowed");
    let allowed = story_facts(key, StoryContentState::Available);
    let locator = story_locator(key, &allowed.content_version);
    tx.upsert_story_with_locators(&allowed, std::slice::from_ref(&locator))
        .expect("allowed story");
    tx.upsert_story_with_locators(&allowed, std::slice::from_ref(&locator))
        .expect("duplicate replay");
    tx.record_blob(key.poster.scope.account, &hash, 4096, 1_000)
        .expect("blob");
    tx.link_story_blob(&key, &hash, 1_100).expect("link");
    tx.commit().expect("allowed commit");
    let allowed_state = store
        .read_txn()
        .expect("read allowed")
        .story(&key)
        .expect("story")
        .expect("exists");
    assert_eq!(allowed_state.locators, vec![locator]);
    assert_eq!(allowed_state.blob_hash, Some(hash));

    let tx = store.write_txn().expect("write protected");
    tx.upsert_story(&story_facts(key, StoryContentState::Protected))
        .expect("protected authority");
    tx.commit().expect("protected commit");
    let protected = store
        .read_txn()
        .expect("read")
        .story(&key)
        .expect("story")
        .expect("exists");
    assert_eq!(protected.facts.content_state, StoryContentState::Protected);
    assert_eq!(protected.facts.mime_type, None);
    assert_eq!(protected.facts.exact_size, None);
    assert_eq!(protected.blob_hash, None);
    assert!(protected.locators.is_empty());
}

#[test]
fn profile_pin_order_round_trips_reorders_and_survives_unordered_live_replay() {
    let mut store = common::store_with_account();
    common::insert_chat(store.connection(), 100);
    let chat = common::chat_key(100);
    let first = StoryKey {
        poster: chat,
        story_id: StoryId(101),
    };
    let second = StoryKey {
        poster: chat,
        story_id: StoryId(102),
    };
    let tx = store.write_txn().expect("write first page");
    for (key, pin_order) in [(first, 0), (second, 1)] {
        let facts = story_facts(key, StoryContentState::Available);
        tx.upsert_story_with_locators(&facts, &[story_locator(key, &facts.content_version)])
            .expect("story");
        tx.set_story_appearance(&StoryAppearanceRecord {
            story: key,
            location: StoryAppearanceLocation::Month {
                year: 2024,
                month: 7,
            },
            display_name: format!("Story {}", key.story_id.0),
            posted_at_ms: facts.source_timestamp_ms,
            expires_at_ms: None,
            removed_at_ms: None,
            profile_scan_generation: Some(3),
            profile_pin_order: Some(pin_order),
        })
        .expect("appearance");
    }
    tx.commit().expect("commit first page");

    let tx = store.write_txn().expect("write unordered live replay");
    tx.set_story_appearance(&StoryAppearanceRecord {
        story: first,
        location: StoryAppearanceLocation::Month {
            year: 2024,
            month: 7,
        },
        display_name: "Story 101 live".to_owned(),
        posted_at_ms: 1_721_555_200_000,
        expires_at_ms: None,
        removed_at_ms: None,
        profile_scan_generation: None,
        profile_pin_order: None,
    })
    .expect("live replay");
    tx.commit().expect("commit live replay");
    assert_eq!(
        store
            .read_txn()
            .expect("read live replay")
            .story_appearances(&first)
            .expect("appearances")[0]
            .profile_pin_order,
        Some(0)
    );

    let tx = store.write_txn().expect("write reordered page");
    tx.clear_profile_pin_order(&chat).expect("clear old order");
    for (key, pin_order) in [(second, 0), (first, 1)] {
        tx.set_story_appearance(&StoryAppearanceRecord {
            story: key,
            location: StoryAppearanceLocation::Month {
                year: 2024,
                month: 7,
            },
            display_name: format!("Story {} reordered", key.story_id.0),
            posted_at_ms: 1_721_555_200_000,
            expires_at_ms: None,
            removed_at_ms: None,
            profile_scan_generation: Some(4),
            profile_pin_order: Some(pin_order),
        })
        .expect("reordered appearance");
    }
    tx.commit().expect("commit reordered page");
    let read = store.read_txn().expect("read reordered page");
    assert_eq!(
        read.story_appearances(&second).expect("second")[0].profile_pin_order,
        Some(0)
    );
    assert_eq!(
        read.story_appearances(&first).expect("first")[0].profile_pin_order,
        Some(1)
    );
}

#[test]
fn account_story_list_progress_is_bounded_durable_and_restarts_by_generation() {
    let mut store = common::store_with_account();
    let scope = common::chat_key(100).scope;
    let initial = store
        .read_txn()
        .expect("read initial")
        .story_list_progress(scope)
        .expect("progress")
        .expect("seeded");
    assert_eq!(initial.generation, 0);
    assert_eq!(initial.pages_loaded, 0);
    assert!(!initial.complete);

    let tx = store.write_txn().expect("start pass");
    tx.start_story_list_pass(scope, 1_000).expect("start");
    tx.commit().expect("commit start");
    let tx = store.write_txn().expect("page");
    tx.advance_story_list_progress(scope, false, 1_100)
        .expect("bounded page");
    tx.commit().expect("commit page");
    let tx = store.write_txn().expect("exhaustion");
    tx.advance_story_list_progress(scope, true, 1_200)
        .expect("exhaustion");
    tx.commit().expect("commit exhaustion");
    let exhausted = store
        .read_txn()
        .expect("read exhaustion")
        .story_list_progress(scope)
        .expect("progress")
        .expect("exists");
    assert_eq!(exhausted.generation, 1);
    assert_eq!(exhausted.pages_loaded, 1);
    assert!(exhausted.complete);

    let tx = store.write_txn().expect("relaunch pass");
    tx.start_story_list_pass(scope, 2_000).expect("restart");
    tx.commit().expect("commit restart");
    let relaunched = store
        .read_txn()
        .expect("read relaunch")
        .story_list_progress(scope)
        .expect("progress")
        .expect("exists");
    assert_eq!(relaunched.generation, 2);
    assert_eq!(relaunched.pages_loaded, 1);
    assert!(!relaunched.complete);

    let tx = store.write_txn().expect("bump namespace");
    let namespace_version = tx
        .bump_namespace(scope.account, 3_000)
        .expect("new namespace");
    let next_scope = gramdrive_model::identity::AccountScope {
        account: scope.account,
        namespace_version,
    };
    tx.start_story_list_pass(next_scope, 3_001)
        .expect("first pass in new namespace");
    tx.commit().expect("commit namespace pass");
    let new_namespace = store
        .read_txn()
        .expect("read new namespace")
        .story_list_progress(next_scope)
        .expect("progress")
        .expect("created lazily");
    assert_eq!(new_namespace.generation, 1);
    assert_eq!(new_namespace.pages_loaded, 0);
    assert!(!new_namespace.complete);
}

#[test]
fn story_progress_commit_rollback_and_relaunch_generation_are_resumable() {
    let mut store = common::store_with_account();
    common::insert_chat(store.connection(), 100);
    let chat = common::chat_key(100);
    let mut progress = store
        .read_txn()
        .expect("read seed")
        .story_sync_progress(&chat)
        .expect("progress")
        .expect("seeded");
    assert_eq!(progress.phase, StorySyncPhase::Pending);
    progress.phase = StorySyncPhase::Syncing;
    progress.active_complete = true;
    progress.profile_cursor = Some(40);
    progress.profile_scan_generation = 3;
    progress.archive_eligibility = StoryArchiveEligibility::Owner;
    progress.pages_committed = 2;
    progress.stories_seen = 51;
    progress.updated_at_ms = 1_000;
    let tx = store.write_txn().expect("write interrupted page");
    tx.put_story_sync_progress(&chat, &progress)
        .expect("stage progress");
    drop(tx);
    assert_eq!(
        store
            .read_txn()
            .expect("read rollback")
            .story_sync_progress(&chat)
            .expect("progress")
            .expect("exists")
            .profile_cursor,
        None
    );

    progress.phase = StorySyncPhase::Ready;
    progress.profile_complete = true;
    progress.archive_complete = true;
    let tx = store.write_txn().expect("write ready");
    tx.put_story_sync_progress(&chat, &progress)
        .expect("ready progress");
    tx.commit().expect("commit ready");
    let tx = store.write_txn().expect("relaunch");
    assert_eq!(
        tx.restart_ready_story_scans(chat.scope, 2_000)
            .expect("restart"),
        1
    );
    tx.commit().expect("commit restart");
    let relaunched = store
        .read_txn()
        .expect("read relaunch")
        .story_sync_progress(&chat)
        .expect("progress")
        .expect("exists");
    assert_eq!(relaunched.phase, StorySyncPhase::Pending);
    assert!(!relaunched.active_complete);
    assert!(!relaunched.profile_complete);
    assert_eq!(relaunched.profile_cursor, None);
    assert_eq!(relaunched.profile_scan_generation, 4);
    assert_eq!(
        relaunched.archive_eligibility,
        StoryArchiveEligibility::Unknown
    );
    assert_eq!(relaunched.archive_cursor, None);
    assert!(!relaunched.archive_complete);
}

#[test]
fn relaunch_preserves_each_interrupted_archive_page_cursor() {
    for (index, cursor) in [150, 100, 50].into_iter().enumerate() {
        let mut store = common::store_with_account();
        common::insert_chat(store.connection(), 100);
        let chat = common::chat_key(100);
        let mut progress = store
            .read_txn()
            .expect("read seed")
            .story_sync_progress(&chat)
            .expect("progress")
            .expect("seeded");
        progress.phase = StorySyncPhase::Syncing;
        progress.active_complete = true;
        progress.profile_complete = true;
        progress.archive_eligibility = StoryArchiveEligibility::Manageable;
        progress.archive_cursor = Some(cursor);
        progress.archive_complete = false;
        progress.pages_committed = u64::try_from(index + 1).expect("page count");
        progress.updated_at_ms = 1_000;
        let tx = store.write_txn().expect("write interrupted archive page");
        tx.put_story_sync_progress(&chat, &progress)
            .expect("persist interrupted page");
        tx.commit().expect("commit interrupted page");

        let tx = store.write_txn().expect("relaunch");
        assert_eq!(
            tx.restart_ready_story_scans(chat.scope, 2_000)
                .expect("restart"),
            0,
            "an interrupted scan must remain resumable at page {index}"
        );
        tx.commit().expect("commit relaunch");
        let resumed = store
            .read_txn()
            .expect("read resumed")
            .story_sync_progress(&chat)
            .expect("progress")
            .expect("exists");
        assert_eq!(resumed.phase, StorySyncPhase::Syncing);
        assert_eq!(
            resumed.archive_eligibility,
            StoryArchiveEligibility::Manageable
        );
        assert_eq!(resumed.archive_cursor, Some(cursor));
        assert!(!resumed.archive_complete);
        assert_eq!(
            resumed.pages_committed,
            u64::try_from(index + 1).expect("page count")
        );
    }
}

#[test]
fn retryable_story_failure_waits_for_relaunch_before_resuming() {
    let mut store = common::store_with_account();
    common::insert_chat(store.connection(), 100);
    let chat = common::chat_key(100);
    let mut progress = store
        .read_txn()
        .expect("read seed")
        .story_sync_progress(&chat)
        .expect("progress")
        .expect("seeded");
    progress.phase = StorySyncPhase::Failed;
    progress.active_complete = true;
    progress.profile_cursor = Some(44);
    progress.failure_category = Some("tdlib_request".to_owned());
    progress.retryable = true;
    progress.attempt_count = 5;
    let tx = store.write_txn().expect("write failure");
    tx.put_story_sync_progress(&chat, &progress)
        .expect("failed progress");
    tx.commit().expect("commit failure");

    assert!(
        store
            .read_txn()
            .expect("read worklist")
            .story_sync_worklist(chat.scope, 1)
            .expect("worklist")
            .is_empty(),
        "failed work must not spin in the same session"
    );

    let tx = store.write_txn().expect("write relaunch");
    assert_eq!(
        tx.restart_ready_story_scans(chat.scope, 2_000)
            .expect("restart"),
        1
    );
    tx.commit().expect("commit relaunch");
    let resumed = store
        .read_txn()
        .expect("read resumed")
        .story_sync_progress(&chat)
        .expect("progress")
        .expect("exists");
    assert_eq!(resumed.phase, StorySyncPhase::Pending);
    assert!(resumed.active_complete);
    assert_eq!(resumed.profile_cursor, Some(44));
    assert_eq!(resumed.attempt_count, 5);
}

#[test]
fn audit_profile_removal_retains_observed_row_without_new_fetch_work() {
    let mut store = common::store_with_account();
    common::insert_chat(store.connection(), 100);
    let chat = common::chat_key(100);
    let key = StoryKey {
        poster: chat,
        story_id: StoryId(94),
    };
    let hash = ContentHash::Sha256([0x94; 32]);
    let tx = store.write_txn().expect("write profile");
    let allowed = story_facts(key, StoryContentState::Available);
    tx.upsert_story_with_locators(&allowed, &[story_locator(key, &allowed.content_version)])
        .expect("story");
    tx.record_blob(key.poster.scope.account, &hash, 4_096, 1_000)
        .expect("blob");
    tx.link_story_blob(&key, &hash, 1_100).expect("link");
    tx.set_story_appearance(&StoryAppearanceRecord {
        story: key,
        location: StoryAppearanceLocation::Month {
            year: 2024,
            month: 7,
        },
        display_name: "Story 94.mp4".to_owned(),
        posted_at_ms: 1_721_555_200_000,
        expires_at_ms: None,
        removed_at_ms: None,
        profile_scan_generation: Some(2),
        profile_pin_order: None,
    })
    .expect("profile");
    tx.commit().expect("commit profile");

    let tx = store.write_txn().expect("write removal");
    tx.finish_profile_scan(&chat, 3, RetentionMode::Audit, 4_000)
        .expect("audit removal");
    tx.commit().expect("commit removal");
    let read = store.read_txn().expect("read");
    assert!(read.story(&key).expect("story").is_some());
    let appearances = read.story_appearances(&key).expect("appearances");
    assert_eq!(appearances.len(), 1);
    assert_eq!(appearances[0].removed_at_ms, Some(4_000));
    assert!(read.story_tombstone(&key).expect("tombstone").is_none());
    drop(read);

    let tx = store.write_txn().expect("write inaccessible");
    tx.mark_story_inaccessible(&key, RetentionMode::Audit, 5_000)
        .expect("reasonless removal");
    tx.commit().expect("commit inaccessible");
    let retained = store
        .read_txn()
        .expect("read retained")
        .story(&key)
        .expect("story")
        .expect("retained");
    assert_eq!(
        retained.facts.content_state,
        StoryContentState::Inaccessible
    );
    assert_eq!(retained.facts.mime_type.as_deref(), Some("video/mp4"));
    assert_eq!(retained.facts.exact_size, Some(4_096));
    assert_eq!(retained.blob_hash, Some(hash));
}

#[test]
fn confirmed_audit_to_mirror_purges_removed_profile_story_and_its_orphan_blob() {
    let mut store = common::store_with_account();
    common::insert_chat(store.connection(), 100);
    let tx = store.write_txn().expect("enable Audit");
    tx.set_retention_mode(common::scope().account, RetentionMode::Audit, None, 900)
        .expect("Audit");
    tx.commit().expect("commit Audit");
    let chat = common::chat_key(100);
    let key = StoryKey {
        poster: chat,
        story_id: StoryId(95),
    };
    let hash = ContentHash::Sha256([0x95; 32]);
    let tx = store.write_txn().expect("write profile");
    let allowed = story_facts(key, StoryContentState::Available);
    tx.upsert_story_with_locators(&allowed, &[story_locator(key, &allowed.content_version)])
        .expect("story");
    tx.record_blob(key.poster.scope.account, &hash, 4_096, 1_000)
        .expect("blob");
    tx.link_story_blob(&key, &hash, 1_100).expect("link");
    tx.set_story_appearance(&StoryAppearanceRecord {
        story: key,
        location: StoryAppearanceLocation::Month {
            year: 2024,
            month: 7,
        },
        display_name: "Story 95.mp4".to_owned(),
        posted_at_ms: 1_721_555_200_000,
        expires_at_ms: None,
        removed_at_ms: None,
        profile_scan_generation: Some(2),
        profile_pin_order: None,
    })
    .expect("profile");
    tx.commit().expect("commit profile");
    let tx = store.write_txn().expect("write removal");
    tx.finish_profile_scan(&chat, 3, RetentionMode::Audit, 4_000)
        .expect("audit removal");
    tx.commit().expect("commit removal");

    let confirmation = AuditToMirrorConfirmation::parse(
        common::scope().account,
        &AuditToMirrorConfirmation::expected_phrase(common::scope().account),
    )
    .expect("confirmation");
    let tx = store.write_txn().expect("purge");
    let change = tx
        .set_retention_mode(
            common::scope().account,
            RetentionMode::Mirror,
            Some(confirmation),
            5_000,
        )
        .expect("Mirror purge");
    tx.commit().expect("commit purge");
    assert_eq!(change.purged_stories, 2, "appearance and canonical row");
    assert_eq!(change.purged_blobs, 1);
    let read = store.read_txn().expect("read purge");
    assert!(read.story(&key).expect("story").is_none());
    assert!(
        read.story_appearances(&key)
            .expect("appearances")
            .is_empty()
    );
    assert!(
        read.blob(common::scope().account, &hash)
            .expect("blob")
            .is_none()
    );
}
