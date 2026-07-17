//! Crate-private unpadded lowercase RFC 4648 base32 text codec.
//!
//! One implementation serves every prefixed opaque text form the model
//! mints — `ItemId` (`"gd"`, `identity::codec`) and `ChangeCursor`
//! (`"gdc-"`, `cursor`). Sharing the codec is what keeps "canonical text"
//! meaning the same thing across those namespaces: lowercase only, no
//! padding, zero trailing bits, so each byte string has exactly one valid
//! spelling.
//!
//! Callers pass their namespace prefix explicitly; the prefixes themselves
//! are chosen so no valid text of one namespace parses in another (a cursor
//! string fails `ItemId` decoding at its `-`, an identity string fails
//! cursor decoding at the missing `"gdc-"` prefix).

/// The RFC 4648 base32 alphabet, lowercased. Chosen for identity text in
/// TASK-260715-1qz1g5: survives case-insensitive filesystems and URL
/// encoding without escaping.
const ALPHABET: [u8; 32] = *b"abcdefghijklmnopqrstuvwxyz234567";

/// Why a text string is not a canonical encoding for the given prefix.
///
/// Deliberately vocabulary-free: each namespace maps these onto its own
/// parse-error type so diagnostics name the thing that failed to parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TextDecodeError {
    /// The text does not start with the namespace prefix.
    MissingPrefix,
    /// A byte outside the lowercase base32 alphabet, at this byte offset
    /// within the full input (prefix included).
    InvalidCharacter { position: usize },
    /// Not the canonical encoding of any byte string: an impossible length
    /// residue or nonzero padding bits.
    NonCanonical,
}

pub(crate) fn encode(prefix: &str, bytes: &[u8]) -> String {
    let mut out = String::with_capacity(prefix.len() + bytes.len().div_ceil(5) * 8);
    out.push_str(prefix);
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

pub(crate) fn decode(prefix: &str, text: &str) -> Result<Vec<u8>, TextDecodeError> {
    let payload = text
        .strip_prefix(prefix)
        .ok_or(TextDecodeError::MissingPrefix)?;
    let mut out = Vec::with_capacity(payload.len() * 5 / 8);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    for (index, &byte) in payload.as_bytes().iter().enumerate() {
        let value = match byte {
            b'a'..=b'z' => byte - b'a',
            b'2'..=b'7' => byte - b'2' + 26,
            _ => {
                return Err(TextDecodeError::InvalidCharacter {
                    position: prefix.len() + index,
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
        return Err(TextDecodeError::NonCanonical);
    }
    Ok(out)
}
