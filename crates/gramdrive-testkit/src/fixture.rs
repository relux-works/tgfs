//! Identity and item constructors for writing scripts.
//!
//! Assembling a [`SourceItem`] by hand means minting an [`ItemId`] through
//! the key vocabulary and validating two version tokens — correct, and too
//! much ceremony to repeat in every test in every crate. These helpers are
//! the shorthand, and nothing more: they mint real identities through
//! [`ItemKey::id`], so a script built from them is made of the same values
//! a real source would serve.
//!
//! Every constructor returns `Result` rather than unwrapping internally.
//! The workspace denies `unwrap_used`/`expect_used` outside `#[cfg(test)]`
//! code, and this module is library code — a helper that panicked on a bad
//! literal would need an exemption to exist. Callers are tests, where
//! `.expect("valid fixture")` costs one call and is allowed
//! (`clippy.toml`).
//!
//! ```
//! # use gramdrive_testkit::fixture;
//! # use gramdrive_testkit::source::{DirectoryKind, FileKind};
//! let scope = fixture::scope();
//! let root = fixture::account_root_id(scope);
//! let chat = fixture::chat_id(scope, 100);
//!
//! let root_item = fixture::directory(root.clone(), None, "Account", "m1", DirectoryKind::Root)
//!     .expect("valid fixture");
//! let chat_item = fixture::directory(chat, Some(root), "Team", "m2", DirectoryKind::Chat)
//!     .expect("valid fixture");
//! # assert!(root_item.is_directory() && chat_item.is_directory());
//! ```

use gramdrive_model::identity::{
    AccountId, AccountKey, AccountScope, AttachmentIndex, AttachmentKey, CanonicalKey, ChatId,
    ChatKey, ChatListKey, ChatListKind, ItemId, ItemKey, MediaDirKey, MessageId, MessageKey,
    NamespaceVersion, YearDirKey,
};
use gramdrive_model::version::{ContentVersion, InvalidVersionToken, MetadataVersion};
use gramdrive_source::{
    ContentAvailability, DirectoryKind, FileFacts, FileKind, ItemContent, SourceItem,
};

/// The account ID [`scope`] uses.
pub const FIXTURE_ACCOUNT_ID: i64 = 7;

/// The canonical scope for tests: account [`FIXTURE_ACCOUNT_ID`], namespace
/// epoch 1.
///
/// A named constant rather than a per-test choice so that a cursor minted
/// in one test's fixture is recognisably foreign to another's — see
/// [`foreign_scope`].
pub fn scope() -> AccountScope {
    scope_for(FIXTURE_ACCOUNT_ID, 1)
}

/// A scope for an arbitrary account and namespace epoch.
pub fn scope_for(account_id: i64, namespace_version: u32) -> AccountScope {
    AccountScope {
        account: AccountKey {
            account_id: AccountId(account_id),
        },
        namespace_version: NamespaceVersion(namespace_version),
    }
}

/// A scope that is not [`scope`] — a different account entirely.
///
/// For the SYNC-004 rejection path: a cursor from this scope presented to a
/// source serving [`scope`] must fail with
/// [`SourceError::CursorRejected`](gramdrive_source::SourceError::CursorRejected).
pub fn foreign_scope() -> AccountScope {
    scope_for(FIXTURE_ACCOUNT_ID + 1, 1)
}

/// The same account under a retired namespace epoch.
///
/// The other half of SYNC-004: not a foreign account, but an identity
/// namespace this source no longer serves.
pub fn retired_scope() -> AccountScope {
    scope_for(FIXTURE_ACCOUNT_ID, 0)
}

/// Identity of the account root directory.
pub fn account_root_id(scope: AccountScope) -> ItemId {
    ItemKey::Canonical(CanonicalKey::Account(scope.account)).id()
}

/// Identity of a chat-list view root (Main, Archive, or a custom folder).
pub fn chat_list_id(scope: AccountScope, kind: ChatListKind) -> ItemId {
    ItemKey::Canonical(CanonicalKey::ChatList(ChatListKey { scope, kind })).id()
}

/// Canonical identity of a chat.
pub fn chat_id(scope: AccountScope, chat: i64) -> ItemId {
    ItemKey::Canonical(CanonicalKey::Chat(chat_key(scope, chat))).id()
}

/// Identity of a chat as it appears through one chat-list view (DOM-022).
pub fn chat_appearance_id(scope: AccountScope, chat: i64, view: ChatListKind) -> ItemId {
    ItemKey::Appearance(gramdrive_model::identity::AppearanceKey {
        view,
        item: CanonicalKey::Chat(chat_key(scope, chat)),
    })
    .id()
}

/// Identity of a chat's calendar-year export directory.
pub fn year_dir_id(scope: AccountScope, chat: i64, year: u16) -> ItemId {
    ItemKey::Canonical(CanonicalKey::YearDir(YearDirKey {
        chat: chat_key(scope, chat),
        year,
    }))
    .id()
}

/// Identity of the media directory of one chat-export year.
pub fn media_dir_id(scope: AccountScope, chat: i64, year: u16) -> ItemId {
    ItemKey::Canonical(CanonicalKey::MediaDir(MediaDirKey {
        chat: chat_key(scope, chat),
        year,
    }))
    .id()
}

/// Identity of one attachment of one message.
pub fn attachment_id(scope: AccountScope, chat: i64, message: i64, index: u32) -> ItemId {
    ItemKey::Canonical(CanonicalKey::Attachment(AttachmentKey {
        message: MessageKey {
            chat: chat_key(scope, chat),
            message_id: MessageId(message),
        },
        index: AttachmentIndex(index),
    }))
    .id()
}

fn chat_key(scope: AccountScope, chat: i64) -> ChatKey {
    ChatKey {
        scope,
        chat_id: ChatId(chat),
    }
}

/// A directory item. `parent` is `None` for the account root and only for
/// the account root.
pub fn directory(
    id: ItemId,
    parent: Option<ItemId>,
    display_name: &str,
    metadata_version: &str,
    kind: DirectoryKind,
) -> Result<SourceItem, InvalidVersionToken> {
    Ok(SourceItem {
        id,
        parent,
        display_name: display_name.to_owned(),
        metadata_version: MetadataVersion::new(metadata_version)?,
        created_at_ms: None,
        modified_at_ms: None,
        content: ItemContent::Directory(kind),
    })
}

/// A fetchable file item of `size` bytes, with no declared MIME type.
///
/// The script must carry matching content for `content_version`;
/// [`ScriptBuilder::build`](crate::ScriptBuilder::build) rejects the script
/// otherwise, and checks `size` against the registered bytes.
pub fn file(
    id: ItemId,
    parent: ItemId,
    display_name: &str,
    metadata_version: &str,
    content_version: &str,
    size: u64,
    kind: FileKind,
) -> Result<SourceItem, InvalidVersionToken> {
    Ok(SourceItem {
        id,
        parent: Some(parent),
        display_name: display_name.to_owned(),
        metadata_version: MetadataVersion::new(metadata_version)?,
        created_at_ms: None,
        modified_at_ms: None,
        content: ItemContent::File(FileFacts {
            kind,
            content_version: ContentVersion::new(content_version)?,
            size: Some(size),
            mime_type: None,
            availability: ContentAvailability::Fetchable,
        }),
    })
}

/// A file whose bytes the source refuses to serve (POL-4).
///
/// Visible, sized, and never fetchable: a fetch fails with
/// [`SourceError::Restricted`](gramdrive_source::SourceError::Restricted),
/// and the script needs no content for it.
pub fn restricted_file(
    id: ItemId,
    parent: ItemId,
    display_name: &str,
    metadata_version: &str,
    content_version: &str,
    size: u64,
    kind: FileKind,
) -> Result<SourceItem, InvalidVersionToken> {
    let mut item = file(
        id,
        parent,
        display_name,
        metadata_version,
        content_version,
        size,
        kind,
    )?;
    if let ItemContent::File(facts) = &mut item.content {
        facts.availability = ContentAvailability::Restricted;
    }
    Ok(item)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes_are_distinguishable() {
        assert_ne!(scope(), foreign_scope(), "foreign scope is another account");
        assert_ne!(scope(), retired_scope(), "retired scope is another epoch");
        assert_eq!(
            scope().account,
            retired_scope().account,
            "a retired epoch is the same account"
        );
    }

    #[test]
    fn identities_round_trip_through_their_keys() {
        let id = chat_id(scope(), 100);
        let parsed = ItemId::parse_text(&id.text()).expect("fixture ids are well formed");
        assert_eq!(parsed, id);
        assert_eq!(parsed.key(), id.key());
    }

    #[test]
    fn distinct_fixtures_have_distinct_identities() {
        let ids = [
            account_root_id(scope()),
            chat_list_id(scope(), ChatListKind::Main),
            chat_list_id(scope(), ChatListKind::Archive),
            chat_id(scope(), 100),
            chat_id(scope(), 101),
            chat_appearance_id(scope(), 100, ChatListKind::Main),
            year_dir_id(scope(), 100, 2026),
            media_dir_id(scope(), 100, 2026),
            attachment_id(scope(), 100, 5, 0),
            attachment_id(scope(), 100, 5, 1),
        ];
        for (i, left) in ids.iter().enumerate() {
            for right in &ids[i + 1..] {
                assert_ne!(left, right, "fixture identities must not collide");
            }
        }
    }

    #[test]
    fn an_appearance_differs_from_its_canonical_item() {
        assert_ne!(
            chat_id(scope(), 100),
            chat_appearance_id(scope(), 100, ChatListKind::Main),
            "DOM-022: an appearance is not its canonical item"
        );
    }

    #[test]
    fn scope_is_part_of_identity() {
        assert_ne!(
            chat_id(scope(), 100),
            chat_id(foreign_scope(), 100),
            "the same chat in another account is another item"
        );
        assert_ne!(
            chat_id(scope(), 100),
            chat_id(retired_scope(), 100),
            "a namespace epoch retires identities"
        );
    }

    #[test]
    fn directory_fixtures_carry_directory_content() {
        let item = directory(
            account_root_id(scope()),
            None,
            "Account",
            "m1",
            DirectoryKind::Root,
        )
        .expect("valid fixture");
        assert!(item.is_directory());
        assert_eq!(item.parent, None);
        assert_eq!(item.display_name, "Account");
        assert!(item.capabilities().enumerate_children);
    }

    #[test]
    fn file_fixtures_are_fetchable_and_sized() {
        let item = file(
            attachment_id(scope(), 100, 5, 0),
            year_dir_id(scope(), 100, 2026),
            "photo.jpg",
            "m2",
            "c1",
            2048,
            FileKind::Attachment,
        )
        .expect("valid fixture");
        assert!(!item.is_directory());
        assert!(item.capabilities().read_content);
        match &item.content {
            ItemContent::File(facts) => {
                assert_eq!(facts.size, Some(2048));
                assert_eq!(facts.availability, ContentAvailability::Fetchable);
                assert_eq!(facts.content_version.as_str(), "c1");
            }
            ItemContent::Directory(_) => unreachable!("file fixture is a file"),
        }
    }

    #[test]
    fn restricted_fixtures_advertise_nothing() {
        let item = restricted_file(
            attachment_id(scope(), 100, 5, 0),
            year_dir_id(scope(), 100, 2026),
            "secret.jpg",
            "m2",
            "c1",
            2048,
            FileKind::Attachment,
        )
        .expect("valid fixture");
        let caps = item.capabilities();
        assert!(
            !caps.read_content,
            "POL-4: restricted bytes are not readable"
        );
        assert!(!caps.enumerate_children);
    }

    #[test]
    fn invalid_version_tokens_are_rejected_not_panicked_on() {
        let error = directory(
            account_root_id(scope()),
            None,
            "Account",
            "",
            DirectoryKind::Root,
        )
        .expect_err("an empty version token is invalid");
        assert!(matches!(error, InvalidVersionToken::Empty));
    }
}
