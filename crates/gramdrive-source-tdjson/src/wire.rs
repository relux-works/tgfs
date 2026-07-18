//! Leaf parsers for TDLib's C JSON wire shapes, shared by the snapshot
//! ([`crate::snapshot`]), the live-update mapper ([`crate::updates`]), and
//! the message normalizer ([`crate::message`]).
//!
//! These are the small, subtle decoders the machines must agree on
//! byte-for-byte — most of all int64 fields (a position `order`, a
//! `media_album_id`, a custom emoji id), which tdjson serializes as decimal
//! *strings* and which a float round trip would silently corrupt. One copy,
//! one set of tests, so the machines cannot drift.

use serde_json::Value;

use gramdrive_model::identity::{ChatListKind, FolderId};

/// The TDLib `ChatList` object of a list kind.
pub(crate) fn list_json(list: ChatListKind) -> Value {
    match list {
        ChatListKind::Main => serde_json::json!({"@type": "chatListMain"}),
        ChatListKind::Archive => serde_json::json!({"@type": "chatListArchive"}),
        ChatListKind::Folder(folder) => {
            serde_json::json!({"@type": "chatListFolder", "chat_folder_id": folder.0})
        }
    }
}

/// Parse a TDLib `ChatList` object; `None` for shapes this build does not
/// know (a future list kind must not break the sync of the known ones).
pub(crate) fn parse_list(value: &Value) -> Option<ChatListKind> {
    match value.get("@type").and_then(Value::as_str)? {
        "chatListMain" => Some(ChatListKind::Main),
        "chatListArchive" => Some(ChatListKind::Archive),
        "chatListFolder" => {
            let id = value.get("chat_folder_id").and_then(Value::as_i64)?;
            Some(ChatListKind::Folder(FolderId(i32::try_from(id).ok()?)))
        }
        _ => None,
    }
}

/// Chat flavor as parsed from a TDLib chat object, including the flavors that
/// never reach a commit. The peer id (`user_id`/`supergroup_id`) is kept so
/// the live mapper can attribute an `updateUser`/`updateSupergroup` username
/// change back to the chat it renames.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum KindFact {
    Private { user_id: i64 },
    Group,
    Supergroup { supergroup_id: i64 },
    Channel { supergroup_id: i64 },
    Secret,
    Unsupported,
}

/// Parse a TDLib chat type object into the fact vocabulary. Unknown shapes
/// become `Unsupported` — excluded and counted, never a guess.
pub(crate) fn parse_chat_kind(value: &Value) -> KindFact {
    match value.get("@type").and_then(Value::as_str) {
        Some("chatTypePrivate") => match value.get("user_id").and_then(Value::as_i64) {
            Some(user_id) => KindFact::Private { user_id },
            None => KindFact::Unsupported,
        },
        Some("chatTypeBasicGroup") => KindFact::Group,
        Some("chatTypeSupergroup") => {
            let supergroup_id = value.get("supergroup_id").and_then(Value::as_i64);
            let is_channel = value
                .get("is_channel")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            match (supergroup_id, is_channel) {
                (Some(supergroup_id), false) => KindFact::Supergroup { supergroup_id },
                (Some(supergroup_id), true) => KindFact::Channel { supergroup_id },
                (None, _) => KindFact::Unsupported,
            }
        }
        Some("chatTypeSecret") => KindFact::Secret,
        _ => KindFact::Unsupported,
    }
}

/// Parse a TDLib int64 field (a position order, an album id, a custom emoji
/// id): the C JSON interface serializes int64 as a decimal string; a plain
/// number is tolerated for robustness.
pub(crate) fn parse_int64(value: &Value) -> Option<i64> {
    match value {
        Value::String(text) => text.parse().ok(),
        Value::Number(_) => value.as_i64(),
        _ => None,
    }
}

/// The first active public username of a TDLib user/supergroup object, when it
/// carries one. Telegram reports usernames through the `usernames` object; the
/// editable username is the canonical one, with the active list as the
/// fallback shape.
pub(crate) fn active_username(object: &Value) -> Option<String> {
    let usernames = object.get("usernames")?;
    let editable = usernames
        .get("editable_username")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty());
    let name = match editable {
        Some(name) => name,
        None => usernames
            .get("active_usernames")
            .and_then(Value::as_array)?
            .iter()
            .filter_map(Value::as_str)
            .find(|name| !name.is_empty())?,
    };
    Some(name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn int64_parses_the_string_shape_tdjson_sends() {
        assert_eq!(
            parse_int64(&Value::String("2685396931233784969".to_owned())),
            Some(2685396931233784969)
        );
        assert_eq!(parse_int64(&json!(42)), Some(42));
        assert_eq!(parse_int64(&Value::String("x".to_owned())), None);
        assert_eq!(parse_int64(&json!(null)), None);
    }

    #[test]
    fn list_json_round_trips_through_parse_list() {
        for list in [
            ChatListKind::Main,
            ChatListKind::Archive,
            ChatListKind::Folder(FolderId(7)),
        ] {
            assert_eq!(parse_list(&list_json(list)), Some(list));
        }
        assert_eq!(parse_list(&json!({"@type": "chatListFuture"})), None);
    }

    #[test]
    fn usernames_prefer_editable_and_fall_back_to_active() {
        let editable = json!({"usernames": {
            "editable_username": "primary",
            "active_usernames": ["secondary"],
        }});
        assert_eq!(active_username(&editable).as_deref(), Some("primary"));
        let active_only = json!({"usernames": {
            "editable_username": "",
            "active_usernames": ["", "fallback"],
        }});
        assert_eq!(active_username(&active_only).as_deref(), Some("fallback"));
        assert_eq!(active_username(&json!({})), None);
        assert_eq!(active_username(&json!({"usernames": {}})), None);
    }
}
