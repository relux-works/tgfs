//! Metadata and content versions (DOM-003; TASK-260715-1j4ij3).
//!
//! Every mutable item carries versions "sufficient to detect stale
//! metadata/content" (DOM-003). This module owns the two version tokens of
//! that sentence:
//!
//! - [`MetadataVersion`] changes whenever provider-visible metadata or
//!   parent membership changes (`.spec/domain-model.md` § Versioning). It is
//!   also the snapshot anchor of paged enumeration (SYNC-003): every page of
//!   one listing reports the same metadata version.
//! - [`ContentVersion`] changes whenever the bytes returned for the same
//!   item may change. A fetch is pinned to it: content fetched for version A
//!   must never be published as version B (DOM versioning; SYNC-042).
//!
//! They are distinct types on purpose. Both wrap an opaque token, but a
//! metadata version where a content version belongs is exactly the mistake
//! that publishes stale bytes under a fresh-looking stamp — so the type
//! system refuses the substitution instead of a review catching it.
//!
//! # Opacity and comparison
//!
//! A token is provider-chosen text: a TDLib source might use message edit
//! stamps, a remote source a server ETag, the renderer a
//! `renderer_version + input watermark` composite (DOM-006). The core
//! compares tokens for *equality only* — same token, same state. Neither
//! type implements `Ord`: DOM-003 allows monotonic *or* content-derived
//! versions, so cross-token "newer than" is not meaningful in general, and
//! offering it would invite ordering logic that is wrong for exactly the
//! content-derived half.
//!
//! # Durability
//!
//! Versions are durable: the state store records them per item and the
//! transfer engine persists the pinned content version of every download
//! (`.spec/domain-model.md` § Transfer). The durable form is the token text
//! itself ([`as_str`] / [`new`]) — there is no structure to version because
//! the token deliberately has none; schema evolution belongs to the
//! containers that persist it. Validation bounds what a well-formed token
//! can be so a corrupt row fails loudly at parse time instead of silently
//! comparing unequal forever.
//!
//! [`as_str`]: MetadataVersion::as_str
//! [`new`]: MetadataVersion::new

/// Upper bound on a version token's UTF-8 length, in bytes.
///
/// Generous for every real scheme (ETags, integer stamps, hash hex) while
/// keeping a malfunctioning provider from growing unbounded database rows.
pub const MAX_VERSION_TOKEN_BYTES: usize = 256;

/// Why a string cannot be a version token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidVersionToken {
    /// The token is empty. An empty version cannot witness a state.
    Empty,
    /// The token exceeds [`MAX_VERSION_TOKEN_BYTES`].
    TooLong {
        /// The rejected length in bytes.
        len: usize,
    },
    /// The token contains an ASCII control character (including DEL).
    /// Versions appear in logs and diagnostics; control bytes there are
    /// corruption or injection, never a legitimate provider scheme.
    ForbiddenCharacter {
        /// Byte offset of the offending character.
        position: usize,
    },
}

impl std::fmt::Display for InvalidVersionToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str("version token is empty"),
            Self::TooLong { len } => write!(
                f,
                "version token is {len} bytes; limit is {MAX_VERSION_TOKEN_BYTES}"
            ),
            Self::ForbiddenCharacter { position } => {
                write!(
                    f,
                    "version token has a control character at byte {position}"
                )
            }
        }
    }
}

impl std::error::Error for InvalidVersionToken {}

fn validate_token(token: &str) -> Result<(), InvalidVersionToken> {
    if token.is_empty() {
        return Err(InvalidVersionToken::Empty);
    }
    if token.len() > MAX_VERSION_TOKEN_BYTES {
        return Err(InvalidVersionToken::TooLong { len: token.len() });
    }
    if let Some(position) = token.bytes().position(|b| b.is_ascii_control()) {
        return Err(InvalidVersionToken::ForbiddenCharacter { position });
    }
    Ok(())
}

/// Version of an item's provider-visible metadata (DOM-003).
///
/// Changes when metadata or parent membership changes; equal tokens mean an
/// unchanged item and a still-valid enumeration snapshot (SYNC-003).
/// Equality-only semantics — see the module docs for why `Ord` is absent.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MetadataVersion {
    token: String,
}

impl MetadataVersion {
    /// Wraps a provider-chosen token, validating it is well-formed.
    pub fn new(token: impl Into<String>) -> Result<Self, InvalidVersionToken> {
        let token = token.into();
        validate_token(&token)?;
        Ok(Self { token })
    }

    /// The token text — also the durable serialized form.
    pub fn as_str(&self) -> &str {
        &self.token
    }
}

impl std::fmt::Display for MetadataVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.token)
    }
}

/// Version of an item's content bytes (DOM-003).
///
/// Changes when the bytes returned for the item may change. Fetches pin to
/// it; a completed fetch is valid only for the pinned token (SYNC-042).
/// Equality-only semantics — see the module docs for why `Ord` is absent.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ContentVersion {
    token: String,
}

impl ContentVersion {
    /// Wraps a provider-chosen token, validating it is well-formed.
    pub fn new(token: impl Into<String>) -> Result<Self, InvalidVersionToken> {
        let token = token.into();
        validate_token(&token)?;
        Ok(Self { token })
    }

    /// The token text — also the durable serialized form.
    pub fn as_str(&self) -> &str {
        &self.token
    }
}

impl std::fmt::Display for ContentVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_token_text() {
        let version = MetadataVersion::new("etag:abc123").unwrap();
        assert_eq!(version.as_str(), "etag:abc123");
        assert_eq!(version.to_string(), "etag:abc123");
        let reparsed = MetadataVersion::new(version.as_str().to_owned()).unwrap();
        assert_eq!(reparsed, version);
    }

    #[test]
    fn rejects_empty_token() {
        assert_eq!(
            MetadataVersion::new("").unwrap_err(),
            InvalidVersionToken::Empty
        );
        assert_eq!(
            ContentVersion::new("").unwrap_err(),
            InvalidVersionToken::Empty
        );
    }

    #[test]
    fn rejects_oversized_token() {
        let long = "v".repeat(MAX_VERSION_TOKEN_BYTES + 1);
        assert_eq!(
            ContentVersion::new(long).unwrap_err(),
            InvalidVersionToken::TooLong {
                len: MAX_VERSION_TOKEN_BYTES + 1
            }
        );
        // The boundary itself is accepted.
        assert!(ContentVersion::new("v".repeat(MAX_VERSION_TOKEN_BYTES)).is_ok());
    }

    #[test]
    fn rejects_control_characters() {
        assert_eq!(
            MetadataVersion::new("ab\ncd").unwrap_err(),
            InvalidVersionToken::ForbiddenCharacter { position: 2 }
        );
        assert_eq!(
            MetadataVersion::new("\u{7f}").unwrap_err(),
            InvalidVersionToken::ForbiddenCharacter { position: 0 }
        );
    }

    #[test]
    fn accepts_unicode_tokens() {
        // Providers choose their scheme; non-ASCII text is not corruption.
        let version = ContentVersion::new("génération-7").unwrap();
        assert_eq!(version.as_str(), "génération-7");
    }

    #[test]
    fn equality_is_by_token() {
        assert_eq!(
            MetadataVersion::new("a").unwrap(),
            MetadataVersion::new("a").unwrap()
        );
        assert_ne!(
            MetadataVersion::new("a").unwrap(),
            MetadataVersion::new("b").unwrap()
        );
    }

    #[test]
    fn error_messages_name_the_violation() {
        assert_eq!(
            InvalidVersionToken::Empty.to_string(),
            "version token is empty"
        );
        assert_eq!(
            InvalidVersionToken::TooLong { len: 300 }.to_string(),
            "version token is 300 bytes; limit is 256"
        );
        assert_eq!(
            InvalidVersionToken::ForbiddenCharacter { position: 4 }.to_string(),
            "version token has a control character at byte 4"
        );
    }
}
