use mail_protocol_core::wire::eq_ascii;
use mail_protocol_core::{ErrorKind, ProtocolError};

use crate::astring::parse_astring_prefix;

const MAX_NUMBER: u64 = u32::MAX as u64;
pub(super) const MAX_NUMBER64: u64 = i64::MAX as u64;

/// A validated BODY or BINARY section, including its square brackets.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct FetchSection<'a> {
    wire: &'a [u8],
    parts: &'a [u8],
    text: Option<FetchSectionText<'a>>,
}

impl<'a> FetchSection<'a> {
    /// Returns the exact bracketed section.
    pub const fn as_bytes(self) -> &'a [u8] {
        self.wire
    }

    /// Iterates over the validated non-zero 32-bit numeric path.
    pub const fn parts(self) -> FetchSectionPartIter<'a> {
        FetchSectionPartIter {
            remaining: self.parts,
        }
    }

    /// Returns the terminal message/MIME text selector, if any.
    pub const fn text(self) -> Option<FetchSectionText<'a>> {
        self.text
    }

    /// Returns whether this section selects the whole message.
    pub const fn is_entire_message(self) -> bool {
        self.parts.is_empty() && self.text.is_none()
    }
}

/// Terminal selector in a BODY section.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum FetchSectionText<'a> {
    /// RFC 5322 header.
    Header,
    /// Selected or excluded RFC 5322 header fields.
    HeaderFields(FetchHeaderFields<'a>),
    /// RFC 5322 body text without the header.
    Text,
    /// MIME header for a numeric body part.
    Mime,
}

/// A validated non-empty HEADER.FIELDS or HEADER.FIELDS.NOT list.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct FetchHeaderFields<'a> {
    wire: &'a [u8],
    excluded: bool,
}

impl<'a> FetchHeaderFields<'a> {
    /// Returns whether the listed fields are excluded rather than selected.
    pub const fn is_not(self) -> bool {
        self.excluded
    }

    /// Returns the exact parenthesized field-name list.
    pub const fn as_bytes(self) -> &'a [u8] {
        self.wire
    }

    /// Iterates over exact validated astring wire values.
    pub fn iter(self) -> FetchHeaderFieldIter<'a> {
        FetchHeaderFieldIter {
            remaining: &self.wire[1..self.wire.len() - 1],
        }
    }
}

/// Allocation-free iterator over HEADER.FIELDS astring values.
#[derive(Clone, Debug)]
pub struct FetchHeaderFieldIter<'a> {
    remaining: &'a [u8],
}

impl<'a> Iterator for FetchHeaderFieldIter<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining.is_empty() {
            return None;
        }
        let end = match parse_header_field(self.remaining) {
            Ok(end) => end,
            Err(error) => {
                debug_assert!(
                    false,
                    "validated FETCH header field became invalid: {error}"
                );
                self.remaining = b"";
                return None;
            }
        };
        let value = &self.remaining[..end];
        self.remaining = if end == self.remaining.len() {
            b""
        } else {
            self.remaining.get(end + 1..).unwrap_or_else(|| {
                debug_assert!(false, "validated FETCH header field separator disappeared");
                b""
            })
        };
        Some(value)
    }
}

/// Allocation-free iterator over numeric FETCH section parts.
#[derive(Clone, Debug)]
pub struct FetchSectionPartIter<'a> {
    remaining: &'a [u8],
}

impl Iterator for FetchSectionPartIter<'_> {
    type Item = u32;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining.is_empty() {
            return None;
        }
        let end = self
            .remaining
            .iter()
            .position(|byte| *byte == b'.')
            .unwrap_or(self.remaining.len());
        let part = self.remaining[..end]
            .iter()
            .fold(0u32, |value, digit| value * 10 + u32::from(*digit - b'0'));
        self.remaining = if end == self.remaining.len() {
            b""
        } else {
            &self.remaining[end + 1..]
        };
        Some(part)
    }
}

/// A 63-bit FETCH byte range.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct FetchPartial {
    /// Zero-based first requested octet.
    pub offset: u64,
    /// Non-zero maximum number of octets requested.
    pub length: u64,
}

pub(crate) struct ParsedSection<'a> {
    pub(crate) section: FetchSection<'a>,
    pub(crate) end: usize,
}

pub(crate) fn parse_section(
    input: &[u8],
    start: usize,
    binary: bool,
) -> Result<ParsedSection<'_>, ProtocolError> {
    if input.get(start) != Some(&b'[') {
        return Err(invalid("IMAP FETCH section opener").at(start));
    }
    let content_start = start + 1;
    let mut cursor = content_start;
    if input.get(cursor) == Some(&b']') {
        let end = cursor + 1;
        return Ok(ParsedSection {
            section: FetchSection {
                wire: &input[start..end],
                parts: b"",
                text: None,
            },
            end,
        });
    }

    let parts_start = cursor;
    let mut parts_end = cursor;
    let mut text_separator = false;
    if input.get(cursor).is_some_and(u8::is_ascii_digit) {
        loop {
            cursor = parse_nz_number_end(input, cursor, MAX_NUMBER, "IMAP FETCH section part")?;
            parts_end = cursor;
            if input.get(cursor) != Some(&b'.') {
                break;
            }
            if input.get(cursor + 1).is_some_and(u8::is_ascii_digit) {
                cursor += 1;
                continue;
            }
            if binary {
                return Err(invalid("IMAP BINARY section part").at(cursor));
            }
            cursor += 1;
            text_separator = true;
            break;
        }
    } else if binary {
        return Err(invalid("IMAP BINARY section part").at(cursor));
    }

    let parts = &input[parts_start..parts_end];
    if input.get(cursor) == Some(&b']') {
        let end = cursor + 1;
        return Ok(ParsedSection {
            section: FetchSection {
                wire: &input[start..end],
                parts,
                text: None,
            },
            end,
        });
    }
    if binary {
        return Err(invalid("IMAP BINARY section terminator").at(cursor));
    }
    if !parts.is_empty() && !text_separator {
        return Err(invalid("IMAP FETCH section separator").at(cursor));
    }

    let (text, text_end) = parse_section_text(input, cursor, !parts.is_empty())?;
    if input.get(text_end) != Some(&b']') {
        return Err(invalid("IMAP FETCH section terminator").at(text_end));
    }
    let end = text_end + 1;
    Ok(ParsedSection {
        section: FetchSection {
            wire: &input[start..end],
            parts,
            text: Some(text),
        },
        end,
    })
}

fn parse_section_text(
    input: &[u8],
    start: usize,
    has_parts: bool,
) -> Result<(FetchSectionText<'_>, usize), ProtocolError> {
    if starts_ascii(&input[start..], b"HEADER.FIELDS.NOT ") {
        let list_start = start + b"HEADER.FIELDS.NOT ".len();
        let end = parse_header_fields(input, list_start)?;
        return Ok((
            FetchSectionText::HeaderFields(FetchHeaderFields {
                wire: &input[list_start..end],
                excluded: true,
            }),
            end,
        ));
    }
    if starts_ascii(&input[start..], b"HEADER.FIELDS ") {
        let list_start = start + b"HEADER.FIELDS ".len();
        let end = parse_header_fields(input, list_start)?;
        return Ok((
            FetchSectionText::HeaderFields(FetchHeaderFields {
                wire: &input[list_start..end],
                excluded: false,
            }),
            end,
        ));
    }
    if token_before_bracket(input, start, b"HEADER") {
        return Ok((FetchSectionText::Header, start + b"HEADER".len()));
    }
    if token_before_bracket(input, start, b"TEXT") {
        return Ok((FetchSectionText::Text, start + b"TEXT".len()));
    }
    if has_parts && token_before_bracket(input, start, b"MIME") {
        return Ok((FetchSectionText::Mime, start + b"MIME".len()));
    }
    Err(invalid("IMAP FETCH section text").at(start))
}

fn parse_header_fields(input: &[u8], start: usize) -> Result<usize, ProtocolError> {
    if input.get(start) != Some(&b'(')
        || input
            .get(start + 1)
            .is_none_or(|byte| matches!(byte, b' ' | b')'))
    {
        return Err(invalid("IMAP FETCH header field list").at(start));
    }
    let mut cursor = start + 1;
    loop {
        let end =
            parse_header_field(&input[cursor..]).map_err(|error| shift_error(error, cursor))?;
        cursor += end;
        match input.get(cursor) {
            Some(b')') => return Ok(cursor + 1),
            Some(b' ')
                if input
                    .get(cursor + 1)
                    .is_some_and(|byte| !matches!(byte, b' ' | b')')) =>
            {
                cursor += 1;
            }
            None => return Err(invalid("unterminated IMAP FETCH header field list").at(cursor)),
            _ => return Err(invalid("IMAP FETCH header field separator").at(cursor)),
        }
    }
}

fn parse_header_field(input: &[u8]) -> Result<usize, ProtocolError> {
    if matches!(input.first(), Some(b'"' | b'{')) {
        return parse_astring_prefix(input).map(|parsed| parsed.end);
    }
    let mut end = 0usize;
    while let Some(byte) = input.get(end) {
        if matches!(byte, b' ' | b')') {
            break;
        }
        if !byte.is_ascii()
            || byte.is_ascii_control()
            || matches!(byte, b'(' | b'{' | b'%' | b'*' | b'"' | b'\\')
        {
            return Err(invalid("IMAP FETCH header field atom").at(end));
        }
        end += 1;
    }
    if end == 0 {
        Err(invalid("empty IMAP FETCH header field atom"))
    } else {
        Ok(end)
    }
}

pub(super) fn parse_optional_partial(
    input: &[u8],
    start: usize,
) -> Result<(Option<FetchPartial>, usize), ProtocolError> {
    if input.get(start) != Some(&b'<') {
        return Ok((None, start));
    }
    let dot = input[start + 1..]
        .iter()
        .position(|byte| *byte == b'.')
        .map(|offset| start + 1 + offset)
        .ok_or_else(|| invalid("IMAP FETCH partial separator").at(start))?;
    let close = input[dot + 1..]
        .iter()
        .position(|byte| *byte == b'>')
        .map(|offset| dot + 1 + offset)
        .ok_or_else(|| invalid("IMAP FETCH partial terminator").at(dot))?;
    let offset = parse_number(&input[start + 1..dot], MAX_NUMBER64, false, false)?;
    let length = parse_number(&input[dot + 1..close], MAX_NUMBER64, true, true)?;
    Ok((Some(FetchPartial { offset, length }), close + 1))
}

fn parse_nz_number_end(
    input: &[u8],
    start: usize,
    maximum: u64,
    context: &'static str,
) -> Result<usize, ProtocolError> {
    let mut cursor = start;
    let mut value = 0u64;
    while let Some(digit) = input.get(cursor).filter(|byte| byte.is_ascii_digit()) {
        if cursor == start && *digit == b'0' {
            return Err(ProtocolError::new(ErrorKind::InvalidSyntax, context).at(start));
        }
        value = value
            .checked_mul(10)
            .and_then(|value| value.checked_add(u64::from(*digit - b'0')))
            .filter(|value| *value <= maximum)
            .ok_or_else(|| ProtocolError::new(ErrorKind::InvalidSyntax, context).at(start))?;
        cursor += 1;
    }
    if cursor == start || value == 0 {
        Err(ProtocolError::new(ErrorKind::InvalidSyntax, context).at(start))
    } else {
        Ok(cursor)
    }
}

fn parse_number(
    input: &[u8],
    maximum: u64,
    non_zero: bool,
    forbid_leading_zero: bool,
) -> Result<u64, ProtocolError> {
    if input.is_empty() || forbid_leading_zero && input.first() == Some(&b'0') {
        return Err(invalid("IMAP FETCH number"));
    }
    let value = input.iter().try_fold(0u64, |value, digit| {
        digit.is_ascii_digit().then_some(value).and_then(|value| {
            value
                .checked_mul(10)
                .and_then(|value| value.checked_add(u64::from(*digit - b'0')))
                .filter(|value| *value <= maximum)
        })
    });
    match value {
        Some(value) if value <= maximum && (!non_zero || value != 0) => Ok(value),
        _ => Err(invalid("IMAP FETCH number")),
    }
}

fn starts_ascii(input: &[u8], expected: &[u8]) -> bool {
    input
        .get(..expected.len())
        .is_some_and(|prefix| eq_ascii(prefix, expected))
}

fn token_before_bracket(input: &[u8], start: usize, expected: &[u8]) -> bool {
    input
        .get(start..start + expected.len())
        .is_some_and(|value| eq_ascii(value, expected))
        && input.get(start + expected.len()) == Some(&b']')
}

fn shift_error(error: ProtocolError, start: usize) -> ProtocolError {
    error.at(start.saturating_add(error.offset().unwrap_or(0)))
}

const fn invalid(context: &'static str) -> ProtocolError {
    ProtocolError::new(ErrorKind::InvalidSyntax, context)
}
