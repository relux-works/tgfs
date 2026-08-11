//! Canonical non-viewing story discovery and privacy regressions.

#![allow(clippy::expect_used, clippy::panic)]

use gramdrive_source_tdjson::{
    StoryAccountKind, StoryArchiveCapability, StoryChatKind, StoryChatPlan, StoryCommit,
    StoryContentKind, StoryFileType, StoryMachine, StoryScanCursor, StoryStep, TdError,
    background_story_request_allowed,
};
use serde_json::{Value, json};

fn cursor() -> StoryScanCursor {
    StoryScanCursor {
        active_complete: false,
        profile_cursor: None,
        profile_scan_generation: 7,
        profile_complete: false,
        archive_capability: StoryArchiveCapability::Unknown,
        archive_cursor: None,
        archive_complete: false,
    }
}

fn take_submit(machine: &mut StoryMachine, method: &str) -> Value {
    let StoryStep::Submit(request) = machine.next_step().expect("step") else {
        panic!("expected submit")
    };
    assert_eq!(request.get("@type").and_then(Value::as_str), Some(method));
    assert!(background_story_request_allowed(&request));
    request
}

fn take_commit(machine: &mut StoryMachine) -> StoryCommit {
    let StoryStep::Commit(commit) = machine.next_step().expect("step") else {
        panic!("expected commit")
    };
    commit
}

fn story(chat_id: i64, story_id: i64, profile: bool, forwarded: bool) -> Value {
    json!({
        "@type": "story",
        "id": story_id,
        "poster_chat_id": chat_id,
        "date": 1_784_692_800,
        "is_posted_to_chat_page": profile,
        "can_be_forwarded": forwarded,
        "content": {
            "@type": "storyContentVideo",
            "video": {
                "@type": "storyVideo",
                "video": {
                    "@type": "file",
                    "id": 599,
                    "size": 4096,
                    "remote": {
                        "@type": "remoteFile",
                        "id": "must-never-cross-protection",
                        "unique_id": "must-never-cross-protection"
                    }
                }
            }
        },
        "caption": {"@type": "formattedText", "text": "must be dropped", "entities": []}
    })
}

#[test]
fn background_dispatch_rejects_every_view_live_download_and_mutation_method() {
    for method in [
        "openStory",
        "closeStory",
        "getGroupCall",
        "joinLiveStory",
        "leaveGroupCall",
        "getGroupCallStreams",
        "getGroupCallStreamSegment",
        "downloadFile",
        "toggleStoryIsPostedToChatPage",
        "deleteStory",
    ] {
        assert!(
            !background_story_request_allowed(&json!({"@type": method})),
            "{method} escaped the background deny boundary"
        );
    }
}

#[test]
fn protected_story_is_redacted_before_it_crosses_the_source_boundary() {
    let mut machine = StoryMachine::new(7, StoryAccountKind::Regular).expect("machine");
    machine.on_update(&json!({
        "@type": "updateStory",
        "story": story(100, 9, true, false)
    }));
    let StoryCommit::Upsert(observation) = take_commit(&mut machine) else {
        panic!("expected upsert")
    };
    assert_eq!(observation.content_kind, StoryContentKind::Protected);
    assert!(!observation.can_be_forwarded);
    assert_eq!(observation.mime_type, None);
    assert_eq!(observation.exact_size, None);
    assert!(observation.locators.is_empty());
    let serialized = format!("{observation:?}");
    assert!(!serialized.contains("must be dropped"));
    assert!(!serialized.contains("must-never-cross-protection"));
    assert!(!serialized.contains("599"));
}

#[test]
fn story_list_main_loader_discovers_a_chat_outside_the_per_chat_worklist_and_resumes_to_404() {
    let mut machine = StoryMachine::new(7, StoryAccountKind::Regular).expect("machine");
    machine
        .start_active_list_discovery()
        .expect("start list loader");
    let request = take_submit(&mut machine, "loadActiveStories");
    assert_eq!(
        request.pointer("/story_list/@type").and_then(Value::as_str),
        Some("storyListMain")
    );

    // Chat 777 was never enqueued for a getChatActiveStories scan. TDLib's
    // ordered list update is independently sufficient to discover it.
    machine.on_update(&json!({
        "@type": "updateChatActiveStories",
        "active_stories": {
            "@type": "chatActiveStories",
            "chat_id": 777,
            "order": 70,
            "stories": [{
                "@type": "storyInfo",
                "story_id": 12,
                "date": 1_784_692_800,
                "is_live": true
            }]
        }
    }));
    machine
        .on_response(Ok(json!({"@type": "ok"})))
        .expect("bounded list page");
    assert!(matches!(
        take_commit(&mut machine),
        StoryCommit::ActiveSnapshot { chat_id: 777, order: 70, stories }
            if stories.len() == 1 && stories[0].story_id == 12
    ));
    assert!(matches!(
        take_commit(&mut machine),
        StoryCommit::ActiveListProgress { complete: false }
    ));

    take_submit(&mut machine, "loadActiveStories");
    machine
        .on_response(Err(TdError::Td {
            code: 404,
            message: "all active stories are loaded".to_owned(),
        }))
        .expect("documented exhaustion");
    assert!(matches!(
        take_commit(&mut machine),
        StoryCommit::ActiveListProgress { complete: true }
    ));
    assert!(matches!(
        machine.next_step().expect("idle"),
        StoryStep::Idle
    ));
}

#[test]
fn allowed_video_retains_typed_primary_alternative_and_thumbnail_locators() {
    let mut value = story(100, 54, true, true);
    value["content"]["video"]["video"]["expected_size"] = json!(4_500);
    value["content"]["video"]["thumbnail"] = json!({
        "@type": "thumbnail",
        "file": {
            "@type": "file",
            "id": 600,
            "size": 128,
            "expected_size": 128,
            "remote": {"@type": "remoteFile", "id": "thumb-r", "unique_id": "thumb-u"}
        }
    });
    value["content"]["alternative_video"] = json!({
        "@type": "storyVideo",
        "video": {
            "@type": "file",
            "id": 601,
            "size": 2048,
            "expected_size": 2048,
            "remote": {"@type": "remoteFile", "id": "alt-r", "unique_id": "alt-u"}
        }
    });
    let mut machine = StoryMachine::new(7, StoryAccountKind::Regular).expect("machine");
    machine.on_update(&json!({"@type": "updateStory", "story": value}));
    let StoryCommit::Upsert(observation) = take_commit(&mut machine) else {
        panic!("expected upsert")
    };
    assert_eq!(observation.locators.len(), 3);
    let primary = observation
        .locators
        .iter()
        .find(|locator| locator.is_primary)
        .expect("primary locator");
    assert_eq!(primary.role, "video-primary");
    assert_eq!(primary.file_type, StoryFileType::VideoStory);
    assert_eq!(primary.local_file_id, Some(599));
    assert_eq!(
        primary.remote_file_id.as_deref(),
        Some("must-never-cross-protection")
    );
    assert_eq!(
        primary.remote_unique_id.as_deref(),
        Some("must-never-cross-protection")
    );
    assert_eq!(primary.size, Some(4096));
    assert_eq!(primary.expected_size, Some(4_500));
    assert_eq!(observation.content_version, primary.content_version);
    assert!(observation.locators.iter().any(|locator| {
        locator.role == "video-alternative" && locator.file_type == StoryFileType::VideoStory
    }));
    assert!(observation.locators.iter().any(|locator| {
        locator.role == "video-thumbnail" && locator.file_type == StoryFileType::Thumbnail
    }));
}

#[test]
fn allowed_photo_retains_all_sizes_and_selects_one_stable_primary_locator() {
    let value = json!({
        "@type": "story",
        "id": 55,
        "poster_chat_id": 100,
        "date": 1_784_692_800,
        "is_posted_to_chat_page": true,
        "can_be_forwarded": true,
        "content": {
            "@type": "storyContentPhoto",
            "photo": {
                "@type": "photo",
                "sizes": [
                    {
                        "@type": "photoSize",
                        "type": "m",
                        "photo": {
                            "@type": "file",
                            "id": 700,
                            "size": 1024,
                            "remote": {"@type": "remoteFile", "id": "m-r", "unique_id": "m-u"}
                        }
                    },
                    {
                        "@type": "photoSize",
                        "type": "x",
                        "photo": {
                            "@type": "file",
                            "id": 701,
                            "size": 4096,
                            "remote": {"@type": "remoteFile", "id": "x-r", "unique_id": "x-u"}
                        }
                    }
                ]
            }
        }
    });
    let mut machine = StoryMachine::new(7, StoryAccountKind::Regular).expect("machine");
    machine.on_update(&json!({"@type": "updateStory", "story": value}));
    let StoryCommit::Upsert(observation) = take_commit(&mut machine) else {
        panic!("expected upsert")
    };
    assert_eq!(observation.locators.len(), 2);
    assert!(observation.locators.iter().all(|locator| {
        locator.file_type == StoryFileType::PhotoStory && locator.role.starts_with("photo-size:")
    }));
    let primary = observation
        .locators
        .iter()
        .find(|locator| locator.is_primary)
        .expect("primary photo size");
    assert_eq!(primary.role, "photo-size:x");
    assert_eq!(primary.local_file_id, Some(701));
    assert_eq!(observation.content_version, primary.content_version);
}

#[test]
fn active_profile_rights_and_archive_are_bounded_and_non_viewing() {
    let mut machine = StoryMachine::new(7, StoryAccountKind::Regular).expect("machine");
    machine
        .enqueue_chat(StoryChatPlan {
            chat_id: -100,
            chat_kind: StoryChatKind::Supergroup,
            cursor: cursor(),
        })
        .expect("enqueue");

    take_submit(&mut machine, "getChatActiveStories");
    machine
        .on_response(Ok(json!({
            "@type": "chatActiveStories",
            "chat_id": -100,
            "order": 70,
            "stories": []
        })))
        .expect("active response");
    assert!(matches!(
        take_commit(&mut machine),
        StoryCommit::ActiveSnapshot { chat_id: -100, stories, .. } if stories.is_empty()
    ));

    take_submit(&mut machine, "getChatPostedToChatPageStories");
    machine
        .on_response(Ok(json!({
            "@type": "stories",
            "total_count": 1,
            "stories": [story(-100, 41, true, true)],
            "pinned_story_ids": [41]
        })))
        .expect("profile response");
    assert!(matches!(
        take_commit(&mut machine),
        StoryCommit::ProfilePage {
            chat_id: -100,
            generation: 7,
            stories,
            pinned_story_ids,
            complete: false,
            ..
        } if stories.len() == 1 && pinned_story_ids == vec![41]
    ));

    let profile_tail = take_submit(&mut machine, "getChatPostedToChatPageStories");
    assert_eq!(
        profile_tail.get("from_story_id").and_then(Value::as_i64),
        Some(41)
    );
    machine
        .on_response(Ok(json!({
            "@type": "stories",
            "total_count": 1,
            "stories": [],
            "pinned_story_ids": []
        })))
        .expect("profile terminal response");
    assert!(matches!(
        take_commit(&mut machine),
        StoryCommit::ProfilePage { complete: true, stories, .. } if stories.is_empty()
    ));

    let rights = take_submit(&mut machine, "getChatMember");
    assert_eq!(
        rights.pointer("/member_id/@type").and_then(Value::as_str),
        Some("messageSenderUser")
    );
    machine
        .on_response(Ok(json!({
            "@type": "chatMember",
            "member_id": {"@type": "messageSenderUser", "user_id": 7},
            "status": {
                "@type": "chatMemberStatusAdministrator",
                "rights": {"@type": "chatAdministratorRights", "can_edit_stories": true}
            }
        })))
        .expect("rights response");
    assert!(matches!(
        take_commit(&mut machine),
        StoryCommit::ArchiveCapability {
            capability: StoryArchiveCapability::Manageable,
            ..
        }
    ));

    take_submit(&mut machine, "getChatArchivedStories");
    machine
        .on_response(Ok(json!({
            "@type": "stories",
            "total_count": 0,
            "stories": [],
            "pinned_story_ids": []
        })))
        .expect("archive response");
    assert!(matches!(
        take_commit(&mut machine),
        StoryCommit::ArchivePage { complete: true, stories, .. } if stories.is_empty()
    ));
    assert!(matches!(
        take_commit(&mut machine),
        StoryCommit::ScanComplete { chat_id: -100 }
    ));
}

#[test]
fn ordinary_chat_never_schedules_rights_or_archive_requests() {
    let mut scan = cursor();
    scan.active_complete = true;
    scan.profile_complete = true;
    let mut machine = StoryMachine::new(7, StoryAccountKind::Regular).expect("machine");
    machine
        .enqueue_chat(StoryChatPlan {
            chat_id: 99,
            chat_kind: StoryChatKind::Private,
            cursor: scan,
        })
        .expect("enqueue");
    assert!(matches!(
        take_commit(&mut machine),
        StoryCommit::ArchiveCapability {
            capability: StoryArchiveCapability::Ineligible,
            ..
        }
    ));
    assert!(matches!(
        take_commit(&mut machine),
        StoryCommit::ScanComplete { chat_id: 99 }
    ));
}

#[test]
fn relaunch_cursor_deduplicates_the_inclusive_profile_boundary() {
    let mut scan = cursor();
    scan.active_complete = true;
    scan.profile_cursor = Some(41);
    let mut machine = StoryMachine::new(7, StoryAccountKind::Regular).expect("machine");
    machine
        .enqueue_chat(StoryChatPlan {
            chat_id: 7,
            chat_kind: StoryChatKind::Private,
            cursor: scan,
        })
        .expect("enqueue");
    let request = take_submit(&mut machine, "getChatPostedToChatPageStories");
    assert_eq!(
        request.get("from_story_id").and_then(Value::as_i64),
        Some(41)
    );
    machine
        .on_response(Ok(json!({
            "@type": "stories",
            "total_count": 2,
            "stories": [story(7, 41, true, true), story(7, 40, true, true)],
            "pinned_story_ids": []
        })))
        .expect("page");
    let StoryCommit::ProfilePage {
        stories,
        pinned_story_ids,
        next_from_story_id,
        complete,
        generation,
        ..
    } = take_commit(&mut machine)
    else {
        panic!("expected profile page")
    };
    assert_eq!(stories.len(), 1);
    assert_eq!(stories[0].story_id, 40);
    assert!(
        pinned_story_ids.is_empty(),
        "continuation pages do not carry pin order"
    );
    assert_eq!(next_from_story_id, Some(40));
    assert!(!complete, "a short page is not terminal");

    // Simulate a crash after the page and relaunch from its committed inclusive
    // cursor. The next short page must still advance rather than finalize the
    // profile reconciliation early.
    let mut resumed = cursor();
    resumed.active_complete = true;
    resumed.profile_cursor = next_from_story_id;
    resumed.profile_scan_generation = generation;
    let mut relaunched = StoryMachine::new(7, StoryAccountKind::Regular).expect("machine");
    relaunched
        .enqueue_chat(StoryChatPlan {
            chat_id: 7,
            chat_kind: StoryChatKind::Private,
            cursor: resumed,
        })
        .expect("resume");
    let request = take_submit(&mut relaunched, "getChatPostedToChatPageStories");
    assert_eq!(
        request.get("from_story_id").and_then(Value::as_i64),
        Some(40)
    );
    relaunched
        .on_response(Ok(json!({
            "@type": "stories",
            "total_count": 3,
            "stories": [story(7, 40, true, true), story(7, 39, true, true)],
            "pinned_story_ids": []
        })))
        .expect("second short page");
    assert!(matches!(
        take_commit(&mut relaunched),
        StoryCommit::ProfilePage {
            stories,
            next_from_story_id: Some(39),
            complete: false,
            ..
        } if stories.len() == 1 && stories[0].story_id == 39
    ));
    let terminal = take_submit(&mut relaunched, "getChatPostedToChatPageStories");
    assert_eq!(
        terminal.get("from_story_id").and_then(Value::as_i64),
        Some(39)
    );
    relaunched
        .on_response(Ok(json!({
            "@type": "stories",
            "total_count": 3,
            "stories": [],
            "pinned_story_ids": []
        })))
        .expect("terminal page");
    assert!(matches!(
        take_commit(&mut relaunched),
        StoryCommit::ProfilePage { complete: true, stories, .. } if stories.is_empty()
    ));
}

#[test]
fn live_updates_emit_transition_removal_and_post_identity_without_viewing() {
    let mut machine = StoryMachine::new(7, StoryAccountKind::Regular).expect("machine");
    machine.on_update(&json!({
        "@type": "updateChatActiveStories",
        "active_stories": {
            "@type": "chatActiveStories",
            "chat_id": 100,
            "order": 70,
            "stories": [{
                "@type": "storyInfo",
                "story_id": 51,
                "date": 1_784_692_800,
                "is_live": false
            }]
        }
    }));
    assert!(matches!(
        take_commit(&mut machine),
        StoryCommit::ActiveSnapshot { chat_id: 100, stories, .. }
            if stories.len() == 1 && stories[0].content_kind == StoryContentKind::MetadataPending
    ));
    take_submit(&mut machine, "getStory");
    machine
        .on_response(Ok(story(100, 51, true, true)))
        .expect("enrichment");
    assert!(matches!(
        take_commit(&mut machine),
        StoryCommit::Upsert(story) if story.is_posted_to_chat_page
    ));

    machine.on_update(&json!({
        "@type": "updateStoryDeleted",
        "story_poster_chat_id": 100,
        "story_id": 51
    }));
    assert!(matches!(
        take_commit(&mut machine),
        StoryCommit::Inaccessible {
            poster_chat_id: 100,
            story_id: 51
        }
    ));

    machine.on_update(&json!({
        "@type": "updateStoryPostSucceeded",
        "old_story_id": -9,
        "story": story(100, 52, false, true)
    }));
    assert!(matches!(
        take_commit(&mut machine),
        StoryCommit::PostSucceeded { old_story_id: -9, story } if story.story_id == 52
    ));
    assert!(matches!(
        machine.next_step().expect("idle"),
        StoryStep::Idle
    ));
}

#[test]
fn expected_size_is_not_claimed_as_exact_story_extent() {
    let mut value = story(100, 53, false, true);
    value["content"]["video"]["video"]["size"] = json!(0);
    value["content"]["video"]["video"]["expected_size"] = json!(9_999);
    let mut machine = StoryMachine::new(7, StoryAccountKind::Regular).expect("machine");
    machine.on_update(&json!({"@type": "updateStory", "story": value}));
    let StoryCommit::Upsert(observation) = take_commit(&mut machine) else {
        panic!("expected upsert")
    };
    assert_eq!(observation.exact_size, None);
    assert_eq!(observation.locators[0].size, None);
    assert_eq!(observation.locators[0].expected_size, Some(9_999));
}

#[test]
fn owner_archive_is_allowed_and_bot_owner_shape_fails_closed() {
    for (account_kind, expected) in [
        (StoryAccountKind::Regular, StoryArchiveCapability::Owner),
        (
            StoryAccountKind::Bot,
            StoryArchiveCapability::AccountUnsupported,
        ),
    ] {
        let mut scan = cursor();
        scan.active_complete = true;
        scan.profile_complete = true;
        let mut machine = StoryMachine::new(7, account_kind).expect("machine");
        machine
            .enqueue_chat(StoryChatPlan {
                chat_id: 7,
                chat_kind: StoryChatKind::Private,
                cursor: scan,
            })
            .expect("enqueue");
        assert!(matches!(
            take_commit(&mut machine),
            StoryCommit::ArchiveCapability { capability, .. } if capability == expected
        ));
        if expected.permits_archive() {
            take_submit(&mut machine, "getChatArchivedStories");
        } else {
            assert!(matches!(
                take_commit(&mut machine),
                StoryCommit::ScanComplete { chat_id: 7 }
            ));
        }
    }
}

#[test]
fn unavailable_rights_fail_closed_and_are_retried_after_relaunch() {
    let mut scan = cursor();
    scan.active_complete = true;
    scan.profile_complete = true;
    let mut first_session = StoryMachine::new(7, StoryAccountKind::Regular).expect("machine");
    first_session
        .enqueue_chat(StoryChatPlan {
            chat_id: -100,
            chat_kind: StoryChatKind::Supergroup,
            cursor: scan,
        })
        .expect("enqueue");
    take_submit(&mut first_session, "getChatMember");
    first_session
        .on_response(Err(TdError::Td {
            code: 403,
            message: "rights temporarily unavailable".to_owned(),
        }))
        .expect("unavailable rights are a fail-closed boundary");
    assert!(matches!(
        take_commit(&mut first_session),
        StoryCommit::ArchiveCapability {
            capability: StoryArchiveCapability::RightsUnavailable,
            ..
        }
    ));
    assert!(matches!(
        take_commit(&mut first_session),
        StoryCommit::ScanComplete { chat_id: -100 }
    ));

    let mut relaunched = cursor();
    relaunched.active_complete = true;
    relaunched.profile_complete = true;
    relaunched.archive_capability = StoryArchiveCapability::RightsUnavailable;
    let mut second_session = StoryMachine::new(7, StoryAccountKind::Regular).expect("machine");
    second_session
        .enqueue_chat(StoryChatPlan {
            chat_id: -100,
            chat_kind: StoryChatKind::Supergroup,
            cursor: relaunched,
        })
        .expect("enqueue relaunch");

    take_submit(&mut second_session, "getChatMember");
    second_session
        .on_response(Ok(json!({
            "@type": "chatMember",
            "member_id": {"@type": "messageSenderUser", "user_id": 7},
            "status": {"@type": "chatMemberStatusCreator"}
        })))
        .expect("rights recovered");
    assert!(matches!(
        take_commit(&mut second_session),
        StoryCommit::ArchiveCapability {
            capability: StoryArchiveCapability::Manageable,
            ..
        }
    ));
    let archive = take_submit(&mut second_session, "getChatArchivedStories");
    assert_eq!(
        archive.get("from_story_id").and_then(Value::as_i64),
        Some(0)
    );
}

#[test]
fn completed_owner_archive_restarts_at_head_and_discovers_downtime_story() {
    // Durable state clears a Ready scan's archive cursor/completion at the
    // next owned-session boundary while retaining interrupted Syncing cursors.
    let mut relaunched = cursor();
    relaunched.active_complete = true;
    relaunched.profile_complete = true;
    let mut machine = StoryMachine::new(7, StoryAccountKind::Regular).expect("machine");
    machine
        .enqueue_chat(StoryChatPlan {
            chat_id: 7,
            chat_kind: StoryChatKind::Private,
            cursor: relaunched,
        })
        .expect("enqueue relaunch");

    assert!(matches!(
        take_commit(&mut machine),
        StoryCommit::ArchiveCapability {
            capability: StoryArchiveCapability::Owner,
            ..
        }
    ));
    let request = take_submit(&mut machine, "getChatArchivedStories");
    assert_eq!(
        request.get("from_story_id").and_then(Value::as_i64),
        Some(0)
    );
    machine
        .on_response(Ok(json!({
            "@type": "stories",
            "total_count": 1,
            "stories": [story(7, 99, false, true)],
            "pinned_story_ids": []
        })))
        .expect("downtime-only story page");
    assert!(matches!(
        take_commit(&mut machine),
        StoryCommit::ArchivePage {
            stories,
            next_from_story_id: Some(99),
            complete: false,
            ..
        }
            if stories.len() == 1 && stories[0].story_id == 99
    ));
    let next = take_submit(&mut machine, "getChatArchivedStories");
    assert_eq!(next.get("from_story_id").and_then(Value::as_i64), Some(99));
    machine
        .on_response(Ok(json!({
            "@type": "stories",
            "total_count": 2,
            "stories": [story(7, 99, false, true), story(7, 98, false, true)],
            "pinned_story_ids": []
        })))
        .expect("second short archive page");
    assert!(matches!(
        take_commit(&mut machine),
        StoryCommit::ArchivePage {
            stories,
            next_from_story_id: Some(98),
            complete: false,
            ..
        } if stories.len() == 1 && stories[0].story_id == 98
    ));
    take_submit(&mut machine, "getChatArchivedStories");
    machine
        .on_response(Ok(json!({
            "@type": "stories",
            "total_count": 2,
            "stories": [],
            "pinned_story_ids": []
        })))
        .expect("terminal archive page");
    assert!(matches!(
        take_commit(&mut machine),
        StoryCommit::ArchivePage { stories, complete: true, .. } if stories.is_empty()
    ));
}

#[test]
fn newer_live_update_discards_inflight_snapshot_and_requires_durable_resync() {
    let mut machine = StoryMachine::new(7, StoryAccountKind::Regular).expect("machine");
    machine
        .enqueue_chat(StoryChatPlan {
            chat_id: 100,
            chat_kind: StoryChatKind::Private,
            cursor: cursor(),
        })
        .expect("enqueue");
    take_submit(&mut machine, "getChatActiveStories");
    machine.on_update(&json!({
        "@type": "updateChatActiveStories",
        "active_stories": {
            "@type": "chatActiveStories",
            "chat_id": 100,
            "order": 70,
            "stories": []
        }
    }));
    machine
        .on_response(Ok(json!({
            "@type": "chatActiveStories",
            "chat_id": 100,
            "order": 70,
            "stories": [{
                "@type": "storyInfo",
                "story_id": 44,
                "date": 1_784_692_800,
                "is_live": false
            }]
        })))
        .expect("stale response is discarded");

    assert!(matches!(
        take_commit(&mut machine),
        StoryCommit::ActiveSnapshot { stories, .. } if stories.is_empty()
    ));
    assert!(matches!(
        machine.next_step().expect("resync"),
        StoryStep::ResyncRequired(chat_ids) if chat_ids == vec![100]
    ));
    assert!(machine.abandon_active_chat().is_some());
    assert!(matches!(
        machine.next_step().expect("idle"),
        StoryStep::Idle
    ));
}
