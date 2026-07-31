use bytes::Bytes;
use mail_protocol_core::wire::eq_ascii;
use mail_protocol_core::{ErrorKind, ProtocolError};

/// Validated, zero-copy legacy `SEARCH` response data.
///
/// `IMAP4rev2` normally uses `ESEARCH`, but `IMAP4rev1` servers and RFC 7162
/// CONDSTORE extension can still return this response form.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchResponse {
    wire: Bytes,
    results_end: usize,
    result_count: usize,
    mod_sequence: Option<u64>,
}

impl SearchResponse {
    /// Parses complete untagged response data beginning with `SEARCH`.
    ///
    /// # Errors
    ///
    /// Returns an error for zero/out-of-range message numbers, malformed
    /// separators, a misplaced MODSEQ suffix, or a MODSEQ outside the positive
    /// signed 63-bit range required by RFC 7162.
    pub fn parse(wire: &Bytes) -> Result<Self, ProtocolError> {
        const NAME: &[u8] = b"SEARCH";
        if wire.len() < NAME.len() || !eq_ascii(&wire[..NAME.len()], NAME) {
            return Err(invalid("IMAP SEARCH response name"));
        }
        if wire.len() > NAME.len() && wire[NAME.len()] != b' ' {
            return Err(invalid("IMAP SEARCH response separator").at(NAME.len()));
        }

        let mut cursor = NAME.len();
        let mut result_count = 0usize;
        let mut results_end = wire.len();
        let mut mod_sequence = None;
        while cursor < wire.len() {
            if wire[cursor] != b' ' {
                return Err(invalid("IMAP SEARCH result separator").at(cursor));
            }
            let start = cursor + 1;
            if start == wire.len() {
                return Err(invalid("trailing IMAP SEARCH separator").at(cursor));
            }

            if wire[start] == b'(' {
                if result_count == 0 {
                    return Err(invalid("IMAP SEARCH MODSEQ without results").at(start));
                }
                mod_sequence = Some(parse_mod_sequence(&wire[start..], start)?);
                results_end = cursor;
                cursor = wire.len();
                continue;
            }

            let end = wire[start..]
                .iter()
                .position(|byte| *byte == b' ')
                .map_or(wire.len(), |offset| start + offset);
            parse_nz_u32(&wire[start..end], "IMAP SEARCH result")
                .map_err(|error| error.at(start))?;
            result_count = result_count
                .checked_add(1)
                .ok_or_else(|| invalid("IMAP SEARCH result count"))?;
            cursor = end;
        }

        Ok(Self {
            wire: wire.clone(),
            results_end,
            result_count,
            mod_sequence,
        })
    }

    /// Returns the exact validated response data beginning with `SEARCH`.
    pub const fn as_bytes(&self) -> &Bytes {
        &self.wire
    }

    /// Iterates over matching message sequence numbers without allocating.
    pub fn results(&self) -> SearchResultIter<'_> {
        SearchResultIter {
            wire: &self.wire,
            cursor: b"SEARCH".len(),
            end: self.results_end,
            remaining: self.result_count,
        }
    }

    /// Returns the RFC 7162 maximum modification sequence, when present.
    pub const fn mod_sequence(&self) -> Option<u64> {
        self.mod_sequence
    }

    /// Returns the number of matching sequence numbers.
    pub const fn len(&self) -> usize {
        self.result_count
    }

    /// Returns whether the result set is empty.
    pub const fn is_empty(&self) -> bool {
        self.result_count == 0
    }
}

/// Allocation-free iterator over validated legacy SEARCH results.
#[derive(Clone, Debug)]
pub struct SearchResultIter<'a> {
    wire: &'a [u8],
    cursor: usize,
    end: usize,
    remaining: usize,
}

impl Iterator for SearchResultIter<'_> {
    type Item = u32;

    fn next(&mut self) -> Option<Self::Item> {
        if self.cursor == self.end {
            return None;
        }
        let start = self.cursor.checked_add(1)?;
        let end = self.wire[start..self.end]
            .iter()
            .position(|byte| *byte == b' ')
            .map_or(self.end, |offset| start + offset);
        let value = parse_nz_u32(&self.wire[start..end], "validated IMAP SEARCH result").ok()?;
        self.cursor = end;
        self.remaining -= 1;
        Some(value)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl ExactSizeIterator for SearchResultIter<'_> {}

fn parse_mod_sequence(input: &[u8], offset: usize) -> Result<u64, ProtocolError> {
    const PREFIX: &[u8] = b"(MODSEQ ";
    if input.len() <= PREFIX.len()
        || input.last() != Some(&b')')
        || !eq_ascii(&input[..PREFIX.len()], PREFIX)
    {
        return Err(invalid("IMAP SEARCH MODSEQ suffix").at(offset));
    }
    let value = parse_decimal(&input[PREFIX.len()..input.len() - 1], "IMAP SEARCH MODSEQ")
        .map_err(|error| error.at(offset + PREFIX.len()))?;
    if value == 0 || value > i64::MAX as u64 {
        return Err(invalid("IMAP SEARCH MODSEQ range").at(offset + PREFIX.len()));
    }
    Ok(value)
}

fn parse_nz_u32(input: &[u8], context: &'static str) -> Result<u32, ProtocolError> {
    if input.first().is_none_or(|first| *first == b'0') {
        return Err(invalid(context));
    }
    let value = parse_decimal(input, context)?;
    u32::try_from(value).map_err(|_| invalid(context))
}

fn parse_decimal(input: &[u8], context: &'static str) -> Result<u64, ProtocolError> {
    if input.is_empty() || !input.iter().all(u8::is_ascii_digit) {
        return Err(invalid(context));
    }
    input.iter().try_fold(0u64, |value, digit| {
        value
            .checked_mul(10)
            .and_then(|value| value.checked_add(u64::from(*digit - b'0')))
            .ok_or_else(|| invalid(context))
    })
}

const fn invalid(context: &'static str) -> ProtocolError {
    ProtocolError::new(ErrorKind::InvalidSyntax, context)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_empty_results_and_condstore_suffix_without_allocation() {
        let empty = SearchResponse::parse(&Bytes::from_static(b"SEARCH")).unwrap();
        assert!(empty.is_empty());
        assert_eq!(empty.results().len(), 0);
        assert_eq!(empty.mod_sequence(), None);

        let response = SearchResponse::parse(&Bytes::from_static(
            b"search 2 5 4294967295 (modseq 917162500)",
        ))
        .unwrap();
        assert_eq!(response.results().collect::<Vec<_>>(), vec![2, 5, u32::MAX]);
        assert_eq!(response.mod_sequence(), Some(917_162_500));
        assert_eq!(
            response.as_bytes().as_ref(),
            b"search 2 5 4294967295 (modseq 917162500)"
        );
    }

    #[test]
    fn rejects_malformed_numbers_modseq_and_separators() {
        for invalid_wire in [
            b"SEARCH ".as_slice(),
            b"SEARCH 0",
            b"SEARCH 01",
            b"SEARCH 4294967296",
            b"SEARCH 1  2",
            b"SEARCH\t1",
            b"SEARCH (MODSEQ 1)",
            b"SEARCH 1 (MODSEQ 0)",
            b"SEARCH 1 (MODSEQ 9223372036854775808)",
            b"SEARCH 1 (MODSEQ 2) extra",
            b"SEARCH 1 (X 2)",
        ] {
            assert!(
                SearchResponse::parse(&Bytes::copy_from_slice(invalid_wire)).is_err(),
                "accepted {invalid_wire:?}"
            );
        }
    }
}
