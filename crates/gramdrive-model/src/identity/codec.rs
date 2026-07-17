//! Versioned binary and text serialization of [`ItemKey`] (format v1).
//!
//! Layout (all integers big-endian two's complement, all fields fixed width
//! once their tags are read, so the encoding is a prefix code and therefore
//! injective):
//!
//! ```text
//! byte 0            format version (0x01)
//! byte 1            item kind tag
//! bytes 2..         fields of that kind, in declaration order
//! ```
//!
//! The full tag and field tables live in the crate README, which is the
//! stability contract: v1 is frozen, and any change to this file that alters
//! an encoding fails the pinned golden tests. A future format is a new
//! version byte decoded alongside v1, never a mutation of v1.

use super::{
    AccountId, AccountKey, AccountScope, AppearanceKey, AttachmentIndex, AttachmentKey, BlobKey,
    CanonicalKey, ChatId, ChatKey, ChatListKey, ChatListKind, ContentHash, DocFormat, DocPartition,
    FolderCatalogKey, FolderId, GeneratedDocKey, IdParseError, ItemKey, MediaDirKey, MessageId,
    MessageKey, NamespaceVersion, SchemaFamily, YearDirKey,
};

const FORMAT_VERSION: u8 = 0x01;

// Item kind tags. 0x10 for appearance leaves the canonical range room to
// grow while keeping the two spaces visually distinct in a hex dump; the
// virtual tree builder's directory kinds occupy 0x08..0x0a.
const TAG_ACCOUNT: u8 = 0x01;
const TAG_CHAT_LIST: u8 = 0x02;
const TAG_CHAT: u8 = 0x03;
const TAG_MESSAGE: u8 = 0x04;
const TAG_ATTACHMENT: u8 = 0x05;
const TAG_GENERATED_DOC: u8 = 0x06;
const TAG_BLOB: u8 = 0x07;
const TAG_FOLDER_CATALOG: u8 = 0x08;
const TAG_YEAR_DIR: u8 = 0x09;
const TAG_MEDIA_DIR: u8 = 0x0a;
const TAG_APPEARANCE: u8 = 0x10;

const LIST_MAIN: u8 = 0x01;
const LIST_ARCHIVE: u8 = 0x02;
const LIST_FOLDER: u8 = 0x03;

const PARTITION_CHAT: u8 = 0x01;
const PARTITION_YEAR: u8 = 0x02;
const PARTITION_MONTH: u8 = 0x03;

const FORMAT_NDJSON: u8 = 0x01;
const FORMAT_MARKDOWN: u8 = 0x02;
const FORMAT_JSON: u8 = 0x03;

const HASH_SHA256: u8 = 0x01;

const FIELD_ITEM_KIND: &str = "item kind";
const FIELD_CANONICAL_KIND: &str = "canonical item kind";
const FIELD_LIST_KIND: &str = "chat list kind";
const FIELD_PARTITION: &str = "doc partition";
const FIELD_FORMAT: &str = "doc format";
const FIELD_HASH: &str = "content hash algorithm";

// ---------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------

pub(super) fn encode_key(key: &ItemKey) -> Vec<u8> {
    // Largest v1 key (blob appearance) is 49 bytes; consumers must size
    // buffers from the tested <=64-byte bound, never from this comment.
    let mut out = Vec::with_capacity(64);
    out.push(FORMAT_VERSION);
    match key {
        ItemKey::Canonical(canonical) => encode_canonical(&mut out, canonical),
        ItemKey::Appearance(AppearanceKey { view, item }) => {
            out.push(TAG_APPEARANCE);
            encode_list_kind(&mut out, view);
            encode_canonical(&mut out, item);
        }
    }
    out
}

fn encode_canonical(out: &mut Vec<u8>, key: &CanonicalKey) {
    match key {
        CanonicalKey::Account(account) => {
            out.push(TAG_ACCOUNT);
            encode_account(out, account);
        }
        CanonicalKey::ChatList(ChatListKey { scope, kind }) => {
            out.push(TAG_CHAT_LIST);
            encode_scope(out, scope);
            encode_list_kind(out, kind);
        }
        CanonicalKey::FolderCatalog(FolderCatalogKey { scope }) => {
            out.push(TAG_FOLDER_CATALOG);
            encode_scope(out, scope);
        }
        CanonicalKey::Chat(chat) => {
            out.push(TAG_CHAT);
            encode_chat(out, chat);
        }
        CanonicalKey::YearDir(YearDirKey { chat, year }) => {
            out.push(TAG_YEAR_DIR);
            encode_chat(out, chat);
            out.extend_from_slice(&year.to_be_bytes());
        }
        CanonicalKey::MediaDir(MediaDirKey { chat, year }) => {
            out.push(TAG_MEDIA_DIR);
            encode_chat(out, chat);
            out.extend_from_slice(&year.to_be_bytes());
        }
        CanonicalKey::Message(message) => {
            out.push(TAG_MESSAGE);
            encode_message(out, message);
        }
        CanonicalKey::Attachment(AttachmentKey { message, index }) => {
            out.push(TAG_ATTACHMENT);
            encode_message(out, message);
            out.extend_from_slice(&index.0.to_be_bytes());
        }
        CanonicalKey::GeneratedDoc(GeneratedDocKey {
            chat,
            partition,
            format,
            schema_family,
        }) => {
            out.push(TAG_GENERATED_DOC);
            encode_chat(out, chat);
            match partition {
                DocPartition::Chat => out.push(PARTITION_CHAT),
                DocPartition::Year { year } => {
                    out.push(PARTITION_YEAR);
                    out.extend_from_slice(&year.to_be_bytes());
                }
                DocPartition::Month { year, month } => {
                    out.push(PARTITION_MONTH);
                    out.extend_from_slice(&year.to_be_bytes());
                    out.push(*month);
                }
            }
            match format {
                DocFormat::Ndjson => out.push(FORMAT_NDJSON),
                DocFormat::Markdown => out.push(FORMAT_MARKDOWN),
                DocFormat::Json => out.push(FORMAT_JSON),
            }
            out.extend_from_slice(&schema_family.0.to_be_bytes());
        }
        CanonicalKey::Blob(BlobKey { account, hash }) => {
            out.push(TAG_BLOB);
            encode_account(out, account);
            match hash {
                ContentHash::Sha256(digest) => {
                    out.push(HASH_SHA256);
                    out.extend_from_slice(digest);
                }
            }
        }
    }
}

fn encode_account(out: &mut Vec<u8>, account: &AccountKey) {
    out.extend_from_slice(&account.account_id.0.to_be_bytes());
}

fn encode_scope(out: &mut Vec<u8>, scope: &AccountScope) {
    encode_account(out, &scope.account);
    out.extend_from_slice(&scope.namespace_version.0.to_be_bytes());
}

fn encode_chat(out: &mut Vec<u8>, chat: &ChatKey) {
    encode_scope(out, &chat.scope);
    out.extend_from_slice(&chat.chat_id.0.to_be_bytes());
}

fn encode_message(out: &mut Vec<u8>, message: &MessageKey) {
    encode_chat(out, &message.chat);
    out.extend_from_slice(&message.message_id.0.to_be_bytes());
}

fn encode_list_kind(out: &mut Vec<u8>, kind: &ChatListKind) {
    match kind {
        ChatListKind::Main => out.push(LIST_MAIN),
        ChatListKind::Archive => out.push(LIST_ARCHIVE),
        ChatListKind::Folder(FolderId(id)) => {
            out.push(LIST_FOLDER);
            out.extend_from_slice(&id.to_be_bytes());
        }
    }
}

// ---------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------

pub(super) fn decode_key(bytes: &[u8]) -> Result<ItemKey, IdParseError> {
    let mut reader = Reader { bytes };
    let version = reader.u8()?;
    if version != FORMAT_VERSION {
        return Err(IdParseError::UnsupportedVersion { version });
    }
    let tag = reader.u8()?;
    let key = if tag == TAG_APPEARANCE {
        let view = decode_list_kind(&mut reader)?;
        let inner_tag = reader.u8()?;
        let item = decode_canonical(&mut reader, inner_tag, FIELD_CANONICAL_KIND)?;
        ItemKey::Appearance(AppearanceKey { view, item })
    } else {
        ItemKey::Canonical(decode_canonical(&mut reader, tag, FIELD_ITEM_KIND)?)
    };
    reader.finish()?;
    Ok(key)
}

fn decode_canonical(
    reader: &mut Reader<'_>,
    tag: u8,
    field: &'static str,
) -> Result<CanonicalKey, IdParseError> {
    match tag {
        TAG_ACCOUNT => Ok(CanonicalKey::Account(decode_account(reader)?)),
        TAG_CHAT_LIST => {
            let scope = decode_scope(reader)?;
            let kind = decode_list_kind(reader)?;
            Ok(CanonicalKey::ChatList(ChatListKey { scope, kind }))
        }
        TAG_FOLDER_CATALOG => Ok(CanonicalKey::FolderCatalog(FolderCatalogKey {
            scope: decode_scope(reader)?,
        })),
        TAG_CHAT => Ok(CanonicalKey::Chat(decode_chat(reader)?)),
        TAG_YEAR_DIR => Ok(CanonicalKey::YearDir(YearDirKey {
            chat: decode_chat(reader)?,
            year: reader.u16()?,
        })),
        TAG_MEDIA_DIR => Ok(CanonicalKey::MediaDir(MediaDirKey {
            chat: decode_chat(reader)?,
            year: reader.u16()?,
        })),
        TAG_MESSAGE => Ok(CanonicalKey::Message(decode_message(reader)?)),
        TAG_ATTACHMENT => {
            let message = decode_message(reader)?;
            let index = AttachmentIndex(reader.u32()?);
            Ok(CanonicalKey::Attachment(AttachmentKey { message, index }))
        }
        TAG_GENERATED_DOC => {
            let chat = decode_chat(reader)?;
            let partition = match reader.u8()? {
                PARTITION_CHAT => DocPartition::Chat,
                PARTITION_YEAR => DocPartition::Year {
                    year: reader.u16()?,
                },
                PARTITION_MONTH => DocPartition::Month {
                    year: reader.u16()?,
                    month: reader.u8()?,
                },
                tag => {
                    return Err(IdParseError::UnknownTag {
                        tag,
                        field: FIELD_PARTITION,
                    });
                }
            };
            let format = match reader.u8()? {
                FORMAT_NDJSON => DocFormat::Ndjson,
                FORMAT_MARKDOWN => DocFormat::Markdown,
                FORMAT_JSON => DocFormat::Json,
                tag => {
                    return Err(IdParseError::UnknownTag {
                        tag,
                        field: FIELD_FORMAT,
                    });
                }
            };
            let schema_family = SchemaFamily(reader.u16()?);
            Ok(CanonicalKey::GeneratedDoc(GeneratedDocKey {
                chat,
                partition,
                format,
                schema_family,
            }))
        }
        TAG_BLOB => {
            let account = decode_account(reader)?;
            let hash = match reader.u8()? {
                HASH_SHA256 => ContentHash::Sha256(reader.array::<32>()?),
                tag => {
                    return Err(IdParseError::UnknownTag {
                        tag,
                        field: FIELD_HASH,
                    });
                }
            };
            Ok(CanonicalKey::Blob(BlobKey { account, hash }))
        }
        tag => Err(IdParseError::UnknownTag { tag, field }),
    }
}

fn decode_account(reader: &mut Reader<'_>) -> Result<AccountKey, IdParseError> {
    Ok(AccountKey {
        account_id: AccountId(reader.i64()?),
    })
}

fn decode_scope(reader: &mut Reader<'_>) -> Result<AccountScope, IdParseError> {
    Ok(AccountScope {
        account: decode_account(reader)?,
        namespace_version: NamespaceVersion(reader.u32()?),
    })
}

fn decode_chat(reader: &mut Reader<'_>) -> Result<ChatKey, IdParseError> {
    Ok(ChatKey {
        scope: decode_scope(reader)?,
        chat_id: ChatId(reader.i64()?),
    })
}

fn decode_message(reader: &mut Reader<'_>) -> Result<MessageKey, IdParseError> {
    Ok(MessageKey {
        chat: decode_chat(reader)?,
        message_id: MessageId(reader.i64()?),
    })
}

fn decode_list_kind(reader: &mut Reader<'_>) -> Result<ChatListKind, IdParseError> {
    match reader.u8()? {
        LIST_MAIN => Ok(ChatListKind::Main),
        LIST_ARCHIVE => Ok(ChatListKind::Archive),
        LIST_FOLDER => Ok(ChatListKind::Folder(FolderId(reader.i32()?))),
        tag => Err(IdParseError::UnknownTag {
            tag,
            field: FIELD_LIST_KIND,
        }),
    }
}

/// Panic-free cursor over the payload being decoded.
struct Reader<'a> {
    bytes: &'a [u8],
}

impl<'a> Reader<'a> {
    fn u8(&mut self) -> Result<u8, IdParseError> {
        match self.bytes.split_first() {
            Some((&byte, rest)) => {
                self.bytes = rest;
                Ok(byte)
            }
            None => Err(IdParseError::Truncated),
        }
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], IdParseError> {
        match (self.bytes.get(..N), self.bytes.get(N..)) {
            (Some(head), Some(rest)) => {
                self.bytes = rest;
                // A slice of length N always converts; map_err keeps the
                // code panic-free rather than handling a real case.
                head.try_into().map_err(|_| IdParseError::Truncated)
            }
            _ => Err(IdParseError::Truncated),
        }
    }

    fn u16(&mut self) -> Result<u16, IdParseError> {
        Ok(u16::from_be_bytes(self.array::<2>()?))
    }

    fn u32(&mut self) -> Result<u32, IdParseError> {
        Ok(u32::from_be_bytes(self.array::<4>()?))
    }

    fn i32(&mut self) -> Result<i32, IdParseError> {
        Ok(i32::from_be_bytes(self.array::<4>()?))
    }

    fn i64(&mut self) -> Result<i64, IdParseError> {
        Ok(i64::from_be_bytes(self.array::<8>()?))
    }

    fn finish(self) -> Result<(), IdParseError> {
        if self.bytes.is_empty() {
            Ok(())
        } else {
            Err(IdParseError::TrailingBytes {
                extra: self.bytes.len(),
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Text form: "gd" + unpadded lowercase base32 (RFC 4648 alphabet)
// ---------------------------------------------------------------------------
//
// Hand-rolled rather than a dependency: strict canonicality (exactly one
// valid text per byte string — lowercase only, no padding, zero trailing
// bits) is the property the identity contract needs, and it is easier to
// prove for forty lines under our own property tests than to audit in a
// third-party crate's flag matrix. Lowercase because identities appear in
// logs and URLs where case-folding environments are common; strict parsing
// keeps text -> bytes one-to-one anyway.

const TEXT_PREFIX: &str = "gd";
const ALPHABET: [u8; 32] = *b"abcdefghijklmnopqrstuvwxyz234567";

pub(super) fn encode_text(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(TEXT_PREFIX.len() + bytes.len().div_ceil(5) * 8);
    out.push_str(TEXT_PREFIX);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for &byte in bytes {
        acc = (acc << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(char::from(ALPHABET[((acc >> bits) & 0x1f) as usize]));
        }
        // Keep only the unconsumed low bits so `acc` stays within 12 bits
        // and `acc << 8` above can never overflow (checks are on in release).
        acc &= (1 << bits) - 1;
    }
    if bits > 0 {
        out.push(char::from(ALPHABET[((acc << (5 - bits)) & 0x1f) as usize]));
    }
    out
}

pub(super) fn decode_text(text: &str) -> Result<Vec<u8>, IdParseError> {
    let payload = text
        .strip_prefix(TEXT_PREFIX)
        .ok_or(IdParseError::MissingPrefix)?;
    let mut out = Vec::with_capacity(payload.len() * 5 / 8);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for (index, &byte) in payload.as_bytes().iter().enumerate() {
        let value = match byte {
            b'a'..=b'z' => byte - b'a',
            b'2'..=b'7' => byte - b'2' + 26,
            _ => {
                return Err(IdParseError::InvalidCharacter {
                    position: TEXT_PREFIX.len() + index,
                });
            }
        };
        acc = (acc << 5) | u32::from(value);
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    // A canonical unpadded encoding leaves fewer than 5 leftover bits (5+
    // means a character count no byte string produces) and those bits are
    // zero (RFC 4648 pads the final quantum with zeros).
    if bits >= 5 || acc != 0 {
        return Err(IdParseError::NonCanonicalText);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 4648 test vectors, lowercased and unpadded.
    #[test]
    fn base32_matches_rfc4648_vectors() {
        let vectors: [(&[u8], &str); 6] = [
            (b"", ""),
            (b"f", "my"),
            (b"fo", "mzxq"),
            (b"foo", "mzxw6"),
            (b"foob", "mzxw6yq"),
            (b"fooba", "mzxw6ytb"),
        ];
        for (bytes, expected) in vectors {
            let text = encode_text(bytes);
            assert_eq!(text, format!("gd{expected}"));
            assert_eq!(decode_text(&text).unwrap(), bytes);
        }
    }

    #[test]
    fn text_rejects_uppercase_padding_and_out_of_alphabet() {
        for bad in ["gdMZXQ", "gdmzxq====", "gdmzx q", "gdmzx1"] {
            assert!(matches!(
                decode_text(bad),
                Err(IdParseError::InvalidCharacter { .. })
            ));
        }
    }

    #[test]
    fn text_rejects_invalid_length_residues() {
        // 1, 3, and 6 characters can never be produced by encoding.
        for bad in ["gdm", "gdmzx", "gdmzxw6y"] {
            assert_eq!(decode_text(bad), Err(IdParseError::NonCanonicalText));
        }
    }

    #[test]
    fn text_rejects_nonzero_padding_bits() {
        // "mz" decodes to one byte with 2 leftover bits; "z" (11001) leaves
        // them nonzero where canonical encoding of 0x66 yields "my".
        assert_eq!(decode_text("gdmz"), Err(IdParseError::NonCanonicalText));
    }

    #[test]
    fn text_requires_prefix() {
        assert_eq!(decode_text("mzxq"), Err(IdParseError::MissingPrefix));
        assert_eq!(decode_text(""), Err(IdParseError::MissingPrefix));
    }

    #[test]
    fn invalid_character_position_is_absolute() {
        assert_eq!(
            decode_text("gdmzX"),
            Err(IdParseError::InvalidCharacter { position: 4 })
        );
    }
}
