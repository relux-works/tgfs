//! Pinned format-v1 encodings and parser error paths (TASK-260715-1qz1g5).
//!
//! The golden table is the app-update stability contract in executable form
//! (DOM-020): an identity minted today must parse identically in every
//! future build. Any code change that alters an encoding fails these tests;
//! the correct response is a new format version, never an edit to a v1
//! expectation. Update an expectation only if the format has never shipped.

use gramdrive_model::identity::{
    AccountId, AccountKey, AccountScope, AppearanceKey, AttachmentIndex, AttachmentKey, BlobKey,
    CanonicalKey, ChatId, ChatKey, ChatListKey, ChatListKind, ContentHash, DocFormat, DocPartition,
    FolderCatalogKey, FolderId, GeneratedDocKey, IdParseError, ItemId, ItemKey, MediaDirKey,
    MessageId, MessageKey, NamespaceVersion, SchemaFamily, YearDirKey,
};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// Result rather than panicking here: the workspace `expect_used` lint is
// only relaxed inside #[test] functions, so callers unwrap.
fn unhex(text: &str) -> Result<Vec<u8>, std::num::ParseIntError> {
    assert!(text.len().is_multiple_of(2), "odd hex literal");
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16))
        .collect()
}

const ACCOUNT: AccountKey = AccountKey {
    account_id: AccountId(42),
};

const SCOPE: AccountScope = AccountScope {
    account: ACCOUNT,
    namespace_version: NamespaceVersion(1),
};

const CHAT: ChatKey = ChatKey {
    scope: SCOPE,
    chat_id: ChatId(-1001234567890),
};

const MESSAGE: MessageKey = MessageKey {
    chat: CHAT,
    message_id: MessageId(777000),
};

struct Golden {
    name: &'static str,
    key: ItemKey,
    bytes_hex: &'static str,
    text: &'static str,
}

fn goldens() -> Vec<Golden> {
    vec![
        Golden {
            name: "account",
            key: ItemKey::Canonical(CanonicalKey::Account(ACCOUNT)),
            bytes_hex: "0101000000000000002a",
            text: "gdaeaqaaaaaaaaaabk",
        },
        Golden {
            name: "chat_list_main",
            key: ItemKey::Canonical(CanonicalKey::ChatList(ChatListKey {
                scope: SCOPE,
                kind: ChatListKind::Main,
            })),
            bytes_hex: "0102000000000000002a0000000101",
            text: "gdaebaaaaaaaaaaabkaaaaaaib",
        },
        Golden {
            name: "chat_list_folder",
            key: ItemKey::Canonical(CanonicalKey::ChatList(ChatListKey {
                scope: SCOPE,
                kind: ChatListKind::Folder(FolderId(-7)),
            })),
            bytes_hex: "0102000000000000002a0000000103fffffff9",
            text: "gdaebaaaaaaaaaaabkaaaaaaid777776i",
        },
        Golden {
            name: "chat",
            key: ItemKey::Canonical(CanonicalKey::Chat(CHAT)),
            bytes_hex: "0103000000000000002a00000001ffffff16e1c4ed2e",
            text: "gdaebqaaaaaaaaaabkaaaaaap7777rnyoe5uxa",
        },
        Golden {
            name: "message",
            key: ItemKey::Canonical(CanonicalKey::Message(MESSAGE)),
            bytes_hex: "0104000000000000002a00000001ffffff16e1c4ed2e00000000000bdb28",
            text: "gdaecaaaaaaaaaaabkaaaaaap7777rnyoe5uxaaaaaaaaaxwzi",
        },
        Golden {
            name: "attachment",
            key: ItemKey::Canonical(CanonicalKey::Attachment(AttachmentKey {
                message: MESSAGE,
                index: AttachmentIndex(2),
            })),
            bytes_hex: "0105000000000000002a00000001ffffff16e1c4ed2e00000000000bdb2800000002",
            text: "gdaecqaaaaaaaaaabkaaaaaap7777rnyoe5uxaaaaaaaaaxwziaaaaaaq",
        },
        Golden {
            name: "generated_doc_month_ndjson",
            key: ItemKey::Canonical(CanonicalKey::GeneratedDoc(GeneratedDocKey {
                chat: CHAT,
                partition: DocPartition::Month {
                    year: 2026,
                    month: 7,
                },
                format: DocFormat::Ndjson,
                schema_family: SchemaFamily(1),
            })),
            bytes_hex: "0106000000000000002a00000001ffffff16e1c4ed2e0307ea07010001",
            text: "gdaedaaaaaaaaaaabkaaaaaap7777rnyoe5uxagb7ka4aqaai",
        },
        Golden {
            name: "generated_doc_whole_chat_markdown",
            key: ItemKey::Canonical(CanonicalKey::GeneratedDoc(GeneratedDocKey {
                chat: CHAT,
                partition: DocPartition::Chat,
                format: DocFormat::Markdown,
                schema_family: SchemaFamily(65535),
            })),
            bytes_hex: "0106000000000000002a00000001ffffff16e1c4ed2e0102ffff",
            text: "gdaedaaaaaaaaaaabkaaaaaap7777rnyoe5uxacax774",
        },
        Golden {
            name: "folder_catalog",
            key: ItemKey::Canonical(CanonicalKey::FolderCatalog(FolderCatalogKey {
                scope: SCOPE,
            })),
            bytes_hex: "0108000000000000002a00000001",
            text: "gdaeeaaaaaaaaaaabkaaaaaai",
        },
        Golden {
            name: "year_dir",
            key: ItemKey::Canonical(CanonicalKey::YearDir(YearDirKey {
                chat: CHAT,
                year: 2026,
            })),
            bytes_hex: "0109000000000000002a00000001ffffff16e1c4ed2e07ea",
            text: "gdaeeqaaaaaaaaaabkaaaaaap7777rnyoe5uxap2q",
        },
        Golden {
            name: "media_dir_appearance_in_main",
            key: ItemKey::Appearance(AppearanceKey {
                view: ChatListKind::Main,
                item: CanonicalKey::MediaDir(MediaDirKey {
                    chat: CHAT,
                    year: 2026,
                }),
            }),
            bytes_hex: "0110010a000000000000002a00000001ffffff16e1c4ed2e07ea",
            text: "gdaeiaccqaaaaaaaaaaavaaaaaah7777yw4hco2lqh5i",
        },
        Golden {
            name: "generated_doc_chat_json",
            key: ItemKey::Canonical(CanonicalKey::GeneratedDoc(GeneratedDocKey {
                chat: CHAT,
                partition: DocPartition::Chat,
                format: DocFormat::Json,
                schema_family: SchemaFamily(1),
            })),
            bytes_hex: "0106000000000000002a00000001ffffff16e1c4ed2e01030001",
            text: "gdaedaaaaaaaaaaabkaaaaaap7777rnyoe5uxacayaae",
        },
        Golden {
            name: "blob_sha256",
            key: ItemKey::Canonical(CanonicalKey::Blob(BlobKey {
                account: ACCOUNT,
                hash: ContentHash::Sha256([
                    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c,
                    0x0d, 0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19,
                    0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f,
                ]),
            })),
            bytes_hex: "0107000000000000002a01000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
            text: "gdaedqaaaaaaaaaabkaeaacaqdaqcqmbyibefawdanbyhraeiscmkbkfqxdamrugy4dupb6",
        },
        Golden {
            name: "appearance_chat_in_archive",
            key: ItemKey::Appearance(AppearanceKey {
                view: ChatListKind::Archive,
                item: CanonicalKey::Chat(CHAT),
            }),
            bytes_hex: "01100203000000000000002a00000001ffffff16e1c4ed2e",
            text: "gdaeiaeayaaaaaaaaaaavaaaaaah7777yw4hco2lq",
        },
        Golden {
            name: "appearance_attachment_in_folder",
            key: ItemKey::Appearance(AppearanceKey {
                view: ChatListKind::Folder(FolderId(5)),
                item: CanonicalKey::Attachment(AttachmentKey {
                    message: MESSAGE,
                    index: AttachmentIndex(0),
                }),
            }),
            bytes_hex: "0110030000000505000000000000002a00000001ffffff16e1c4ed2e00000000000bdb2800000000",
            text: "gdaeiagaaaaacqkaaaaaaaaaaafiaaaaab77776fxbytws4aaaaaaaac63faaaaaaa",
        },
        Golden {
            name: "extreme_values",
            key: ItemKey::Appearance(AppearanceKey {
                view: ChatListKind::Folder(FolderId(i32::MIN)),
                item: CanonicalKey::Attachment(AttachmentKey {
                    message: MessageKey {
                        chat: ChatKey {
                            scope: AccountScope {
                                account: AccountKey {
                                    account_id: AccountId(i64::MIN),
                                },
                                namespace_version: NamespaceVersion(u32::MAX),
                            },
                            chat_id: ChatId(i64::MAX),
                        },
                        message_id: MessageId(-1),
                    },
                    index: AttachmentIndex(u32::MAX),
                }),
            }),
            bytes_hex: "01100380000000058000000000000000ffffffff7fffffffffffffffffffffffffffffffffffffff",
            text: "gdaeiahaaaaaaalaaaaaaaaaaaad777777p7777777777777777777777777777777",
        },
    ]
}

/// Every golden key encodes to exactly the pinned bytes and text.
#[test]
fn golden_encodings_are_pinned() {
    let mut failures = Vec::new();
    for golden in goldens() {
        let id = golden.key.id();
        let actual_hex = hex(id.as_bytes());
        let actual_text = id.text();
        if actual_hex != golden.bytes_hex || actual_text != golden.text {
            failures.push(format!(
                "{}:\n            bytes_hex: \"{actual_hex}\",\n            text: \"{actual_text}\",",
                golden.name
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "encodings drifted from the pinned v1 goldens:\n{}",
        failures.join("\n")
    );
}

/// Every pinned serialization parses back to its original key — through
/// bytes and through text.
#[test]
fn golden_serializations_parse_back() {
    for golden in goldens() {
        let pinned = unhex(golden.bytes_hex).expect("golden hex literal");
        let from_bytes = ItemId::parse_bytes(&pinned).expect(golden.name);
        assert_eq!(from_bytes.key(), golden.key, "{} via bytes", golden.name);
        let from_text = ItemId::parse_text(golden.text).expect(golden.name);
        assert_eq!(from_text.key(), golden.key, "{} via text", golden.name);
        assert_eq!(from_bytes, from_text, "{} bytes/text agree", golden.name);
    }
}

/// Ids behave as opaque values: equal across construction paths and usable
/// as hash keys.
#[test]
fn ids_are_value_equal_across_construction_paths() {
    let built = ItemKey::Canonical(CanonicalKey::Chat(CHAT)).id();
    let parsed = ItemId::parse_text(&built.text()).expect("own text form parses");
    assert_eq!(built, parsed);
    let set: std::collections::HashSet<ItemId> = [built.clone(), parsed].into_iter().collect();
    assert_eq!(set.len(), 1);
    assert_eq!(built.to_string(), built.text());
}

#[test]
fn empty_and_truncated_payloads_are_rejected() {
    assert_eq!(
        ItemId::parse_bytes(&[]).map(|id| id.key()),
        Err(IdParseError::Truncated)
    );
    assert_eq!(
        ItemId::parse_bytes(&[0x01]).map(|id| id.key()),
        Err(IdParseError::Truncated)
    );
    // "gd" with an empty payload decodes to zero bytes, then fails as a
    // truncated key rather than as a text error.
    assert_eq!(
        ItemId::parse_text("gd").map(|id| id.key()),
        Err(IdParseError::Truncated)
    );
}

#[test]
fn unknown_tags_name_the_field() {
    let cases: [(&[u8], u8, &str); 5] = [
        // Unknown item kind.
        (&[0x01, 0x7f], 0x7f, "item kind"),
        // Appearance wrapping an appearance is not representable: the inner
        // tag position accepts canonical kinds only.
        (&[0x01, 0x10, 0x01, 0x10], 0x10, "canonical item kind"),
        // Chat list kind inside an appearance view.
        (&[0x01, 0x10, 0x09], 0x09, "chat list kind"),
        // Doc partition: chat key (20 bytes) then a bad partition tag.
        (
            &[
                0x01, 0x06, 0, 0, 0, 0, 0, 0, 0, 42, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 9, 0x2a,
            ],
            0x2a,
            "doc partition",
        ),
        // Content hash algorithm: account key (8 bytes) then a bad hash tag.
        (
            &[0x01, 0x07, 0, 0, 0, 0, 0, 0, 0, 42, 0x02],
            0x02,
            "content hash algorithm",
        ),
    ];
    for (bytes, tag, field) in cases {
        assert_eq!(
            ItemId::parse_bytes(bytes).map(|id| id.key()),
            Err(IdParseError::UnknownTag { tag, field }),
            "field {field}"
        );
    }
}

#[test]
fn doc_format_tag_is_validated() {
    // Chat key, then Month partition (year 2026, month 7), then a bad
    // format tag.
    let mut bytes = vec![0x01, 0x06];
    bytes.extend_from_slice(&42i64.to_be_bytes());
    bytes.extend_from_slice(&1u32.to_be_bytes());
    bytes.extend_from_slice(&9i64.to_be_bytes());
    bytes.extend_from_slice(&[0x03]);
    bytes.extend_from_slice(&2026u16.to_be_bytes());
    bytes.extend_from_slice(&[7, 0x00]);
    assert_eq!(
        ItemId::parse_bytes(&bytes).map(|id| id.key()),
        Err(IdParseError::UnknownTag {
            tag: 0x00,
            field: "doc format"
        })
    );
}

#[test]
fn text_prefix_is_case_sensitive() {
    assert_eq!(
        ItemId::parse_text("GDmzxq").map(|id| id.key()),
        Err(IdParseError::MissingPrefix)
    );
}

#[test]
fn parse_errors_render_diagnostics() {
    let cases: [(IdParseError, &str); 4] = [
        (
            IdParseError::UnsupportedVersion { version: 2 },
            "unsupported item id format version 2",
        ),
        (
            IdParseError::UnknownTag {
                tag: 0x7f,
                field: "item kind",
            },
            "unknown tag 0x7f for item kind",
        ),
        (
            IdParseError::TrailingBytes { extra: 3 },
            "item id payload has 3 trailing byte(s)",
        ),
        (
            IdParseError::InvalidCharacter { position: 4 },
            "invalid base32 character at byte 4",
        ),
    ];
    for (error, expected) in cases {
        assert_eq!(error.to_string(), expected);
    }
}
