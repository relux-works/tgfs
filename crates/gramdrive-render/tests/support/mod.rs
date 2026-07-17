//! Shared test support: a tiny dependency-free JSON parser (the crate ships no
//! serde, on purpose) and the fixture corpus the golden and unit tests render.
//!
//! The parser exists to prove the renderer's output is *parseable* JSON
//! (SYNC-030 / the task's acceptance criterion), independent of the writer that
//! produced it: a hand-rolled writer needs a hand-rolled reader to check it
//! against, or the round trip proves nothing. It accepts the RFC 8259 grammar
//! the renderer can emit; numbers are only ever integers here, so integer and
//! float are kept distinct for exact assertions.

#![allow(dead_code, clippy::expect_used, clippy::panic)]

use gramdrive_model::identity::{
    AccountId, AccountKey, AccountScope, AttachmentIndex, ChatId, ChatKey, ContentHash, MessageId,
    NamespaceVersion, SchemaFamily,
};
use gramdrive_render::ndjson::{
    Attachment, Availability, Deletion, Entity, EntityKind, MediaKind, MessageBody, MessageHistory,
    Reaction, ReactionKey, Revision, Sender, ServiceAction,
};

// --- JSON parser -----------------------------------------------------------

/// A parsed JSON value. Objects keep field order so a test can assert it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum JsonValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Arr(Vec<JsonValue>),
    Obj(Vec<(String, JsonValue)>),
}

impl JsonValue {
    pub(crate) fn get(&self, key: &str) -> Option<&JsonValue> {
        match self {
            JsonValue::Obj(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// Field value or a panic naming the missing key — tests want the panic.
    pub(crate) fn field(&self, key: &str) -> &JsonValue {
        self.get(key)
            .unwrap_or_else(|| panic!("missing field {key:?} in {self:?}"))
    }

    pub(crate) fn as_str(&self) -> Option<&str> {
        match self {
            JsonValue::Str(value) => Some(value),
            _ => None,
        }
    }

    pub(crate) fn as_i64(&self) -> Option<i64> {
        match self {
            JsonValue::Int(value) => Some(*value),
            _ => None,
        }
    }

    pub(crate) fn as_bool(&self) -> Option<bool> {
        match self {
            JsonValue::Bool(value) => Some(*value),
            _ => None,
        }
    }

    pub(crate) fn as_array(&self) -> Option<&[JsonValue]> {
        match self {
            JsonValue::Arr(items) => Some(items),
            _ => None,
        }
    }

    pub(crate) fn is_null(&self) -> bool {
        matches!(self, JsonValue::Null)
    }

    /// The object's field keys, in order.
    pub(crate) fn keys(&self) -> Vec<String> {
        match self {
            JsonValue::Obj(fields) => fields.iter().map(|(k, _)| k.clone()).collect(),
            _ => Vec::new(),
        }
    }
}

/// Parses one complete JSON value, rejecting trailing bytes.
pub(crate) fn parse(input: &str) -> Result<JsonValue, String> {
    let mut parser = Parser {
        chars: input.chars().collect(),
        pos: 0,
    };
    parser.skip_ws();
    let value = parser.value()?;
    parser.skip_ws();
    if parser.pos != parser.chars.len() {
        return Err(format!("trailing characters at index {}", parser.pos));
    }
    Ok(value)
}

/// Asserts every line of an NDJSON document is well-formed JSON, returning the
/// parsed values in order.
pub(crate) fn parse_lines(document: &str) -> Vec<JsonValue> {
    assert!(
        document.ends_with('\n'),
        "NDJSON document must end with a newline"
    );
    document
        .lines()
        .enumerate()
        .map(|(index, line)| {
            parse(line)
                .unwrap_or_else(|error| panic!("line {index} is not valid JSON: {error}\n{line}"))
        })
        .collect()
}

struct Parser {
    chars: Vec<char>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let character = self.peek();
        if character.is_some() {
            self.pos += 1;
        }
        character
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t' | '\n' | '\r')) {
            self.pos += 1;
        }
    }

    fn value(&mut self) -> Result<JsonValue, String> {
        match self.peek() {
            Some('{') => self.object(),
            Some('[') => self.array(),
            Some('"') => Ok(JsonValue::Str(self.string()?)),
            Some('t') | Some('f') => self.boolean(),
            Some('n') => self.null(),
            Some(c) if c == '-' || c.is_ascii_digit() => self.number(),
            other => Err(format!("unexpected {other:?} at index {}", self.pos)),
        }
    }

    fn object(&mut self) -> Result<JsonValue, String> {
        self.expect('{')?;
        let mut fields = Vec::new();
        self.skip_ws();
        if self.peek() == Some('}') {
            self.pos += 1;
            return Ok(JsonValue::Obj(fields));
        }
        loop {
            self.skip_ws();
            let key = self.string()?;
            self.skip_ws();
            self.expect(':')?;
            self.skip_ws();
            let value = self.value()?;
            fields.push((key, value));
            self.skip_ws();
            match self.bump() {
                Some(',') => continue,
                Some('}') => break,
                other => return Err(format!("expected ',' or '}}', got {other:?}")),
            }
        }
        Ok(JsonValue::Obj(fields))
    }

    fn array(&mut self) -> Result<JsonValue, String> {
        self.expect('[')?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(']') {
            self.pos += 1;
            return Ok(JsonValue::Arr(items));
        }
        loop {
            self.skip_ws();
            items.push(self.value()?);
            self.skip_ws();
            match self.bump() {
                Some(',') => continue,
                Some(']') => break,
                other => return Err(format!("expected ',' or ']', got {other:?}")),
            }
        }
        Ok(JsonValue::Arr(items))
    }

    fn string(&mut self) -> Result<String, String> {
        self.expect('"')?;
        let mut out = String::new();
        loop {
            match self.bump() {
                None => return Err("unterminated string".to_owned()),
                Some('"') => break,
                Some('\\') => match self.bump() {
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
                                .bump()
                                .and_then(|c| c.to_digit(16))
                                .ok_or("bad \\u escape")?;
                            code = code * 16 + digit;
                        }
                        out.push(char::from_u32(code).ok_or("bad code point")?);
                    }
                    other => return Err(format!("bad escape \\{other:?}")),
                },
                Some(other) => out.push(other),
            }
        }
        Ok(out)
    }

    fn boolean(&mut self) -> Result<JsonValue, String> {
        if self.take("true") {
            Ok(JsonValue::Bool(true))
        } else if self.take("false") {
            Ok(JsonValue::Bool(false))
        } else {
            Err(format!("bad literal at index {}", self.pos))
        }
    }

    fn null(&mut self) -> Result<JsonValue, String> {
        if self.take("null") {
            Ok(JsonValue::Null)
        } else {
            Err(format!("bad literal at index {}", self.pos))
        }
    }

    fn number(&mut self) -> Result<JsonValue, String> {
        let start = self.pos;
        if self.peek() == Some('-') {
            self.pos += 1;
        }
        let mut is_float = false;
        while let Some(c) = self.peek() {
            match c {
                '0'..='9' => self.pos += 1,
                '.' | 'e' | 'E' | '+' | '-' => {
                    is_float = true;
                    self.pos += 1;
                }
                _ => break,
            }
        }
        let text: String = self.chars[start..self.pos].iter().collect();
        if is_float {
            text.parse::<f64>()
                .map(JsonValue::Float)
                .map_err(|error| format!("bad number {text:?}: {error}"))
        } else {
            text.parse::<i64>()
                .map(JsonValue::Int)
                .map_err(|error| format!("bad integer {text:?}: {error}"))
        }
    }

    fn expect(&mut self, expected: char) -> Result<(), String> {
        match self.bump() {
            Some(c) if c == expected => Ok(()),
            other => Err(format!("expected {expected:?}, got {other:?}")),
        }
    }

    fn take(&mut self, literal: &str) -> bool {
        let literal_chars: Vec<char> = literal.chars().collect();
        let end = self.pos + literal_chars.len();
        if end <= self.chars.len() && self.chars[self.pos..end] == literal_chars[..] {
            self.pos = end;
            true
        } else {
            false
        }
    }
}

// --- Fixture corpus --------------------------------------------------------

/// The fixture chat every test renders from. Negative chat id (a channel/
/// supergroup) and a non-zero namespace epoch exercise the id encoding.
pub(crate) fn fixture_chat() -> ChatKey {
    ChatKey {
        scope: AccountScope {
            account: AccountKey {
                account_id: AccountId(7),
            },
            namespace_version: NamespaceVersion(2),
        },
        chat_id: ChatId(-1_001_234_567_890),
    }
}

fn empty_body() -> MessageBody {
    MessageBody {
        text: None,
        entities: Vec::new(),
        reply_to: None,
        thread_top: None,
        topic_id: None,
        album_id: None,
        reactions: Vec::new(),
        attachments: Vec::new(),
        service: None,
        protected: false,
    }
}

fn revision(event_seq: i64, observed_at_ms: i64, body: MessageBody) -> Revision {
    Revision {
        event_seq,
        edited_at_ms: None,
        observed_at_ms,
        payload_schema: SchemaFamily(1),
        body,
    }
}

fn digest(byte: u8) -> ContentHash {
    ContentHash::Sha256([byte; 32])
}

/// A comprehensive corpus covering every SYNC-034 fixture category: Unicode and
/// entities, replies/threads/topics, albums, edits, reactions, service
/// messages, missing senders, and deleted/restricted/view-once/unavailable
/// media — plus the forward-compat `Other` kinds. Ordered by
/// `(sent_at_ms, message_id)`, as a well-formed export supplies them.
pub(crate) fn corpus() -> Vec<MessageHistory> {
    vec![
        // 1. Rich text: Unicode, several entity kinds, reactions, sender.
        MessageHistory {
            message_id: MessageId(100),
            sender: Some(Sender { id: 555 }),
            sent_at_ms: 1_700_000_000_000,
            revisions: vec![revision(1, 1_700_000_000_500, {
                let mut body = empty_body();
                body.text = Some("Привет, мир! **bold** and a link 👨‍👩‍👧".to_owned());
                body.entities = vec![
                    Entity {
                        kind: EntityKind::Bold,
                        offset: 14,
                        length: 4,
                    },
                    Entity {
                        kind: EntityKind::TextLink {
                            url: "https://example.com/a?b=c&d".to_owned(),
                        },
                        offset: 25,
                        length: 4,
                    },
                    Entity {
                        kind: EntityKind::Pre {
                            language: Some("rust".to_owned()),
                        },
                        offset: 0,
                        length: 6,
                    },
                    Entity {
                        kind: EntityKind::CustomEmoji { document_id: 987 },
                        offset: 30,
                        length: 2,
                    },
                    Entity {
                        kind: EntityKind::Other {
                            kind: "future_entity".to_owned(),
                        },
                        offset: 1,
                        length: 1,
                    },
                ];
                body.reactions = vec![
                    Reaction {
                        key: ReactionKey::Emoji("👍".to_owned()),
                        count: 3,
                        chosen: true,
                    },
                    Reaction {
                        key: ReactionKey::Custom(555_000),
                        count: 1,
                        chosen: false,
                    },
                ];
                body
            })],
            deletion: None,
        },
        // 2. Edited twice: three revisions in one history (out of seq order to
        //    prove the renderer sorts by event_seq).
        MessageHistory {
            message_id: MessageId(101),
            sender: Some(Sender { id: 555 }),
            sent_at_ms: 1_700_000_100_000,
            revisions: vec![
                Revision {
                    event_seq: 9,
                    edited_at_ms: Some(1_700_000_300_000),
                    observed_at_ms: 1_700_000_300_100,
                    payload_schema: SchemaFamily(1),
                    body: {
                        let mut body = empty_body();
                        body.text = Some("third".to_owned());
                        body
                    },
                },
                revision(2, 1_700_000_100_100, {
                    let mut body = empty_body();
                    body.text = Some("first".to_owned());
                    body
                }),
                Revision {
                    event_seq: 5,
                    edited_at_ms: Some(1_700_000_200_000),
                    observed_at_ms: 1_700_000_200_100,
                    payload_schema: SchemaFamily(1),
                    body: {
                        let mut body = empty_body();
                        body.text = Some("second".to_owned());
                        body
                    },
                },
            ],
            deletion: None,
        },
        // 3. Deleted after an edit: Mirror omits, Audit keeps a tombstone.
        MessageHistory {
            message_id: MessageId(102),
            sender: Some(Sender { id: 777 }),
            sent_at_ms: 1_700_000_150_000,
            revisions: vec![
                revision(3, 1_700_000_150_100, {
                    let mut body = empty_body();
                    body.text = Some("original".to_owned());
                    body
                }),
                Revision {
                    event_seq: 6,
                    edited_at_ms: Some(1_700_000_250_000),
                    observed_at_ms: 1_700_000_250_100,
                    payload_schema: SchemaFamily(1),
                    body: {
                        let mut body = empty_body();
                        body.text = Some("edited then deleted".to_owned());
                        body
                    },
                },
            ],
            deletion: Some(Deletion {
                observed_at_ms: 1_700_000_400_000,
            }),
        },
        // 4. Reply within a thread/topic.
        MessageHistory {
            message_id: MessageId(103),
            sender: Some(Sender { id: 888 }),
            sent_at_ms: 1_700_000_160_000,
            revisions: vec![revision(4, 1_700_000_160_100, {
                let mut body = empty_body();
                body.text = Some("a reply".to_owned());
                body.reply_to = Some(MessageId(100));
                body.thread_top = Some(MessageId(100));
                body.topic_id = Some(42);
                body
            })],
            deletion: None,
        },
        // 5a. Album member with a downloaded photo (content present).
        MessageHistory {
            message_id: MessageId(104),
            sender: Some(Sender { id: 555 }),
            sent_at_ms: 1_700_000_170_000,
            revisions: vec![revision(7, 1_700_000_170_100, {
                let mut body = empty_body();
                body.album_id = Some(9_000);
                body.attachments = vec![Attachment {
                    index: AttachmentIndex(0),
                    media_kind: MediaKind::Photo,
                    name: Some("IMG_0001.jpg".to_owned()),
                    mime_type: Some("image/jpeg".to_owned()),
                    size: Some(204_800),
                    availability: Availability::Fetchable,
                    content_hash: Some(digest(0xab)),
                }];
                body
            })],
            deletion: None,
        },
        // 5b. Second album member, not yet downloaded (dataless placeholder).
        MessageHistory {
            message_id: MessageId(105),
            sender: Some(Sender { id: 555 }),
            sent_at_ms: 1_700_000_170_000,
            revisions: vec![revision(8, 1_700_000_170_200, {
                let mut body = empty_body();
                body.album_id = Some(9_000);
                body.attachments = vec![Attachment {
                    index: AttachmentIndex(0),
                    media_kind: MediaKind::Video,
                    name: None,
                    mime_type: Some("video/mp4".to_owned()),
                    size: Some(5_242_880),
                    availability: Availability::Fetchable,
                    content_hash: None,
                }];
                body
            })],
            deletion: None,
        },
        // 6. Protected content: restricted attachment, protected flag set.
        MessageHistory {
            message_id: MessageId(106),
            sender: Some(Sender { id: 999 }),
            sent_at_ms: 1_700_000_180_000,
            revisions: vec![revision(10, 1_700_000_180_100, {
                let mut body = empty_body();
                body.text = Some("no-save channel".to_owned());
                body.protected = true;
                body.attachments = vec![Attachment {
                    index: AttachmentIndex(0),
                    media_kind: MediaKind::Document,
                    name: Some("secret.pdf".to_owned()),
                    mime_type: Some("application/pdf".to_owned()),
                    size: Some(1_024),
                    availability: Availability::Restricted,
                    content_hash: None,
                }];
                body
            })],
            deletion: None,
        },
        // 7. View-once and unavailable media, plus an unknown media kind.
        MessageHistory {
            message_id: MessageId(107),
            sender: Some(Sender { id: 999 }),
            sent_at_ms: 1_700_000_190_000,
            revisions: vec![revision(11, 1_700_000_190_100, {
                let mut body = empty_body();
                body.attachments = vec![
                    Attachment {
                        index: AttachmentIndex(0),
                        media_kind: MediaKind::Photo,
                        name: None,
                        mime_type: None,
                        size: None,
                        availability: Availability::ViewOnce,
                        content_hash: None,
                    },
                    Attachment {
                        index: AttachmentIndex(1),
                        media_kind: MediaKind::Voice,
                        name: None,
                        mime_type: Some("audio/ogg".to_owned()),
                        size: None,
                        availability: Availability::Unavailable,
                        content_hash: None,
                    },
                    Attachment {
                        index: AttachmentIndex(2),
                        media_kind: MediaKind::Other {
                            kind: "giveaway".to_owned(),
                        },
                        name: None,
                        mime_type: None,
                        size: None,
                        availability: Availability::Unavailable,
                        content_hash: None,
                    },
                ];
                body
            })],
            deletion: None,
        },
        // 8. Service message: members added (a service action with a list).
        MessageHistory {
            message_id: MessageId(108),
            sender: Some(Sender { id: 555 }),
            sent_at_ms: 1_700_000_200_000,
            revisions: vec![revision(12, 1_700_000_200_100, {
                let mut body = empty_body();
                body.service = Some(ServiceAction::MembersAdded {
                    user_ids: vec![777, 888, 999],
                });
                body
            })],
            deletion: None,
        },
        // 9. Channel post: missing sender, plus an unknown service action.
        MessageHistory {
            message_id: MessageId(109),
            sender: None,
            sent_at_ms: 1_700_000_210_000,
            revisions: vec![revision(13, 1_700_000_210_100, {
                let mut body = empty_body();
                body.text = Some("posted as the channel".to_owned());
                body.service = Some(ServiceAction::Other {
                    kind: "boost_applied".to_owned(),
                });
                body
            })],
            deletion: None,
        },
    ]
}
