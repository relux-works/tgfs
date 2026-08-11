//! Deterministic, privacy-bounded `.chat.json` metadata rendering.
//!
//! The document intentionally contains only provider-useful chat metadata.
//! Account identity, namespace identity, Telegram chat ids, authorization
//! state, secret references, local paths, and message content are not inputs,
//! so they cannot accidentally enter the rendered bytes.

use gramdrive_model::hash::sha256;
use gramdrive_model::identity::{ContentHash, SchemaFamily};

use crate::json::Json;

/// Stable schema lineage used in generated-document identity.
pub const CHAT_SCHEMA_FAMILY: SchemaFamily = SchemaFamily(1);
/// Human/machine readable schema identifier carried in every document.
pub const SCHEMA_ID: &str = "gramdrive.chat";
/// Record schema revision.
pub const SCHEMA_VERSION: u32 = 1;
/// Renderer implementation revision.
pub const RENDERER_VERSION: u32 = 1;

/// Privacy-safe chat kind exposed by `.chat.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatKind {
    /// A one-on-one chat.
    Private,
    /// A basic group.
    Group,
    /// A supergroup.
    Supergroup,
    /// A broadcast channel.
    Channel,
}

impl ChatKind {
    fn tag(self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::Group => "group",
            Self::Supergroup => "supergroup",
            Self::Channel => "channel",
        }
    }
}

/// Complete byte-shaping input of one `.chat.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMetadataInput<'a> {
    /// Telegram chat flavor.
    pub kind: ChatKind,
    /// Current provider-visible title.
    pub title: &'a str,
    /// Public username, when the chat has one.
    pub username: Option<&'a str>,
    /// Telegram protected-content state.
    pub is_protected: bool,
    /// Per-chat Archive Mode state.
    pub archive_mode: bool,
    /// When the account left the chat, if observed.
    pub left_at_ms: Option<i64>,
    /// When deletion was observed, if any.
    pub deleted_at_ms: Option<i64>,
    /// Last known metadata update instant.
    pub last_update_at_ms: Option<i64>,
}

/// Renders compact RFC 8259 JSON with a trailing newline.
///
/// Field order is fixed here rather than delegated to a map-backed serializer,
/// so equal inputs are byte-identical across relaunches.
pub fn render(input: &ChatMetadataInput<'_>) -> String {
    let value = Json::Object(vec![
        ("schema", Json::str(SCHEMA_ID)),
        ("schema_version", Json::U64(u64::from(SCHEMA_VERSION))),
        ("renderer_version", Json::U64(u64::from(RENDERER_VERSION))),
        (
            "chat",
            Json::Object(vec![
                ("type", Json::str(input.kind.tag())),
                ("title", Json::str(input.title)),
                ("username", input.username.map_or(Json::Null, Json::str)),
                ("is_protected", Json::Bool(input.is_protected)),
                ("archive_mode", Json::Bool(input.archive_mode)),
                ("left_at_ms", input.left_at_ms.map_or(Json::Null, Json::I64)),
                (
                    "deleted_at_ms",
                    input.deleted_at_ms.map_or(Json::Null, Json::I64),
                ),
                (
                    "last_update_at_ms",
                    input.last_update_at_ms.map_or(Json::Null, Json::I64),
                ),
            ]),
        ),
    ]);
    let mut output = String::new();
    value.write(&mut output);
    output.push('\n');
    output
}

/// Stable content-version token for the exact rendered bytes.
///
/// Hashing the bytes keeps arbitrary titles/usernames out of the version token
/// while still making every byte-shaping metadata change produce a new pin.
pub fn content_version_token(bytes: &[u8]) -> String {
    let ContentHash::Sha256(digest) = sha256(bytes);
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{SCHEMA_ID}/s{SCHEMA_VERSION}/r{RENDERER_VERSION}/sha256-{hex}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input<'a>() -> ChatMetadataInput<'a> {
        ChatMetadataInput {
            kind: ChatKind::Supergroup,
            title: "Release \"room\"",
            username: Some("public_room"),
            is_protected: false,
            archive_mode: true,
            left_at_ms: None,
            deleted_at_ms: None,
            last_update_at_ms: Some(1_784_116_800_000),
        }
    }

    #[test]
    fn renders_stable_privacy_bounded_metadata() {
        let bytes = render(&input());
        assert_eq!(
            bytes,
            concat!(
                "{\"schema\":\"gramdrive.chat\",\"schema_version\":1,",
                "\"renderer_version\":1,\"chat\":{\"type\":\"supergroup\",",
                "\"title\":\"Release \\\"room\\\"\",\"username\":\"public_room\",",
                "\"is_protected\":false,\"archive_mode\":true,\"left_at_ms\":null,",
                "\"deleted_at_ms\":null,\"last_update_at_ms\":1784116800000}}\n"
            )
        );
        for forbidden in [
            "account_id",
            "chat_id",
            "namespace",
            "authorization",
            "secret",
            "path",
            "message",
        ] {
            assert!(!bytes.contains(forbidden));
        }
    }

    #[test]
    fn equal_bytes_have_equal_versions_and_metadata_changes_move_them() {
        let first = render(&input());
        let replay = render(&input());
        assert_eq!(first, replay);
        assert_eq!(
            content_version_token(first.as_bytes()),
            content_version_token(replay.as_bytes())
        );

        let mut changed = input();
        changed.archive_mode = false;
        let changed = render(&changed);
        assert_ne!(first, changed);
        assert_ne!(
            content_version_token(first.as_bytes()),
            content_version_token(changed.as_bytes())
        );
    }
}
