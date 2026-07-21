use std::ops::Range;

use bytes::{Bytes, BytesMut};
use mail_protocol_core::{ErrorKind, ProtocolError};

use crate::astring::parse_astring_prefix;
use crate::{AString, AStringKind, Command, CommandBody, SearchProgram};

/// Default maximum nesting accepted in SORT search criteria.
pub const DEFAULT_SORT_MAX_DEPTH: usize = 64;

/// Maximum number of sort criteria accepted in one SORT command.
pub const MAX_SORT_CRITERIA: usize = 64;

/// One RFC 5256 sort key.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum SortKey<'a> {
    /// Internal date and time of arrival.
    Arrival,
    /// First `cc` mailbox.
    Cc,
    /// Sent date and time.
    Date,
    /// First `from` mailbox.
    From,
    /// Message size.
    Size,
    /// Normalized subject.
    Subject,
    /// First `to` mailbox.
    To,
    /// An extension sort key.
    Other(&'a [u8]),
}

impl<'a> SortKey<'a> {
    /// Returns the sort-key name.
    pub const fn name(self) -> &'a [u8] {
        match self {
            Self::Arrival => b"ARRIVAL",
            Self::Cc => b"CC",
            Self::Date => b"DATE",
            Self::From => b"FROM",
            Self::Size => b"SIZE",
            Self::Subject => b"SUBJECT",
            Self::To => b"TO",
            Self::Other(name) => name,
        }
    }
}

/// One RFC 5256 sort criterion.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SortCriterion<'a> {
    reverse: bool,
    key: SortKey<'a>,
}

impl<'a> SortCriterion<'a> {
    /// Returns whether this criterion reverses the key's natural order.
    pub const fn is_reverse(self) -> bool {
        self.reverse
    }

    /// Returns the typed sort key.
    pub const fn key(self) -> SortKey<'a> {
        self.key
    }
}

/// Allocation-free iterator over validated SORT criteria.
#[derive(Clone, Debug)]
pub struct SortCriterionIter<'a> {
    remaining: &'a [u8],
}

impl<'a> Iterator for SortCriterionIter<'a> {
    type Item = SortCriterion<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining.is_empty() {
            return None;
        }
        let parsed = match parse_criterion(self.remaining, 0) {
            Ok(parsed) => parsed,
            Err(error) => {
                debug_assert!(false, "validated SORT criterion became invalid: {error}");
                self.remaining = b"";
                return None;
            }
        };
        self.remaining = if parsed.end == self.remaining.len() {
            b""
        } else {
            self.remaining.get(parsed.end + 1..)?
        };
        Some(parsed.criterion)
    }
}

/// Validated, zero-copy RFC 5256 SORT command arguments.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SortArguments {
    wire: Bytes,
    criteria: Range<usize>,
    charset: AString,
    search: SearchProgram,
}

impl SortArguments {
    /// Parses SORT arguments with the default search nesting limit.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed or empty sort criteria, an invalid
    /// charset, missing search criteria, or an embedded SEARCH `RETURN` or
    /// `CHARSET` prefix.
    pub fn parse(wire: &Bytes) -> Result<Self, ProtocolError> {
        Self::parse_with_max_depth(wire, DEFAULT_SORT_MAX_DEPTH)
    }

    /// Parses SORT arguments with an explicit search nesting limit.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::parse`] and `NestingTooDeep` when a
    /// search key exceeds `max_depth`.
    pub fn parse_with_max_depth(wire: &Bytes, max_depth: usize) -> Result<Self, ProtocolError> {
        let parsed = validate_sort_arguments(wire, max_depth)?;
        let charset = AString::parse(&wire.slice(parsed.charset.clone()))?;
        let search = SearchProgram::parse_with_max_depth(&wire.slice(parsed.search), max_depth)?;
        Ok(Self {
            wire: wire.clone(),
            criteria: parsed.criteria,
            charset,
            search,
        })
    }

    /// Returns the exact bytes following the SORT command name.
    pub const fn as_bytes(&self) -> &Bytes {
        &self.wire
    }

    /// Returns the complete parenthesized sort-criterion list.
    pub fn criteria_bytes(&self) -> &[u8] {
        &self.wire[self.criteria.clone()]
    }

    /// Iterates over typed sort criteria without allocating.
    pub fn criteria(&self) -> SortCriterionIter<'_> {
        SortCriterionIter {
            remaining: &self.wire[self.criteria.start + 1..self.criteria.end - 1],
        }
    }

    /// Returns the mandatory charset.
    pub const fn charset(&self) -> &AString {
        &self.charset
    }

    /// Returns the validated search program following the charset.
    pub const fn search_program(&self) -> &SearchProgram {
        &self.search
    }

    /// Appends the validated arguments exactly as received.
    pub fn encode(&self, destination: &mut BytesMut) {
        destination.extend_from_slice(&self.wire);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ParsedSortArguments {
    pub(crate) criteria: Range<usize>,
    pub(crate) charset: Range<usize>,
    pub(crate) search: Range<usize>,
}

/// Validates SORT arguments while borrowing a complete command frame.
pub(crate) fn validate_sort_arguments(
    input: &[u8],
    max_depth: usize,
) -> Result<ParsedSortArguments, ProtocolError> {
    if input.first() != Some(&b'(') {
        return Err(invalid("IMAP SORT criterion list").at(0));
    }
    let mut cursor = 1usize;
    if input.get(cursor) == Some(&b')') {
        return Err(invalid("empty IMAP SORT criterion list").at(cursor));
    }
    let mut count = 0usize;
    loop {
        if count == MAX_SORT_CRITERIA {
            return Err(invalid("too many IMAP SORT criteria").at(cursor));
        }
        let parsed = parse_criterion(input, cursor)?;
        count += 1;
        cursor = parsed.end;
        match input.get(cursor) {
            Some(b')') => {
                cursor += 1;
                break;
            }
            Some(b' ') => cursor += 1,
            _ => return Err(invalid("IMAP SORT criterion separator").at(cursor)),
        }
        if input.get(cursor) == Some(&b')') {
            return Err(invalid("trailing IMAP SORT criterion separator").at(cursor));
        }
    }
    let criteria = 0..cursor;

    cursor = require_space(input, cursor, "IMAP SORT charset separator")?;
    let charset_start = cursor;
    let parsed_charset = parse_astring_prefix(&input[charset_start..])
        .map_err(|error| shift_error(error, charset_start))?;
    if matches!(parsed_charset.kind, AStringKind::Literal { .. }) {
        return Err(invalid("literal IMAP SORT charset").at(charset_start));
    }
    cursor = charset_start + parsed_charset.end;
    let charset = charset_start..cursor;

    cursor = require_space(input, cursor, "IMAP SORT search separator")?;
    if cursor == input.len() {
        return Err(invalid("missing IMAP SORT search criteria").at(cursor));
    }
    crate::search::validate_search_program(&input[cursor..], max_depth)
        .map_err(|error| shift_error(error, cursor))?;
    let first_end = input[cursor..]
        .iter()
        .position(|byte| *byte == b' ')
        .map_or(input.len(), |offset| cursor + offset);
    if input[cursor..first_end].eq_ignore_ascii_case(b"RETURN")
        || input[cursor..first_end].eq_ignore_ascii_case(b"CHARSET")
    {
        return Err(invalid("prefixed IMAP SORT search program").at(cursor));
    }

    Ok(ParsedSortArguments {
        criteria,
        charset,
        search: cursor..input.len(),
    })
}

impl Command {
    /// Returns typed arguments for direct or UID SORT commands.
    ///
    /// # Errors
    ///
    /// Returns an error if a manually constructed UID or raw command contains
    /// invalid SORT arguments.
    pub fn parsed_sort_arguments(&self) -> Result<Option<SortArguments>, ProtocolError> {
        match &self.body {
            CommandBody::Sort { arguments } => Ok(Some(arguments.clone())),
            CommandBody::Uid { command, arguments } if command.eq_ignore_ascii_case(b"SORT") => {
                SortArguments::parse(arguments).map(Some)
            }
            CommandBody::Raw { name, arguments } if name.eq_ignore_ascii_case(b"SORT") => {
                SortArguments::parse(arguments).map(Some)
            }
            _ => Ok(None),
        }
    }
}

/// Validated, zero-copy SORT response data.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SortResponse {
    wire: Bytes,
    results: Option<Range<usize>>,
    result_count: usize,
    mod_sequence: Option<u64>,
}

impl SortResponse {
    /// Parses complete untagged response data beginning with `SORT`.
    ///
    /// RFC 7162's optional `(MODSEQ value)` suffix is accepted only after at
    /// least one result number.
    ///
    /// # Errors
    ///
    /// Returns an error for zero or overflowing result numbers, malformed
    /// separators, a misplaced MODSEQ suffix, or a modification sequence above
    /// the signed 63-bit protocol limit.
    pub fn parse(wire: &Bytes) -> Result<Self, ProtocolError> {
        const NAME: &[u8] = b"SORT";
        if wire.len() < NAME.len() || !wire[..NAME.len()].eq_ignore_ascii_case(NAME) {
            return Err(invalid("IMAP SORT response name"));
        }
        if wire.len() == NAME.len() {
            return Ok(Self {
                wire: wire.clone(),
                results: None,
                result_count: 0,
                mod_sequence: None,
            });
        }
        if wire.get(NAME.len()) != Some(&b' ') {
            return Err(invalid("IMAP SORT response separator").at(NAME.len()));
        }

        let mut cursor = NAME.len() + 1;
        let mut results_start = None;
        let mut results_end = cursor;
        let mut result_count = 0usize;
        let mut mod_sequence = None;
        loop {
            if keyword_at(wire, cursor, b"(MODSEQ ") {
                if result_count == 0 {
                    return Err(invalid("IMAP SORT MODSEQ without results").at(cursor));
                }
                let value_start = cursor + b"(MODSEQ ".len();
                let close = wire[value_start..]
                    .iter()
                    .position(|byte| *byte == b')')
                    .map_or(wire.len(), |offset| value_start + offset);
                let value = parse_decimal(
                    &wire[value_start..close],
                    i64::MAX as u64,
                    true,
                    "IMAP SORT MODSEQ",
                )?;
                if close + 1 != wire.len() {
                    return Err(invalid("trailing IMAP SORT response data").at(close + 1));
                }
                mod_sequence = Some(value);
                break;
            }

            let end = wire[cursor..]
                .iter()
                .position(|byte| *byte == b' ')
                .map_or(wire.len(), |offset| cursor + offset);
            parse_decimal(
                &wire[cursor..end],
                u64::from(u32::MAX),
                true,
                "IMAP SORT result",
            )?;
            results_start.get_or_insert(cursor);
            results_end = end;
            result_count = result_count
                .checked_add(1)
                .ok_or_else(|| invalid("too many IMAP SORT results"))?;
            if end == wire.len() {
                break;
            }
            cursor = end + 1;
            if cursor == wire.len() {
                return Err(invalid("trailing IMAP SORT response separator").at(cursor));
            }
        }

        Ok(Self {
            wire: wire.clone(),
            results: results_start.map(|start| start..results_end),
            result_count,
            mod_sequence,
        })
    }

    /// Returns the complete response data beginning with `SORT`.
    pub const fn as_bytes(&self) -> &Bytes {
        &self.wire
    }

    /// Iterates over result message numbers without allocating.
    pub fn results(&self) -> SortResultIter<'_> {
        SortResultIter {
            remaining: self
                .results
                .as_ref()
                .map_or(b"".as_slice(), |range| &self.wire[range.clone()]),
        }
    }

    /// Returns the number of result message numbers.
    pub const fn result_count(&self) -> usize {
        self.result_count
    }

    /// Returns the optional RFC 7162 modification sequence.
    pub const fn mod_sequence(&self) -> Option<u64> {
        self.mod_sequence
    }

    /// Appends the validated response data exactly as received.
    pub fn encode(&self, destination: &mut BytesMut) {
        destination.extend_from_slice(&self.wire);
    }
}

/// Allocation-free iterator over SORT result numbers.
#[derive(Clone, Debug)]
pub struct SortResultIter<'a> {
    remaining: &'a [u8],
}

impl Iterator for SortResultIter<'_> {
    type Item = u32;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining.is_empty() {
            return None;
        }
        let end = self
            .remaining
            .iter()
            .position(|byte| *byte == b' ')
            .unwrap_or(self.remaining.len());
        let value = u32::try_from(
            parse_decimal(
                &self.remaining[..end],
                u64::from(u32::MAX),
                true,
                "IMAP SORT result",
            )
            .ok()?,
        )
        .ok()?;
        self.remaining = if end == self.remaining.len() {
            b""
        } else {
            &self.remaining[end + 1..]
        };
        Some(value)
    }
}

struct ParsedCriterion<'a> {
    criterion: SortCriterion<'a>,
    end: usize,
}

fn parse_criterion(input: &[u8], start: usize) -> Result<ParsedCriterion<'_>, ProtocolError> {
    let first_end = token_end(input, start);
    let first = &input[start..first_end];
    let (reverse, key_start, key_end) = if first.eq_ignore_ascii_case(b"REVERSE") {
        let key_start = require_space(input, first_end, "IMAP SORT REVERSE separator")?;
        (true, key_start, token_end(input, key_start))
    } else {
        (false, start, first_end)
    };
    let key_bytes = &input[key_start..key_end];
    validate_sort_key(key_bytes).map_err(|error| shift_error(error, key_start))?;
    if key_bytes.eq_ignore_ascii_case(b"REVERSE") {
        return Err(invalid("IMAP SORT key after REVERSE").at(key_start));
    }
    Ok(ParsedCriterion {
        criterion: SortCriterion {
            reverse,
            key: classify_key(key_bytes),
        },
        end: key_end,
    })
}

fn validate_sort_key(key: &[u8]) -> Result<(), ProtocolError> {
    let parsed = parse_astring_prefix(key)?;
    if parsed.end != key.len() || parsed.kind != AStringKind::Atom {
        return Err(invalid("IMAP SORT key"));
    }
    Ok(())
}

fn classify_key(key: &[u8]) -> SortKey<'_> {
    if key.eq_ignore_ascii_case(b"ARRIVAL") {
        SortKey::Arrival
    } else if key.eq_ignore_ascii_case(b"CC") {
        SortKey::Cc
    } else if key.eq_ignore_ascii_case(b"DATE") {
        SortKey::Date
    } else if key.eq_ignore_ascii_case(b"FROM") {
        SortKey::From
    } else if key.eq_ignore_ascii_case(b"SIZE") {
        SortKey::Size
    } else if key.eq_ignore_ascii_case(b"SUBJECT") {
        SortKey::Subject
    } else if key.eq_ignore_ascii_case(b"TO") {
        SortKey::To
    } else {
        SortKey::Other(key)
    }
}

fn parse_decimal(
    digits: &[u8],
    maximum: u64,
    non_zero: bool,
    context: &'static str,
) -> Result<u64, ProtocolError> {
    if digits.is_empty()
        || !digits.iter().all(u8::is_ascii_digit)
        || digits.len() > 1 && digits[0] == b'0'
    {
        return Err(invalid(context));
    }
    let value = digits.iter().try_fold(0u64, |value, digit| {
        value
            .checked_mul(10)
            .and_then(|value| value.checked_add(u64::from(*digit - b'0')))
    });
    value
        .filter(|value| *value <= maximum && (!non_zero || *value != 0))
        .ok_or_else(|| invalid(context))
}

fn token_end(input: &[u8], start: usize) -> usize {
    input[start..]
        .iter()
        .position(|byte| matches!(byte, b' ' | b')'))
        .map_or(input.len(), |offset| start + offset)
}

fn keyword_at(input: &[u8], start: usize, keyword: &[u8]) -> bool {
    input
        .get(start..start.saturating_add(keyword.len()))
        .is_some_and(|value| value.eq_ignore_ascii_case(keyword))
}

fn require_space(
    input: &[u8],
    cursor: usize,
    context: &'static str,
) -> Result<usize, ProtocolError> {
    if input.get(cursor) == Some(&b' ') {
        Ok(cursor + 1)
    } else {
        Err(invalid(context).at(cursor))
    }
}

fn shift_error(error: ProtocolError, start: usize) -> ProtocolError {
    error.at(start.saturating_add(error.offset().unwrap_or(0)))
}

const fn invalid(context: &'static str) -> ProtocolError {
    ProtocolError::new(ErrorKind::InvalidSyntax, context)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(input: &'static [u8]) -> Result<SortArguments, ProtocolError> {
        SortArguments::parse(&Bytes::from_static(input))
    }

    fn response(input: &'static [u8]) -> Result<SortResponse, ProtocolError> {
        SortResponse::parse(&Bytes::from_static(input))
    }

    #[test]
    fn parses_sort_arguments_and_extension_keys() {
        let value = arguments(b"(REVERSE DATE SUBJECT X-SPAM-SCORE) UTF-8 NOT DELETED").unwrap();
        let criteria: Vec<_> = value.criteria().collect();
        assert_eq!(criteria.len(), 3);
        assert!(criteria[0].is_reverse());
        assert_eq!(criteria[0].key(), SortKey::Date);
        assert_eq!(criteria[1].key(), SortKey::Subject);
        assert_eq!(criteria[2].key(), SortKey::Other(b"X-SPAM-SCORE"));
        assert_eq!(value.charset().decoded().as_ref(), b"UTF-8");
        assert_eq!(value.search_program().criteria(), b"NOT DELETED");
    }

    #[test]
    fn rejects_malformed_arguments_and_prefixed_search_programs() {
        for input in [
            b"() UTF-8 ALL".as_slice(),
            b"(REVERSE) UTF-8 ALL".as_slice(),
            b"(REVERSE REVERSE) UTF-8 ALL".as_slice(),
            b"(DATE ) UTF-8 ALL".as_slice(),
            b"(DATE) {5}\r\nUTF-8 ALL".as_slice(),
            b"(DATE) UTF-8".as_slice(),
            b"(DATE) UTF-8 CHARSET US-ASCII ALL".as_slice(),
            b"(DATE) UTF-8 RETURN (COUNT) ALL".as_slice(),
        ] {
            assert!(arguments(input).is_err(), "{input:?}");
        }
    }

    #[test]
    fn parses_sort_results_and_modseq_suffix() {
        let value = response(b"SORT 2 3 6 (MODSEQ 917162500)").unwrap();
        assert_eq!(value.results().collect::<Vec<_>>(), [2, 3, 6]);
        assert_eq!(value.result_count(), 3);
        assert_eq!(value.mod_sequence(), Some(917_162_500));
        assert!(response(b"SORT").unwrap().results().next().is_none());
    }

    #[test]
    fn enforces_sort_response_boundaries() {
        assert!(response(b"SORT 4294967295 (MODSEQ 9223372036854775807)").is_ok());
        for input in [
            b"SORT ".as_slice(),
            b"SORT 0".as_slice(),
            b"SORT 01".as_slice(),
            b"SORT 4294967296".as_slice(),
            b"SORT (MODSEQ 1)".as_slice(),
            b"SORT 1 (MODSEQ 0)".as_slice(),
            b"SORT 1 (MODSEQ 9223372036854775808)".as_slice(),
            b"SORT 1  (MODSEQ 2)".as_slice(),
        ] {
            assert!(response(input).is_err(), "{input:?}");
        }
    }

    #[test]
    fn encoding_is_exact() {
        let value = arguments(b"(ARRIVAL) \"UTF-8\" ALL").unwrap();
        let mut encoded = BytesMut::new();
        value.encode(&mut encoded);
        assert_eq!(&encoded[..], value.as_bytes().as_ref());

        let value = response(b"SORT 1 9").unwrap();
        encoded.clear();
        value.encode(&mut encoded);
        assert_eq!(&encoded[..], value.as_bytes().as_ref());
    }
}
