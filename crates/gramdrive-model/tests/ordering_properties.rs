//! Property suite for the ordering projection (TASK-260715-1jmsdp).
//!
//! Proves over sampled input what the fixtures show by example: the document
//! is a pure function of the input *set*, positions never reach identity or
//! names, and the rendered JSON is well-formed for every title Telegram can
//! send — including the ones designed to break a writer.

use gramdrive_model::identity::{
    AccountId, AccountKey, AccountScope, ChatId, ChatListKey, ChatListKind, FolderId,
    NamespaceVersion, SchemaFamily,
};
use gramdrive_model::ordering::{ChatOrderRecord, ChatPosition, OrderInputError, OrderProjection};
use proptest::prelude::*;

fn arb_list() -> impl Strategy<Value = ChatListKey> {
    (
        any::<i64>(),
        any::<u32>(),
        prop_oneof![
            Just(ChatListKind::Main),
            Just(ChatListKind::Archive),
            any::<i32>().prop_map(|id| ChatListKind::Folder(FolderId(id))),
        ],
    )
        .prop_map(|(account, version, kind)| ChatListKey {
            scope: AccountScope {
                account: AccountKey {
                    account_id: AccountId(account),
                },
                namespace_version: NamespaceVersion(version),
            },
            kind,
        })
}

/// Titles worth sampling: ordinary text, plus the shapes that break naive
/// writers — quotes, backslashes, controls, newlines, Unicode, and empty.
fn arb_title() -> impl Strategy<Value = String> {
    prop_oneof![
        "[a-zA-Z0-9 ]{0,20}",
        Just(String::new()),
        Just("\"quoted\"".to_string()),
        Just("back\\slash".to_string()),
        Just("line\nbreak\ttab".to_string()),
        Just("control\u{01}\u{1f}char".to_string()),
        Just("Привет 👨‍👩‍👧‍👦".to_string()),
        Just("order.json".to_string()),
        Just("../../etc/passwd".to_string()),
        ".{0,30}",
    ]
}

fn arb_record() -> impl Strategy<Value = ChatOrderRecord> {
    (
        any::<i64>(),
        arb_title(),
        proptest::option::of("[a-z_]{1,12}"),
        any::<i64>(),
        any::<bool>(),
    )
        .prop_map(
            |(chat_id, title, username, order, is_pinned)| ChatOrderRecord {
                chat_id: ChatId(chat_id),
                title,
                username,
                position: ChatPosition { order, is_pinned },
            },
        )
}

/// Records with distinct chat IDs — the projection's input contract.
fn arb_records() -> impl Strategy<Value = Vec<ChatOrderRecord>> {
    proptest::collection::vec(arb_record(), 0..8).prop_map(|mut records| {
        let mut seen = std::collections::HashSet::new();
        records.retain(|record| seen.insert(record.chat_id.0));
        records
    })
}

// Returns the Result rather than unwrapping: the `expect_used` lint is only
// relaxed inside #[test] functions, and proptest bodies are #[test] bodies.
fn project(
    list: ChatListKey,
    records: Vec<ChatOrderRecord>,
) -> Result<OrderProjection, OrderInputError> {
    OrderProjection::new(list, SchemaFamily(1), records)
}

proptest! {
    /// The document depends on the input set, not on its order.
    #[test]
    fn rendering_is_independent_of_record_order(
        list in arb_list(),
        records in arb_records(),
        seed in any::<u64>(),
    ) {
        let expected = project(list, records.clone()).unwrap().to_json();

        // A deterministic shuffle driven by the sampled seed.
        let mut shuffled = records;
        let len = shuffled.len();
        for index in 0..len {
            let swap = ((seed >> (index % 64)) as usize).wrapping_add(index) % len.max(1);
            shuffled.swap(index, swap);
        }
        prop_assert_eq!(project(list, shuffled).unwrap().to_json(), expected);
    }

    /// Ranks are exactly 0..n, and the sequence obeys (order, chat id)
    /// descending.
    #[test]
    fn ranks_are_dense_and_correctly_sorted(list in arb_list(), records in arb_records()) {
        let projection = project(list, records).unwrap();
        let entries = projection.entries();

        for (index, entry) in entries.iter().enumerate() {
            prop_assert_eq!(entry.rank, index);
        }
        for pair in entries.windows(2) {
            let left = (pair[0].position.order, pair[0].chat.chat_id.0);
            let right = (pair[1].position.order, pair[1].chat.chat_id.0);
            prop_assert!(left > right, "{:?} must outrank {:?}", left, right);
        }
    }

    /// Positions cannot reach identity or names: repositioning the same chats
    /// yields the same (id, name) set, only reordered (POL-1, DOM-005).
    #[test]
    fn positions_never_reach_identity_or_names(
        list in arb_list(),
        records in arb_records(),
        orders in proptest::collection::vec(any::<i64>(), 0..8),
    ) {
        let before = project(list, records.clone()).unwrap();

        let mut moved = records;
        for (record, order) in moved.iter_mut().zip(orders) {
            record.position.order = order;
        }
        let after = project(list, moved).unwrap();

        let mut before_pairs: Vec<(String, String)> = before
            .entries()
            .iter()
            .map(|entry| (entry.id.text(), entry.name.as_str().to_string()))
            .collect();
        let mut after_pairs: Vec<(String, String)> = after
            .entries()
            .iter()
            .map(|entry| (entry.id.text(), entry.name.as_str().to_string()))
            .collect();
        before_pairs.sort();
        after_pairs.sort();
        prop_assert_eq!(before_pairs, after_pairs);
        // The document itself is the same file throughout.
        prop_assert_eq!(before.doc_id(), after.doc_id());
    }

    /// Names are unique within a list root, and none of them is the reserved
    /// document name — the two things a directory enumeration cannot survive.
    #[test]
    fn projected_names_are_unique_and_never_shadow_the_document(
        list in arb_list(),
        records in arb_records(),
    ) {
        let projection = project(list, records).unwrap();
        let names: Vec<&str> = projection
            .entries()
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();

        let unique: std::collections::HashSet<&&str> = names.iter().collect();
        prop_assert_eq!(unique.len(), names.len(), "duplicate names: {:?}", names);
        prop_assert!(
            !names.contains(&gramdrive_model::ordering::ORDER_FILE_NAME),
            "a chat took the reserved name: {:?}",
            names
        );
    }

    /// A duplicated chat is rejected wherever it lands in the sort — the
    /// records between the two copies must not hide the second one.
    #[test]
    fn duplicate_chats_are_rejected_at_any_distance(
        list in arb_list(),
        records in arb_records(),
        index in 0usize..8,
        order in any::<i64>(),
    ) {
        prop_assume!(!records.is_empty());
        let victim = records[index % records.len()].clone();

        let mut duplicated = records;
        // A second record for the same chat at an unrelated position, so the
        // two copies are separated by whatever sorts between them.
        duplicated.push(ChatOrderRecord {
            position: ChatPosition { order, ..victim.position },
            ..victim.clone()
        });

        prop_assert_eq!(
            project(list, duplicated),
            Err(OrderInputError::DuplicateChat { chat: victim.chat_id })
        );
    }

    /// Whatever the titles, the rendered document is well-formed JSON that
    /// parses back to the projected order.
    #[test]
    fn rendered_document_is_well_formed_json(list in arb_list(), records in arb_records()) {
        let projection = project(list, records).unwrap();
        let json = projection.to_json();
        let parsed = mini_json::parse(&json).map_err(|error| {
            TestCaseError::fail(format!("invalid JSON: {error}\n{json}"))
        })?;

        let chats = parsed.get("chats").and_then(mini_json::Value::as_array);
        let chats = chats.ok_or_else(|| TestCaseError::fail("missing chats array"))?;
        prop_assert_eq!(chats.len(), projection.entries().len());

        for (value, entry) in chats.iter().zip(projection.entries()) {
            prop_assert_eq!(
                value.get("name").and_then(mini_json::Value::as_str),
                Some(entry.name.as_str())
            );
            let order = entry.position.order.to_string();
            prop_assert_eq!(
                value.get("order").and_then(mini_json::Value::as_str),
                Some(order.as_str())
            );
        }
    }
}

/// A minimal JSON reader, used only to check that what the projection writes
/// is really JSON.
///
/// Independent of the writer on purpose: asserting the output against a
/// string the same code built would prove nothing. It is a reader rather than
/// `serde_json` because a dev-dependency is still supply-chain surface
/// (POL-6), and the properties above need exactly three things back —
/// objects, arrays, and correctly *unescaped* strings, which is where a
/// writer bug would show.
mod mini_json {
    use std::collections::BTreeMap;

    #[derive(Debug, Clone, PartialEq)]
    pub(crate) enum Value {
        Null,
        Bool(bool),
        Number(String),
        Str(String),
        Array(Vec<Value>),
        Object(BTreeMap<String, Value>),
    }

    impl Value {
        pub(crate) fn get(&self, key: &str) -> Option<&Value> {
            match self {
                Value::Object(map) => map.get(key),
                _ => None,
            }
        }

        pub(crate) fn as_array(&self) -> Option<&Vec<Value>> {
            match self {
                Value::Array(items) => Some(items),
                _ => None,
            }
        }

        pub(crate) fn as_str(&self) -> Option<&str> {
            match self {
                Value::Str(text) => Some(text),
                _ => None,
            }
        }
    }

    pub(crate) fn parse(text: &str) -> Result<Value, String> {
        let mut parser = Parser {
            chars: text.chars().collect(),
            position: 0,
        };
        parser.skip_whitespace();
        let value = parser.value()?;
        parser.skip_whitespace();
        if parser.position != parser.chars.len() {
            return Err(format!("trailing input at {}", parser.position));
        }
        Ok(value)
    }

    struct Parser {
        chars: Vec<char>,
        position: usize,
    }

    impl Parser {
        fn peek(&self) -> Option<char> {
            self.chars.get(self.position).copied()
        }

        fn next(&mut self) -> Option<char> {
            let character = self.peek();
            self.position += 1;
            character
        }

        fn skip_whitespace(&mut self) {
            while matches!(self.peek(), Some(' ' | '\n' | '\t' | '\r')) {
                self.position += 1;
            }
        }

        fn expect(&mut self, expected: char) -> Result<(), String> {
            match self.next() {
                Some(found) if found == expected => Ok(()),
                found => Err(format!(
                    "expected {expected:?} at {}, found {found:?}",
                    self.position - 1
                )),
            }
        }

        fn value(&mut self) -> Result<Value, String> {
            match self.peek() {
                Some('{') => self.object(),
                Some('[') => self.array(),
                Some('"') => Ok(Value::Str(self.string()?)),
                Some('t') => self.literal("true", Value::Bool(true)),
                Some('f') => self.literal("false", Value::Bool(false)),
                Some('n') => self.literal("null", Value::Null),
                Some(c) if c == '-' || c.is_ascii_digit() => self.number(),
                found => Err(format!("unexpected {found:?} at {}", self.position)),
            }
        }

        fn literal(&mut self, text: &str, value: Value) -> Result<Value, String> {
            for expected in text.chars() {
                self.expect(expected)?;
            }
            Ok(value)
        }

        fn number(&mut self) -> Result<Value, String> {
            let start = self.position;
            if self.peek() == Some('-') {
                self.position += 1;
            }
            while matches!(self.peek(), Some(c) if c.is_ascii_digit() || c == '.' || c == 'e' || c == 'E' || c == '+' || c == '-')
            {
                self.position += 1;
            }
            if start == self.position {
                return Err(format!("empty number at {start}"));
            }
            Ok(Value::Number(
                self.chars[start..self.position].iter().collect(),
            ))
        }

        fn string(&mut self) -> Result<String, String> {
            self.expect('"')?;
            let mut out = String::new();
            loop {
                match self.next() {
                    None => return Err("unterminated string".to_string()),
                    Some('"') => return Ok(out),
                    Some('\\') => match self.next() {
                        Some('"') => out.push('"'),
                        Some('\\') => out.push('\\'),
                        Some('/') => out.push('/'),
                        Some('b') => out.push('\u{08}'),
                        Some('f') => out.push('\u{0c}'),
                        Some('n') => out.push('\n'),
                        Some('r') => out.push('\r'),
                        Some('t') => out.push('\t'),
                        Some('u') => {
                            let mut code = 0u32;
                            for _ in 0..4 {
                                let digit = self
                                    .next()
                                    .and_then(|c| c.to_digit(16))
                                    .ok_or("bad \\u escape")?;
                                code = code * 16 + digit;
                            }
                            out.push(char::from_u32(code).ok_or("bad code point")?);
                        }
                        found => return Err(format!("bad escape {found:?}")),
                    },
                    // The grammar forbids a raw control character in a string;
                    // this is the check that catches an unescaped one.
                    Some(control) if (control as u32) < 0x20 => {
                        return Err(format!("raw control U+{:04X} in string", control as u32));
                    }
                    Some(other) => out.push(other),
                }
            }
        }

        fn array(&mut self) -> Result<Value, String> {
            self.expect('[')?;
            let mut items = Vec::new();
            self.skip_whitespace();
            if self.peek() == Some(']') {
                self.position += 1;
                return Ok(Value::Array(items));
            }
            loop {
                self.skip_whitespace();
                items.push(self.value()?);
                self.skip_whitespace();
                match self.next() {
                    Some(',') => {}
                    Some(']') => return Ok(Value::Array(items)),
                    found => return Err(format!("expected , or ] found {found:?}")),
                }
            }
        }

        fn object(&mut self) -> Result<Value, String> {
            self.expect('{')?;
            let mut map = BTreeMap::new();
            self.skip_whitespace();
            if self.peek() == Some('}') {
                self.position += 1;
                return Ok(Value::Object(map));
            }
            loop {
                self.skip_whitespace();
                let key = self.string()?;
                self.skip_whitespace();
                self.expect(':')?;
                self.skip_whitespace();
                let value = self.value()?;
                if map.insert(key.clone(), value).is_some() {
                    return Err(format!("duplicate key {key:?}"));
                }
                self.skip_whitespace();
                match self.next() {
                    Some(',') => {}
                    Some('}') => return Ok(Value::Object(map)),
                    found => return Err(format!("expected , or }} found {found:?}")),
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn parses_and_unescapes() {
            let value = parse(r#"{ "a": [1, "x\ny", true, null], "b": "A" }"#).expect("valid");
            assert_eq!(
                value.get("a").and_then(Value::as_array).map(Vec::len),
                Some(4)
            );
            assert_eq!(value.get("b").and_then(Value::as_str), Some("A"));
        }

        #[test]
        fn rejects_raw_controls_and_trailing_input() {
            assert!(parse("\"a\u{01}b\"").is_err());
            assert!(parse("{} {}").is_err());
            assert!(parse(r#"{"a": 1,}"#).is_err());
        }
    }
}
