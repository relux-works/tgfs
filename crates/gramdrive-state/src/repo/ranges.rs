//! The stored form of byte-range lists (`transfers.requested_ranges` /
//! `completed_ranges`): a JSON array of `[start, end)` pairs.
//!
//! Hand-rolled on purpose. The format is fixed and tiny — `[[0,5],[10,20]]`
//! — and a JSON dependency for it would be pure supply-chain surface
//! (POL-6). The decoder accepts exactly what the encoder writes plus
//! insignificant whitespace; anything else is reported as corruption by the
//! caller, never coerced.

use gramdrive_model::ByteRange;

/// Why stored range text failed to decode. The caller wraps this into
/// [`crate::StateError::CorruptRow`] with its table context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RangeDecodeError {
    /// What was malformed.
    pub detail: String,
}

/// Encodes ranges as the stored JSON form. Fails (with a description for
/// [`crate::StateError::InvalidArgument`]) if an offset exceeds the SQLite
/// INTEGER range — a caller bug, not a storable value.
pub(crate) fn encode(ranges: &[ByteRange]) -> Result<String, &'static str> {
    let mut text = String::with_capacity(2 + ranges.len() * 16);
    text.push('[');
    for (index, range) in ranges.iter().enumerate() {
        if range.end() > i64::MAX as u64 {
            return Err("byte range end exceeds the SQLite INTEGER range");
        }
        if index > 0 {
            text.push(',');
        }
        text.push('[');
        text.push_str(&range.start().to_string());
        text.push(',');
        text.push_str(&range.end().to_string());
        text.push(']');
    }
    text.push(']');
    Ok(text)
}

/// Decodes the stored JSON form back into ranges.
pub(crate) fn decode(text: &str) -> Result<Vec<ByteRange>, RangeDecodeError> {
    let mut parser = Parser {
        bytes: text.as_bytes(),
        position: 0,
    };
    parser.skip_whitespace();
    parser.expect(b'[')?;
    let mut ranges = Vec::new();
    parser.skip_whitespace();
    if parser.peek() == Some(b']') {
        parser.position += 1;
    } else {
        loop {
            ranges.push(parser.pair()?);
            parser.skip_whitespace();
            match parser.next() {
                Some(b',') => parser.skip_whitespace(),
                Some(b']') => break,
                _ => return Err(parser.fail("expected ',' or ']' after a range pair")),
            }
        }
    }
    parser.skip_whitespace();
    if parser.position != parser.bytes.len() {
        return Err(parser.fail("trailing bytes after the range list"));
    }
    Ok(ranges)
}

struct Parser<'text> {
    bytes: &'text [u8],
    position: usize,
}

impl Parser<'_> {
    fn fail(&self, detail: &str) -> RangeDecodeError {
        RangeDecodeError {
            detail: format!("{detail} at byte {}", self.position),
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.position).copied()
    }

    fn next(&mut self) -> Option<u8> {
        let byte = self.peek()?;
        self.position += 1;
        Some(byte)
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.position += 1;
        }
    }

    fn expect(&mut self, expected: u8) -> Result<(), RangeDecodeError> {
        if self.next() == Some(expected) {
            Ok(())
        } else {
            self.position = self.position.saturating_sub(1);
            Err(self.fail("unexpected byte"))
        }
    }

    fn number(&mut self) -> Result<u64, RangeDecodeError> {
        let start = self.position;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.position += 1;
        }
        if self.position == start {
            return Err(self.fail("expected a number"));
        }
        // The digits are ASCII by construction; only overflow can fail.
        std::str::from_utf8(&self.bytes[start..self.position])
            .ok()
            .and_then(|digits| digits.parse().ok())
            .ok_or_else(|| self.fail("number does not fit u64"))
    }

    fn pair(&mut self) -> Result<ByteRange, RangeDecodeError> {
        self.skip_whitespace();
        self.expect(b'[')?;
        self.skip_whitespace();
        let start = self.number()?;
        self.skip_whitespace();
        self.expect(b',')?;
        self.skip_whitespace();
        let end = self.number()?;
        self.skip_whitespace();
        self.expect(b']')?;
        ByteRange::new(start, end).map_err(|error| self.fail(&format!("invalid range: {error}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(start: u64, end: u64) -> ByteRange {
        ByteRange::new(start, end).unwrap()
    }

    #[test]
    fn round_trips_lists() {
        for ranges in [
            vec![],
            vec![range(0, 5)],
            vec![range(0, 5), range(10, 20), range(1 << 40, (1 << 40) + 1)],
        ] {
            let text = encode(&ranges).unwrap();
            assert_eq!(decode(&text).unwrap(), ranges, "text: {text}");
        }
        assert_eq!(
            encode(&[range(0, 5), range(10, 20)]).unwrap(),
            "[[0,5],[10,20]]"
        );
    }

    #[test]
    fn accepts_insignificant_whitespace() {
        assert_eq!(
            decode(" [ [0, 5] , [10,\n20] ] ").unwrap(),
            vec![range(0, 5), range(10, 20)]
        );
    }

    #[test]
    fn rejects_malformed_text() {
        for text in [
            "",
            "[",
            "]",
            "[[0,5]",
            "[[0,5],]",
            "[[5,0]]",   // inverted
            "[[0,0]]",   // empty range
            "[[0,5]]x",  // trailing bytes
            "[[-1,5]]",  // negative
            "[[0,5.0]]", // fraction
            "[0,5]",     // pair without list
            "[[0]]",     // missing end
        ] {
            assert!(decode(text).is_err(), "must reject {text:?}");
        }
    }

    #[test]
    fn rejects_offsets_beyond_the_integer_range() {
        let too_large = ByteRange::new(0, u64::MAX).unwrap();
        assert!(encode(&[too_large]).is_err());
    }
}
