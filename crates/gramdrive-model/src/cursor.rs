//! Durable change cursors (DOM-004, SYNC-004; TASK-260715-1j4ij3).
//!
//! A [`ChangeCursor`] anchors one position in a source's change feed: the
//! state store persists it transactionally with the normalized state it
//! witnessed (SYNC-022), and after a restart the drive resumes from exactly
//! that position (SYNC-004). Per DOM-004 a cursor is opaque and scoped —
//! this module makes both properties structural:
//!
//! - **Scoped.** Every cursor carries the [`AccountScope`] (account plus
//!   namespace epoch) it was minted under. A cursor from another account, or
//!   from before a namespace bump retired the account's identities, is
//!   detected by [`ChangeCursor::require_scope`] and rejected explicitly —
//!   never silently applied to the wrong namespace.
//! - **Opaque payload.** The feed position itself is provider state
//!   (Telegram `pts`-style counters, a remote service's token) serialized by
//!   the source into bytes the core never interprets (DEC-003). The core
//!   guarantees only that the payload survives the round trip unchanged.
//!
//! # Serialization format v1
//!
//! Durable means versioned: the encoding carries a format version byte and
//! any future change is a new version decoded alongside v1, never a mutation
//! of v1 — the same policy the identity codec froze (crate README). Binary
//! layout (integers big-endian, two's complement):
//!
//! ```text
//! byte  0        format version (0x01)
//! bytes 1..9     account id (i64)
//! bytes 9..13    namespace version (u32)
//! bytes 13..     provider payload (0..=4096 bytes, the remainder)
//! ```
//!
//! Exactly one variable-length field, at the tail, so the encoding is
//! injective. Text form: `"gdc-"` + unpadded lowercase base32 of the binary
//! form (`crate::base32`, shared with `ItemId`). The prefixes cannot alias:
//! cursor text fails `ItemId` parsing at the `-` (outside the base32
//! alphabet) and identity text fails cursor parsing at the missing prefix.

use crate::base32::{self, TextDecodeError};
use crate::identity::{AccountId, AccountKey, AccountScope, NamespaceVersion};

const FORMAT_VERSION: u8 = 0x01;
const TEXT_PREFIX: &str = "gdc-";
/// Bytes 0..13 of the v1 layout: version, account id, namespace version.
const HEADER_LEN: usize = 13;

/// Upper bound on the provider payload, in bytes.
///
/// Change-feed positions are counters and short tokens; kilobytes of state
/// belong in the state store, not inside a cursor row. The cap keeps a
/// malfunctioning provider from turning every checkpoint into an unbounded
/// write.
pub const MAX_CURSOR_PAYLOAD_BYTES: usize = 4096;

/// One durable position in a source's change feed (DOM-004).
///
/// Construct with [`ChangeCursor::new`] from the source that owns the feed;
/// persist via [`ChangeCursor::encode`]; restore via [`ChangeCursor::decode`]
/// and gate every use behind [`ChangeCursor::require_scope`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChangeCursor {
    scope: AccountScope,
    payload: Vec<u8>,
}

impl ChangeCursor {
    /// Creates a cursor for the given scope around an opaque provider
    /// payload. An empty payload is valid — a provider whose feed starts at
    /// "nothing observed yet" has no position bytes to record.
    pub fn new(
        scope: AccountScope,
        payload: impl Into<Vec<u8>>,
    ) -> Result<Self, InvalidCursorPayload> {
        let payload = payload.into();
        if payload.len() > MAX_CURSOR_PAYLOAD_BYTES {
            return Err(InvalidCursorPayload { len: payload.len() });
        }
        Ok(Self { scope, payload })
    }

    /// The account and namespace epoch this cursor was minted under.
    pub fn scope(&self) -> AccountScope {
        self.scope
    }

    /// The provider's opaque feed position. The core never interprets it.
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    /// Verifies this cursor belongs to `expected`, rejecting account and
    /// namespace mismatches explicitly (SYNC-004).
    ///
    /// Every consumer must call this before acting on a restored cursor: a
    /// mismatch means the cursor describes a retired or foreign identity
    /// namespace, and the only correct reaction is an explicit failure that
    /// leads to re-baselining — not a silent apply.
    pub fn require_scope(&self, expected: AccountScope) -> Result<(), CursorScopeMismatch> {
        if self.scope == expected {
            Ok(())
        } else {
            Err(CursorScopeMismatch {
                expected,
                found: self.scope,
            })
        }
    }

    /// The durable text form: `"gdc-"` + base32 of the v1 binary layout.
    pub fn encode(&self) -> String {
        let mut bytes = Vec::with_capacity(HEADER_LEN + self.payload.len());
        bytes.push(FORMAT_VERSION);
        bytes.extend_from_slice(&self.scope.account.account_id.0.to_be_bytes());
        bytes.extend_from_slice(&self.scope.namespace_version.0.to_be_bytes());
        bytes.extend_from_slice(&self.payload);
        base32::encode(TEXT_PREFIX, &bytes)
    }

    /// Parses the durable text form, accepting exactly the canonical
    /// encodings this build understands.
    ///
    /// A version byte from a future format fails with
    /// [`CursorParseError::UnsupportedVersion`] — the explicit schema
    /// rejection SYNC-004 requires, distinct from corruption.
    pub fn decode(text: &str) -> Result<Self, CursorParseError> {
        let bytes = base32::decode(TEXT_PREFIX, text).map_err(|error| match error {
            TextDecodeError::MissingPrefix => CursorParseError::MissingPrefix,
            TextDecodeError::InvalidCharacter { position } => {
                CursorParseError::InvalidCharacter { position }
            }
            TextDecodeError::NonCanonical => CursorParseError::NonCanonicalText,
        })?;
        let version = *bytes.first().ok_or(CursorParseError::Truncated)?;
        if version != FORMAT_VERSION {
            return Err(CursorParseError::UnsupportedVersion { version });
        }
        if bytes.len() < HEADER_LEN {
            return Err(CursorParseError::Truncated);
        }
        // Both slices are fixed-width by the layout, so the conversions
        // cannot fail; indexing is in bounds by the length check above.
        let account_id = i64::from_be_bytes(
            bytes[1..9]
                .try_into()
                .map_err(|_| CursorParseError::Truncated)?,
        );
        let namespace_version = u32::from_be_bytes(
            bytes[9..HEADER_LEN]
                .try_into()
                .map_err(|_| CursorParseError::Truncated)?,
        );
        let payload = bytes[HEADER_LEN..].to_vec();
        if payload.len() > MAX_CURSOR_PAYLOAD_BYTES {
            return Err(CursorParseError::PayloadTooLarge { len: payload.len() });
        }
        Ok(Self {
            scope: AccountScope {
                account: AccountKey {
                    account_id: AccountId(account_id),
                },
                namespace_version: NamespaceVersion(namespace_version),
            },
            payload,
        })
    }
}

/// Why a payload cannot be wrapped into a [`ChangeCursor`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidCursorPayload {
    /// The rejected payload length in bytes.
    pub len: usize,
}

impl std::fmt::Display for InvalidCursorPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "cursor payload is {} bytes; limit is {MAX_CURSOR_PAYLOAD_BYTES}",
            self.len
        )
    }
}

impl std::error::Error for InvalidCursorPayload {}

/// Why a text string is not a valid [`ChangeCursor`].
///
/// Structured for diagnostics, not recovery: an unparseable cursor is data
/// corruption or version skew, and the caller's job is to report which and
/// re-baseline (SYNC-004).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorParseError {
    /// The format version byte is not one this build understands — most
    /// likely a cursor minted by a newer app version.
    UnsupportedVersion {
        /// The version byte found.
        version: u8,
    },
    /// The payload ended before the fixed-width header was complete.
    Truncated,
    /// The decoded payload exceeds [`MAX_CURSOR_PAYLOAD_BYTES`].
    PayloadTooLarge {
        /// The rejected payload length in bytes.
        len: usize,
    },
    /// The text form does not start with the `"gdc-"` prefix.
    MissingPrefix,
    /// The text form contains a byte outside the lowercase base32 alphabet.
    InvalidCharacter {
        /// Byte offset of the offending character within the full input.
        position: usize,
    },
    /// The text form is not the canonical base32 encoding of any byte
    /// string (invalid length residue or nonzero padding bits).
    NonCanonicalText,
}

impl std::fmt::Display for CursorParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedVersion { version } => {
                write!(f, "unsupported change cursor format version {version}")
            }
            Self::Truncated => f.write_str("change cursor payload is truncated"),
            Self::PayloadTooLarge { len } => write!(
                f,
                "change cursor payload is {len} bytes; limit is {MAX_CURSOR_PAYLOAD_BYTES}"
            ),
            Self::MissingPrefix => f.write_str("change cursor text lacks the 'gdc-' prefix"),
            Self::InvalidCharacter { position } => {
                write!(f, "invalid base32 character at byte {position}")
            }
            Self::NonCanonicalText => f.write_str("change cursor text is not canonical base32"),
        }
    }
}

impl std::error::Error for CursorParseError {}

/// A cursor presented against a scope it was not minted under (SYNC-004).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorScopeMismatch {
    /// The scope the consumer serves.
    pub expected: AccountScope,
    /// The scope the cursor carries.
    pub found: AccountScope,
}

impl std::fmt::Display for CursorScopeMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "change cursor scope mismatch: expected account {} namespace {}, found account {} namespace {}",
            self.expected.account.account_id.0,
            self.expected.namespace_version.0,
            self.found.account.account_id.0,
            self.found.namespace_version.0,
        )
    }
}

impl std::error::Error for CursorScopeMismatch {}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(account: i64, namespace: u32) -> AccountScope {
        AccountScope {
            account: AccountKey {
                account_id: AccountId(account),
            },
            namespace_version: NamespaceVersion(namespace),
        }
    }

    #[test]
    fn round_trips_scope_and_payload() {
        let cursor = ChangeCursor::new(scope(42, 7), b"pts:12345".to_vec()).unwrap();
        let decoded = ChangeCursor::decode(&cursor.encode()).unwrap();
        assert_eq!(decoded, cursor);
        assert_eq!(decoded.scope(), scope(42, 7));
        assert_eq!(decoded.payload(), b"pts:12345");
    }

    #[test]
    fn round_trips_empty_payload_and_extreme_scope() {
        for account in [i64::MIN, -1, 0, i64::MAX] {
            for namespace in [0, u32::MAX] {
                let cursor = ChangeCursor::new(scope(account, namespace), Vec::new()).unwrap();
                let decoded = ChangeCursor::decode(&cursor.encode()).unwrap();
                assert_eq!(decoded, cursor);
                assert!(decoded.payload().is_empty());
            }
        }
    }

    #[test]
    fn rejects_oversized_payload_at_construction() {
        let err =
            ChangeCursor::new(scope(1, 0), vec![0u8; MAX_CURSOR_PAYLOAD_BYTES + 1]).unwrap_err();
        assert_eq!(
            err,
            InvalidCursorPayload {
                len: MAX_CURSOR_PAYLOAD_BYTES + 1
            }
        );
        // The boundary itself is accepted.
        assert!(ChangeCursor::new(scope(1, 0), vec![0u8; MAX_CURSOR_PAYLOAD_BYTES]).is_ok());
    }

    #[test]
    fn scope_check_accepts_matching_and_rejects_foreign() {
        let cursor = ChangeCursor::new(scope(5, 2), b"x".to_vec()).unwrap();
        assert!(cursor.require_scope(scope(5, 2)).is_ok());

        let err = cursor.require_scope(scope(5, 3)).unwrap_err();
        assert_eq!(err.expected, scope(5, 3));
        assert_eq!(err.found, scope(5, 2));
        assert_eq!(
            err.to_string(),
            "change cursor scope mismatch: expected account 5 namespace 3, \
             found account 5 namespace 2"
        );
        assert!(cursor.require_scope(scope(6, 2)).is_err());
    }

    #[test]
    fn rejects_unsupported_format_version() {
        // A hypothetical v2 cursor: version byte 0x02, then a valid header.
        let mut bytes = vec![0x02];
        bytes.extend_from_slice(&1i64.to_be_bytes());
        bytes.extend_from_slice(&0u32.to_be_bytes());
        let text = base32::encode(TEXT_PREFIX, &bytes);
        assert_eq!(
            ChangeCursor::decode(&text),
            Err(CursorParseError::UnsupportedVersion { version: 0x02 })
        );
    }

    #[test]
    fn rejects_truncated_header() {
        for len in 0..HEADER_LEN {
            let mut bytes = vec![0u8; len];
            if len > 0 {
                bytes[0] = FORMAT_VERSION;
            }
            let text = base32::encode(TEXT_PREFIX, &bytes);
            assert_eq!(
                ChangeCursor::decode(&text),
                Err(CursorParseError::Truncated),
                "header of {len} bytes must be rejected"
            );
        }
    }

    #[test]
    fn rejects_foreign_and_malformed_text() {
        // Identity-prefixed text is not a cursor.
        assert_eq!(
            ChangeCursor::decode("gdmzxq"),
            Err(CursorParseError::MissingPrefix)
        );
        // Uppercase and out-of-alphabet bytes are rejected with position.
        assert!(matches!(
            ChangeCursor::decode("gdc-MZXQ"),
            Err(CursorParseError::InvalidCharacter { .. })
        ));
        // Non-canonical residue.
        assert_eq!(
            ChangeCursor::decode("gdc-m"),
            Err(CursorParseError::NonCanonicalText)
        );
    }

    #[test]
    fn cursor_text_is_not_a_valid_item_id() {
        let cursor = ChangeCursor::new(scope(42, 7), b"pts:1".to_vec()).unwrap();
        assert!(crate::identity::ItemId::parse_text(&cursor.encode()).is_err());
    }

    #[test]
    fn error_messages_name_the_failure() {
        assert_eq!(
            CursorParseError::UnsupportedVersion { version: 9 }.to_string(),
            "unsupported change cursor format version 9"
        );
        assert_eq!(
            CursorParseError::MissingPrefix.to_string(),
            "change cursor text lacks the 'gdc-' prefix"
        );
        assert_eq!(
            InvalidCursorPayload { len: 5000 }.to_string(),
            "cursor payload is 5000 bytes; limit is 4096"
        );
    }
}
