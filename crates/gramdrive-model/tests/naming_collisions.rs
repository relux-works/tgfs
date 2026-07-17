//! Collision resolution from stable identity (TASK-260715-1ffbkg; SYNC-012).
//!
//! Two things are pinned here. That colliding siblings come out distinct, and
//! that the suffix they come out with is a function of identity alone — never
//! of the order they were discovered in. The second is the one that costs
//! something to get wrong: a discovery-ordered suffix reshuffles folder names
//! on every re-sync, moving files under readers who have them open.
//!
//! The suffix goldens are pinned like the identity encodings are
//! (`identity_golden.rs`), and for the same reason: a chat's folder name must
//! survive an app update. A code change that moves one is renaming a user's
//! folder.

use gramdrive_model::identity::{
    AccountId, AccountKey, AccountScope, CanonicalKey, ChatId, ChatKey, ChatListKind, ItemId,
    ItemKey, NamespaceVersion,
};
use gramdrive_model::naming::{FALLBACK_NAME, NameKind, Platform, SafeName, SiblingName, sanitize};

fn scope() -> AccountScope {
    AccountScope {
        account: AccountKey {
            account_id: AccountId(42),
        },
        namespace_version: NamespaceVersion(1),
    }
}

/// The `ItemId` of chat `id` — the canonical key, as the tree would mint it
/// for a Main-list appearance's underlying record.
fn chat_id(id: i64) -> ItemId {
    ItemKey::Canonical(CanonicalKey::Chat(ChatKey {
        scope: scope(),
        chat_id: ChatId(id),
    }))
    .id()
}

fn resolve(siblings: &[(&ItemId, &str, NameKind)]) -> Vec<String> {
    let inputs: Vec<SiblingName<'_>> = siblings
        .iter()
        .map(|(id, raw, kind)| SiblingName {
            id,
            raw,
            kind: *kind,
        })
        .collect();
    gramdrive_model::naming::resolve_siblings(&inputs)
        .into_iter()
        .map(SafeName::into_string)
        .collect()
}

#[test]
fn siblings_that_do_not_collide_keep_their_plain_names() {
    let (a, b) = (chat_id(1), chat_id(2));
    let names = resolve(&[
        (&a, "Alice", NameKind::Directory),
        (&b, "Bob", NameKind::Directory),
    ]);
    assert_eq!(names, vec!["Alice", "Bob"]);
}

#[test]
fn colliding_siblings_are_suffixed_from_identity() {
    let (a, b) = (chat_id(1), chat_id(2));
    let names = resolve(&[
        (&a, "Bob", NameKind::Directory),
        (&b, "Bob", NameKind::Directory),
    ]);

    // Goldens: the suffix is a pure function of the ItemId bytes.
    assert_eq!(names, vec!["Bob (47fjxm4)", "Bob (27ngzyb)"]);
}

#[test]
fn every_member_of_a_collision_set_is_suffixed() {
    // Leaving the first bare would privilege whichever chat sorted first and
    // would rename the survivor when the other is deleted.
    let (a, b) = (chat_id(1), chat_id(2));
    let names = resolve(&[
        (&a, "Bob", NameKind::Directory),
        (&b, "Bob", NameKind::Directory),
    ]);
    assert!(names.iter().all(|name| name != "Bob"), "{names:?}");
}

#[test]
fn suffixes_ignore_discovery_order() {
    // The whole point of SYNC-012. Same set, three orders, same mapping.
    let (a, b, c) = (chat_id(1), chat_id(2), chat_id(3));

    let forward = resolve(&[
        (&a, "Bob", NameKind::Directory),
        (&b, "Bob", NameKind::Directory),
        (&c, "Bob", NameKind::Directory),
    ]);
    let reversed = resolve(&[
        (&c, "Bob", NameKind::Directory),
        (&b, "Bob", NameKind::Directory),
        (&a, "Bob", NameKind::Directory),
    ]);
    let shuffled = resolve(&[
        (&b, "Bob", NameKind::Directory),
        (&a, "Bob", NameKind::Directory),
        (&c, "Bob", NameKind::Directory),
    ]);

    assert_eq!(forward[0], reversed[2]);
    assert_eq!(forward[1], reversed[1]);
    assert_eq!(forward[2], reversed[0]);
    assert_eq!(forward[0], shuffled[1]);
    assert_eq!(forward[1], shuffled[0]);
    assert_eq!(forward[2], shuffled[2]);
}

#[test]
fn an_unrelated_sibling_does_not_change_a_collision_suffix() {
    // Adding Alice must not rename either Bob: a suffix depends on identity,
    // not on how many siblings there happen to be.
    let (a, b, c) = (chat_id(1), chat_id(2), chat_id(3));
    let without = resolve(&[
        (&a, "Bob", NameKind::Directory),
        (&b, "Bob", NameKind::Directory),
    ]);
    let with = resolve(&[
        (&a, "Bob", NameKind::Directory),
        (&b, "Bob", NameKind::Directory),
        (&c, "Alice", NameKind::Directory),
    ]);
    assert_eq!(without[0], with[0]);
    assert_eq!(without[1], with[1]);
}

#[test]
fn collisions_are_detected_case_insensitively() {
    // Apple and Windows resolve these to one directory, so the set is
    // ambiguous on the strictest target even though the strings differ.
    //
    // ASCII is the case every fold agrees on, and therefore the case that
    // proves least: the pairs the platforms fold *differently* — Greek sigma,
    // Turkish dotless i, sharp s — are pinned against each platform's own rule
    // in the fold corpus (`naming_fixture.rs`), which is where a wrong fold
    // shows up.
    let (a, b) = (chat_id(1), chat_id(2));
    let names = resolve(&[
        (&a, "Bob", NameKind::Directory),
        (&b, "BOB", NameKind::Directory),
    ]);
    assert_eq!(names, vec!["Bob (47fjxm4)", "BOB (27ngzyb)"]);
}

#[test]
fn collisions_created_by_sanitization_are_resolved() {
    // Different titles, one sanitized name: the substitution of `/` and `:`
    // is what makes them collide, and resolution is what makes them usable.
    let (a, b) = (chat_id(1), chat_id(2));
    let names = resolve(&[
        (&a, "a/b", NameKind::Directory),
        (&b, "a:b", NameKind::Directory),
    ]);
    assert_eq!(names, vec!["a_b (47fjxm4)", "a_b (27ngzyb)"]);
}

#[test]
fn untitled_siblings_collide_on_the_fallback_and_are_separated() {
    let (a, b) = (chat_id(1), chat_id(2));
    let names = resolve(&[
        (&a, "", NameKind::Directory),
        (&b, "   ", NameKind::Directory),
    ]);
    assert_eq!(names, vec!["Unnamed (47fjxm4)", "Unnamed (27ngzyb)"]);
    assert!(names.iter().all(|name| name.starts_with(FALLBACK_NAME)));
}

#[test]
fn a_suffixed_file_keeps_its_extension_last() {
    // "photo.jpg (abc)" would be a file whose extension is " (abc)" — no
    // platform could type it. The suffix goes on the stem.
    let (a, b) = (chat_id(1), chat_id(2));
    let names = resolve(&[
        (&a, "photo.jpg", NameKind::File),
        (&b, "photo.jpg", NameKind::File),
    ]);
    assert_eq!(names, vec!["photo (47fjxm4).jpg", "photo (27ngzyb).jpg"]);
    assert!(names.iter().all(|name| name.ends_with(".jpg")));
}

#[test]
fn a_crafted_title_impersonating_a_suffix_still_resolves() {
    // The adversarial case. Chat B is titled exactly what chat A's suffixed
    // name will be, so A's suffixing collides with B's plain name. Because
    // the check runs on the *final* names, B simply joins the collision set.
    let (a, b, c) = (chat_id(1), chat_id(2), chat_id(3));

    let plain = resolve(&[
        (&a, "Bob", NameKind::Directory),
        (&c, "Bob", NameKind::Directory),
    ]);
    let impersonation = plain[0].clone();

    let names = resolve(&[
        (&a, "Bob", NameKind::Directory),
        (&c, "Bob", NameKind::Directory),
        (&b, &impersonation, NameKind::Directory),
    ]);

    let distinct: std::collections::HashSet<String> =
        names.iter().map(|name| name.to_lowercase()).collect();
    assert_eq!(distinct.len(), names.len(), "not distinct: {names:?}");
}

#[test]
fn suffixed_names_stay_inside_the_budget() {
    // The stem yields to the suffix, not the other way round: a truncated
    // suffix would not disambiguate.
    let (a, b) = (chat_id(1), chat_id(2));
    let long = "x".repeat(400);
    let names = resolve(&[
        (&a, &long, NameKind::Directory),
        (&b, &long, NameKind::Directory),
    ]);
    for name in &names {
        assert!(name.len() <= 255, "{} bytes", name.len());
        assert!(name.ends_with(')'), "suffix was truncated: {name}");
        for platform in Platform::ALL {
            assert_eq!(platform.check(name), Ok(()), "{platform:?}");
        }
    }
    assert_ne!(names[0], names[1]);
}

#[test]
fn a_long_file_keeps_both_its_suffix_and_its_extension() {
    let (a, b) = (chat_id(1), chat_id(2));
    let long = format!("{}.jpg", "y".repeat(400));
    let names = resolve(&[(&a, &long, NameKind::File), (&b, &long, NameKind::File)]);
    for name in &names {
        assert!(name.len() <= 255, "{} bytes", name.len());
        assert!(name.ends_with(".jpg"), "lost the extension: {name}");
        assert!(SafeName::parse(name).is_ok());
    }
    assert_ne!(names[0], names[1]);
}

#[test]
fn a_reserved_name_collision_stays_escaped_and_distinct() {
    let (a, b) = (chat_id(1), chat_id(2));
    let names = resolve(&[
        (&a, "CON", NameKind::Directory),
        (&b, "con", NameKind::Directory),
    ]);
    assert_eq!(names, vec!["CON_ (47fjxm4)", "con_ (27ngzyb)"]);
    for name in &names {
        assert_eq!(Platform::Windows.check(name), Ok(()));
    }
}

#[test]
fn identity_not_title_drives_the_suffix() {
    // The same chat identity in the same collision shape gets the same
    // suffix whatever the title is — the suffix reads only the ItemId.
    let (a, b) = (chat_id(1), chat_id(2));
    let bobs = resolve(&[
        (&a, "Bob", NameKind::Directory),
        (&b, "Bob", NameKind::Directory),
    ]);
    let zeds = resolve(&[
        (&a, "Zed", NameKind::Directory),
        (&b, "Zed", NameKind::Directory),
    ]);
    let suffix_of = |name: &str| name[name.find('(').expect("suffix")..].to_string();
    assert_eq!(suffix_of(&bobs[0]), suffix_of(&zeds[0]));
    assert_eq!(suffix_of(&bobs[1]), suffix_of(&zeds[1]));
}

#[test]
fn appearances_of_one_chat_are_named_alike_in_every_view() {
    // A chat in Main and in a folder is two ItemIds over one canonical
    // record. Naming reads the canonical chat, so the folder is called the
    // same thing in both views — PRD-013 in name form.
    let record = chat_id(7);
    let name = sanitize(
        &gramdrive_model::naming::chat_folder_name("Alex", Some("alex")),
        NameKind::Directory,
    );
    for view in [ChatListKind::Main, ChatListKind::Archive] {
        let appearance = ItemKey::Appearance(gramdrive_model::identity::AppearanceKey {
            view,
            item: CanonicalKey::Chat(ChatKey {
                scope: scope(),
                chat_id: ChatId(7),
            }),
        })
        .id();
        // Distinct identities...
        assert_ne!(appearance.as_bytes(), record.as_bytes());
        // ...one name, because the name is derived from the chat's title.
        let names = resolve(&[(&record, "Alex — @alex", NameKind::Directory)]);
        assert_eq!(names[0], name.as_str());
    }
}
