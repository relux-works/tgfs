//! Property suite for stable item identities (TASK-260715-1qz1g5).
//!
//! Proves the acceptance criteria over sampled key space:
//! determinism and round-tripping (bytes and text), namespace separation
//! (canonical vs appearance, view vs view, epoch vs epoch, kind vs kind via
//! full injectivity), version gating, and parser strictness (prefix-free,
//! no trailing bytes, canonical text only).
//!
//! No-path/title/order dependence is structural — no key type carries a
//! string or an ordering position — so there is no input through which a
//! rename could reach an encoding; see the identity module docs.

use gramdrive_model::identity::{
    AccountId, AccountKey, AccountScope, AppearanceKey, AttachmentIndex, AttachmentKey, BlobKey,
    CanonicalKey, ChatId, ChatKey, ChatListKey, ChatListKind, ContentHash, DocFormat, DocPartition,
    FolderId, GeneratedDocKey, IdParseError, ItemId, ItemKey, MessageId, MessageKey,
    NamespaceVersion, SchemaFamily,
};
use proptest::prelude::*;

fn arb_account() -> impl Strategy<Value = AccountKey> {
    any::<i64>().prop_map(|id| AccountKey {
        account_id: AccountId(id),
    })
}

fn arb_scope() -> impl Strategy<Value = AccountScope> {
    (arb_account(), any::<u32>()).prop_map(|(account, ns)| AccountScope {
        account,
        namespace_version: NamespaceVersion(ns),
    })
}

fn arb_list_kind() -> impl Strategy<Value = ChatListKind> {
    prop_oneof![
        Just(ChatListKind::Main),
        Just(ChatListKind::Archive),
        any::<i32>().prop_map(|id| ChatListKind::Folder(FolderId(id))),
    ]
}

fn arb_chat() -> impl Strategy<Value = ChatKey> {
    (arb_scope(), any::<i64>()).prop_map(|(scope, id)| ChatKey {
        scope,
        chat_id: ChatId(id),
    })
}

fn arb_message() -> impl Strategy<Value = MessageKey> {
    (arb_chat(), any::<i64>()).prop_map(|(chat, id)| MessageKey {
        chat,
        message_id: MessageId(id),
    })
}

fn arb_partition() -> impl Strategy<Value = DocPartition> {
    prop_oneof![
        Just(DocPartition::Chat),
        any::<u16>().prop_map(|year| DocPartition::Year { year }),
        (any::<u16>(), any::<u8>()).prop_map(|(year, month)| DocPartition::Month { year, month }),
    ]
}

fn arb_canonical() -> impl Strategy<Value = CanonicalKey> {
    prop_oneof![
        arb_account().prop_map(CanonicalKey::Account),
        (arb_scope(), arb_list_kind())
            .prop_map(|(scope, kind)| CanonicalKey::ChatList(ChatListKey { scope, kind })),
        arb_chat().prop_map(CanonicalKey::Chat),
        arb_message().prop_map(CanonicalKey::Message),
        (arb_message(), any::<u32>()).prop_map(|(message, index)| {
            CanonicalKey::Attachment(AttachmentKey {
                message,
                index: AttachmentIndex(index),
            })
        }),
        (
            arb_chat(),
            arb_partition(),
            prop_oneof![Just(DocFormat::Ndjson), Just(DocFormat::Markdown)],
            any::<u16>()
        )
            .prop_map(|(chat, partition, format, family)| {
                CanonicalKey::GeneratedDoc(GeneratedDocKey {
                    chat,
                    partition,
                    format,
                    schema_family: SchemaFamily(family),
                })
            }),
        (arb_account(), any::<[u8; 32]>()).prop_map(|(account, digest)| {
            CanonicalKey::Blob(BlobKey {
                account,
                hash: ContentHash::Sha256(digest),
            })
        }),
    ]
}

fn arb_item_key() -> impl Strategy<Value = ItemKey> {
    prop_oneof![
        arb_canonical().prop_map(ItemKey::Canonical),
        (arb_list_kind(), arb_canonical())
            .prop_map(|(view, item)| ItemKey::Appearance(AppearanceKey { view, item })),
    ]
}

proptest! {
    /// Round-trip through the binary form: parse(encode(k)) == k.
    ///
    /// Together with `distinct_keys_never_collide` this is also the
    /// collision-freedom proof: decoding is a function, so two keys sharing
    /// an encoding would have to be equal.
    #[test]
    fn round_trips_through_bytes(key in arb_item_key()) {
        let id = key.id();
        let parsed = ItemId::parse_bytes(id.as_bytes());
        prop_assert_eq!(parsed.as_ref().map(ItemId::key), Ok(key));
        prop_assert_eq!(parsed, Ok(id));
    }

    /// Round-trip through the text form, including text canonicality:
    /// the parsed id re-serializes to the identical string.
    #[test]
    fn round_trips_through_text(key in arb_item_key()) {
        let id = key.id();
        let text = id.text();
        let parsed = ItemId::parse_text(&text);
        prop_assert_eq!(parsed.as_ref().map(ItemId::key), Ok(key));
        prop_assert_eq!(parsed.map(|p| p.text()), Ok(text));
    }

    /// Independent serializations of equal keys agree byte for byte — the
    /// restart/rebuild stability of DOM-020 as far as a test can state it.
    #[test]
    fn encoding_is_deterministic(key in arb_item_key()) {
        let first = key.id();
        let second = key.id();
        prop_assert_eq!(first.as_bytes(), second.as_bytes());
        prop_assert_eq!(first.text(), second.text());
    }

    /// Full injectivity sample: distinct keys — same kind or different kind —
    /// never share a binary or text identity.
    #[test]
    fn distinct_keys_never_collide(a in arb_item_key(), b in arb_item_key()) {
        prop_assume!(a != b);
        prop_assert_ne!(a.id(), b.id());
        prop_assert_ne!(a.id().text(), b.id().text());
    }

    /// DOM-002: the canonical identity and any appearance of the same item
    /// live in separate namespaces.
    #[test]
    fn canonical_and_appearance_namespaces_are_separate(
        item in arb_canonical(),
        view in arb_list_kind(),
    ) {
        let canonical = ItemKey::Canonical(item).id();
        let appearance = ItemKey::Appearance(AppearanceKey { view, item }).id();
        prop_assert_ne!(canonical, appearance);
    }

    /// PRD-013/DOM-022: one canonical chat seen through Main, Archive, and a
    /// folder yields three distinct appearance identities, while the wrapped
    /// canonical key stays identical.
    #[test]
    fn views_separate_appearances_without_touching_canonical_identity(
        item in arb_canonical(),
        folder in any::<i32>(),
    ) {
        let views = [
            ChatListKind::Main,
            ChatListKind::Archive,
            ChatListKind::Folder(FolderId(folder)),
        ];
        let ids: Vec<ItemId> = views
            .iter()
            .map(|view| ItemKey::Appearance(AppearanceKey { view: *view, item }).id())
            .collect();
        prop_assert_ne!(&ids[0], &ids[1]);
        prop_assert_ne!(&ids[0], &ids[2]);
        prop_assert_ne!(&ids[1], &ids[2]);
        for id in ids {
            match id.key() {
                ItemKey::Appearance(parsed) => prop_assert_eq!(parsed.item, item),
                ItemKey::Canonical(_) => prop_assert!(false, "appearance decoded as canonical"),
            }
        }
    }

    /// DOM-021: bumping the namespace epoch retires a derived identity.
    #[test]
    fn namespace_version_scopes_derived_identities(
        account in arb_account(),
        ns_a in any::<u32>(),
        ns_b in any::<u32>(),
        chat_id in any::<i64>(),
    ) {
        prop_assume!(ns_a != ns_b);
        let chat = |ns| ItemKey::Canonical(CanonicalKey::Chat(ChatKey {
            scope: AccountScope { account, namespace_version: NamespaceVersion(ns) },
            chat_id: ChatId(chat_id),
        }));
        prop_assert_ne!(chat(ns_a).id(), chat(ns_b).id());
    }

    /// The encoding is self-delimiting: no proper prefix of a valid identity
    /// is itself valid, so ids can never be confused by truncation.
    #[test]
    fn proper_prefixes_never_parse(key in arb_item_key()) {
        let id = key.id();
        let bytes = id.as_bytes();
        for len in 0..bytes.len() {
            prop_assert!(ItemId::parse_bytes(&bytes[..len]).is_err());
        }
    }

    /// Strict length: any appended byte is rejected, not ignored.
    #[test]
    fn trailing_bytes_never_parse(key in arb_item_key(), extra in any::<u8>()) {
        let mut bytes = key.id().as_bytes().to_vec();
        bytes.push(extra);
        prop_assert_eq!(
            ItemId::parse_bytes(&bytes).map(|id| id.key()),
            Err(IdParseError::TrailingBytes { extra: 1 })
        );
    }

    /// Version compatibility gate: only format version 1 parses today, and
    /// every other version byte fails with `UnsupportedVersion` — the
    /// contract that lets a future format coexist without ambiguity.
    #[test]
    fn foreign_version_bytes_are_rejected(key in arb_item_key(), version in any::<u8>()) {
        prop_assume!(version != 1);
        let mut bytes = key.id().as_bytes().to_vec();
        bytes[0] = version;
        prop_assert_eq!(
            ItemId::parse_bytes(&bytes).map(|id| id.key()),
            Err(IdParseError::UnsupportedVersion { version })
        );
    }

    /// The text form stays inside the documented alphabet ("gd" prefix plus
    /// lowercase base32), so it is filesystem-, URL-, and log-safe, and a
    /// case-folding environment cannot produce two spellings of one id.
    #[test]
    fn text_form_is_prefixed_lowercase_base32(key in arb_item_key()) {
        let text = key.id().text();
        prop_assert!(text.starts_with("gd"));
        prop_assert!(
            text.bytes().all(|b| b.is_ascii_lowercase() || (b'2'..=b'7').contains(&b))
        );
    }

    /// Only the canonical spelling parses: uppercasing any letter breaks it.
    #[test]
    fn uppercased_text_never_parses(key in arb_item_key()) {
        let text = key.id().text().to_ascii_uppercase();
        prop_assert!(ItemId::parse_text(&text).is_err());
    }

    /// Identity payloads stay small: every provider carrier (Apple string
    /// ids, Android document ids, Windows 4 KiB file-identity blobs) has
    /// orders of magnitude of headroom.
    #[test]
    fn encoded_size_is_bounded(key in arb_item_key()) {
        prop_assert!(key.id().as_bytes().len() <= 64);
        prop_assert!(key.id().text().len() <= 128);
    }
}
