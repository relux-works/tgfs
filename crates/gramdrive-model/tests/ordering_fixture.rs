//! Ordering projection fixtures (TASK-260715-1jmsdp; POL-1, DEC-013).
//!
//! The acceptance criterion in executable form: a reorder changes the
//! metadata and nothing else — no identity, no name, no path — and a rename
//! is the only thing that moves a folder.

use gramdrive_model::identity::{
    AccountId, AccountKey, AccountScope, ChatId, ChatListKey, ChatListKind, FolderId,
    NamespaceVersion, SchemaFamily,
};
use gramdrive_model::ordering::{
    ChatOrderRecord, ChatPosition, ORDER_FILE_NAME, OrderInputError, OrderProjection,
};

const SCOPE: AccountScope = AccountScope {
    account: AccountKey {
        account_id: AccountId(42),
    },
    namespace_version: NamespaceVersion(1),
};

const MAIN: ChatListKey = ChatListKey {
    scope: SCOPE,
    kind: ChatListKind::Main,
};

const FAMILY: SchemaFamily = SchemaFamily(1);

fn chat(id: i64, title: &str, username: Option<&str>, order: i64, pinned: bool) -> ChatOrderRecord {
    ChatOrderRecord {
        chat_id: ChatId(id),
        title: title.to_string(),
        username: username.map(str::to_string),
        position: ChatPosition {
            order,
            is_pinned: pinned,
        },
    }
}

/// The reference list: a pinned chat on top, then two ordinary ones.
fn baseline() -> Vec<ChatOrderRecord> {
    vec![
        chat(-1001, "Team", Some("team_chat"), 9_000, false),
        chat(
            2002,
            "Alice",
            Some("alice"),
            9_223_372_036_854_775_807,
            true,
        ),
        chat(3003, "Bob", None, 8_000, false),
    ]
}

// Returns the Result rather than unwrapping: the `expect_used` lint is only
// relaxed inside #[test] functions, so callers unwrap.
fn project(chats: Vec<ChatOrderRecord>) -> Result<OrderProjection, OrderInputError> {
    OrderProjection::new(MAIN, FAMILY, chats)
}

fn names(projection: &OrderProjection) -> Vec<String> {
    projection
        .entries()
        .iter()
        .map(|entry| entry.name.as_str().to_string())
        .collect()
}

fn ids(projection: &OrderProjection) -> Vec<String> {
    projection
        .entries()
        .iter()
        .map(|entry| entry.id.text())
        .collect()
}

/// Chats sort by (order, chat id) descending — Telegram's own rule.
#[test]
fn orders_by_position_then_chat_id_descending() {
    let projection = project(baseline()).unwrap();
    assert_eq!(
        names(&projection),
        ["Alice — @alice", "Team — @team_chat", "Bob"]
    );
    assert_eq!(
        projection
            .entries()
            .iter()
            .map(|entry| entry.rank)
            .collect::<Vec<_>>(),
        [0, 1, 2]
    );
}

/// Equal server order is broken by chat ID, descending — never by input order.
#[test]
fn equal_order_breaks_ties_by_chat_id_descending() {
    let projection = project(vec![
        chat(100, "Low id", None, 500, false),
        chat(900, "High id", None, 500, false),
    ])
    .unwrap();
    assert_eq!(names(&projection), ["High id", "Low id"]);
}

/// AC: a reorder rewrites `order.json` and touches nothing else — every
/// identity and every name is byte-identical to the baseline.
#[test]
fn reorder_changes_metadata_only() {
    let before = project(baseline()).unwrap();

    // Bob overtakes Team; nothing else about any chat changes.
    let mut reordered = baseline();
    reordered[2].position.order = 9_500;
    let after = project(reordered).unwrap();

    // The order really did change.
    assert_eq!(
        names(&before),
        ["Alice — @alice", "Team — @team_chat", "Bob"]
    );
    assert_eq!(
        names(&after),
        ["Alice — @alice", "Bob", "Team — @team_chat"]
    );
    assert_ne!(before.to_json(), after.to_json());

    // ...and the identities did not. Same set, same ids, same names.
    let mut before_pairs: Vec<(String, String)> =
        ids(&before).into_iter().zip(names(&before)).collect();
    let mut after_pairs: Vec<(String, String)> =
        ids(&after).into_iter().zip(names(&after)).collect();
    before_pairs.sort();
    after_pairs.sort();
    assert_eq!(
        before_pairs, after_pairs,
        "a reorder must not change any id or name"
    );

    // The document's own identity is stable too: same file, new bytes.
    assert_eq!(before.doc_id(), after.doc_id());
    assert_eq!(before.doc_key(), after.doc_key());
}

/// AC: a rename is the one thing that changes a folder name — and it still
/// leaves identity alone.
#[test]
fn rename_changes_the_name_and_nothing_else() {
    let before = project(baseline()).unwrap();

    let mut renamed = baseline();
    renamed[0].title = "Team Rocket".to_string();
    let after = project(renamed).unwrap();

    assert_eq!(names(&before)[1], "Team — @team_chat");
    assert_eq!(names(&after)[1], "Team Rocket — @team_chat");
    // Identity is title-independent (DOM-005), so the ids are untouched.
    assert_eq!(ids(&before), ids(&after));
    // And the order is untouched: renaming does not move a chat.
    assert_eq!(
        before
            .entries()
            .iter()
            .map(|entry| entry.chat.chat_id.0)
            .collect::<Vec<_>>(),
        after
            .entries()
            .iter()
            .map(|entry| entry.chat.chat_id.0)
            .collect::<Vec<_>>()
    );
}

/// Input order cannot reach the output: the same set shuffled renders the
/// same bytes.
#[test]
fn record_order_does_not_influence_the_document() {
    let forward = project(baseline()).unwrap();
    let mut reversed = baseline();
    reversed.reverse();
    assert_eq!(forward.to_json(), project(reversed).unwrap().to_json());
}

/// A chat titled `order.json` yields; the metadata keeps its POL-1 name.
#[test]
fn a_chat_cannot_shadow_the_order_document() {
    let projection = project(vec![
        chat(7, "order.json", None, 10, false),
        chat(8, "Bob", None, 5, false),
    ])
    .unwrap();
    let resolved = names(&projection);
    assert_ne!(
        resolved[0], ORDER_FILE_NAME,
        "the chat must not be projected onto the reserved name"
    );
    assert!(
        resolved[0].starts_with("order.json ("),
        "the chat should keep its title plus an identity suffix, got {:?}",
        resolved[0]
    );
    assert_eq!(resolved[1], "Bob");
}

/// Two chats that sanitize alike are separated by identity, not by position.
#[test]
fn colliding_titles_are_suffixed_independently_of_order() {
    let first = project(vec![
        chat(11, "Bob", None, 20, false),
        chat(12, "Bob", None, 10, false),
    ])
    .unwrap();
    // Swap their positions; the names must not follow.
    let second = project(vec![
        chat(11, "Bob", None, 10, false),
        chat(12, "Bob", None, 20, false),
    ])
    .unwrap();

    let mut first_pairs: Vec<(i64, String)> = first
        .entries()
        .iter()
        .map(|entry| (entry.chat.chat_id.0, entry.name.as_str().to_string()))
        .collect();
    let mut second_pairs: Vec<(i64, String)> = second
        .entries()
        .iter()
        .map(|entry| (entry.chat.chat_id.0, entry.name.as_str().to_string()))
        .collect();
    first_pairs.sort();
    second_pairs.sort();
    assert_eq!(first_pairs, second_pairs);
    assert_ne!(first_pairs[0].1, first_pairs[1].1, "names must be distinct");
}

/// The rendered document, pinned. Regenerating it is a schema change.
///
/// The `Team "A"` title also shows where quoting is actually settled: the
/// naming policy substitutes `"` long before the writer sees it, so the name
/// reaches JSON already inert. `write_json_string`'s own escaping is tested
/// against hostile input directly, in the module's unit tests.
#[test]
fn renders_the_documented_schema() {
    let projection = project(vec![
        chat(
            2002,
            "Alice",
            Some("alice"),
            9_223_372_036_854_775_807,
            true,
        ),
        chat(-1001, "Team \"A\"", None, 9_000, false),
    ])
    .unwrap();
    let expected = r#"{
  "schema": "gramdrive.order",
  "schema_family": 1,
  "list": { "kind": "main" },
  "chats": [
    {
      "rank": 0,
      "id": "gdaeiacayaaaaaaaaaaavaaaaaaeaaaaaaaaaapuq",
      "chat_id": 2002,
      "name": "Alice — @alice",
      "order": "9223372036854775807",
      "pinned": true
    },
    {
      "rank": 1,
      "id": "gdaeiacayaaaaaaaaaaavaaaaaah7777777777yfy",
      "chat_id": -1001,
      "name": "Team _A_",
      "order": "9000",
      "pinned": false
    }
  ]
}
"#;
    assert_eq!(projection.to_json(), expected);
}

/// An empty list still publishes a well-formed document.
#[test]
fn renders_an_empty_list() {
    let projection = project(Vec::new()).unwrap();
    assert_eq!(
        projection.to_json(),
        r#"{
  "schema": "gramdrive.order",
  "schema_family": 1,
  "list": { "kind": "main" },
  "chats": []
}
"#
    );
}

/// Each list root names itself in its own document.
#[test]
fn renders_each_list_kind() {
    for (kind, expected) in [
        (ChatListKind::Main, r#"{ "kind": "main" }"#),
        (ChatListKind::Archive, r#"{ "kind": "archive" }"#),
        (
            ChatListKind::Folder(FolderId(-7)),
            r#"{ "kind": "folder", "folder_id": -7 }"#,
        ),
    ] {
        let list = ChatListKey { scope: SCOPE, kind };
        let json = OrderProjection::new(list, FAMILY, Vec::new())
            .expect("valid records")
            .to_json();
        assert!(
            json.contains(expected),
            "list {kind:?} should render {expected}, got:\n{json}"
        );
    }
}

/// Every list root has its own ordering document identity.
#[test]
fn each_list_has_a_distinct_order_document() {
    let ids: Vec<String> = [
        ChatListKind::Main,
        ChatListKind::Archive,
        ChatListKind::Folder(FolderId(1)),
        ChatListKind::Folder(FolderId(2)),
    ]
    .into_iter()
    .map(|kind| {
        OrderProjection::new(ChatListKey { scope: SCOPE, kind }, FAMILY, Vec::new())
            .expect("valid records")
            .doc_id()
            .text()
    })
    .collect();
    let unique: std::collections::HashSet<&String> = ids.iter().collect();
    assert_eq!(unique.len(), ids.len(), "order doc ids must be distinct");
}

/// One chat holds one position per list; two records are a source bug.
#[test]
fn duplicate_chat_records_are_rejected() {
    let error = OrderProjection::new(
        MAIN,
        FAMILY,
        vec![
            chat(5, "Bob", None, 10, false),
            chat(5, "Bob again", None, 20, false),
        ],
    )
    .expect_err("duplicate chat");
    assert_eq!(error, OrderInputError::DuplicateChat { chat: ChatId(5) });
    assert_eq!(error.to_string(), "duplicate position record for chat 5");
}

/// A duplicate separated by another chat's position must still be caught:
/// the sort key starts with `order`, so duplicates are NOT adjacent.
#[test]
fn duplicate_chat_records_are_rejected_even_when_not_adjacent() {
    let error = OrderProjection::new(
        MAIN,
        FAMILY,
        vec![
            chat(5, "Bob", None, 20, false),
            chat(9, "Interloper", None, 15, false),
            chat(5, "Bob again", None, 10, false),
        ],
    )
    .expect_err("duplicate chat separated by another position");
    assert_eq!(error, OrderInputError::DuplicateChat { chat: ChatId(5) });
}
