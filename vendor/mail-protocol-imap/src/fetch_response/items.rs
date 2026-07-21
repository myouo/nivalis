use bytes::Bytes;
use mail_protocol_core::ProtocolError;

use crate::append::{DATE_TIME_LEN, validate_date_time};
use crate::fetch::{FetchSection, parse_section};

use super::body::{BodyStructure, parse_body};
use super::values::{
    FetchBinaryData, FetchEnvelope, FetchFlags, FetchNString, parse_binary_data, parse_envelope,
    parse_flag_list, parse_literal8, parse_nstring, parse_string,
};
use super::{
    DEFAULT_FETCH_RESPONSE_MAX_DEPTH, MAX_NUMBER, MAX_NUMBER64, invalid, nesting_too_deep,
    parse_number_token, required_space, shift_error, starts_ascii, validate_atom,
};

/// A validated RFC 9051 FETCH response attribute list.
///
/// The value owns the original [`Bytes`] view. Items and nested structures are
/// exposed by borrowing that allocation, so literal message data is never
/// copied by the semantic parser.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct FetchResponse {
    wire: Bytes,
    nesting_depth: usize,
}

impl FetchResponse {
    /// Parses one complete `msg-att` parenthesized list.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed attributes, strings, literals, numeric
    /// bounds, sections, envelopes, body structures, spacing, or nesting.
    pub fn parse(wire: &Bytes) -> Result<Self, ProtocolError> {
        Self::parse_with_max_depth(wire, DEFAULT_FETCH_RESPONSE_MAX_DEPTH)
    }

    /// Parses with an explicit BODYSTRUCTURE and extension nesting budget.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::parse`] and `NestingTooDeep` when
    /// the supplied budget is exceeded.
    pub fn parse_with_max_depth(wire: &Bytes, max_depth: usize) -> Result<Self, ProtocolError> {
        let nesting_depth = validate_items(wire, max_depth)?;
        Ok(Self {
            wire: wire.clone(),
            nesting_depth,
        })
    }

    /// Returns the exact validated parenthesized response data.
    pub const fn as_bytes(&self) -> &Bytes {
        &self.wire
    }

    /// Returns the greatest BODYSTRUCTURE or extension nesting observed.
    pub const fn nesting_depth(&self) -> usize {
        self.nesting_depth
    }

    /// Iterates over typed response attributes without allocating.
    pub fn items(&self) -> FetchResponseItemIter<'_> {
        FetchResponseItemIter {
            remaining: &self.wire[1..self.wire.len() - 1],
            max_depth: self.nesting_depth,
        }
    }
}

/// One typed FETCH response attribute.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum FetchResponseItem<'a> {
    /// Current message flags.
    Flags(FetchFlags<'a>),
    /// Parsed RFC 5322 envelope fields.
    Envelope(FetchEnvelope<'a>),
    /// Fixed-format IMAP internal date string, including quotes.
    InternalDate(&'a [u8]),
    /// Complete RFC 5322 message, as named by `IMAP4rev1`.
    Rfc822(FetchNString<'a>),
    /// RFC 5322 header, as named by `IMAP4rev1`.
    Rfc822Header(FetchNString<'a>),
    /// RFC 5322 body text, as named by `IMAP4rev1`.
    Rfc822Text(FetchNString<'a>),
    /// RFC 5322 message size in octets.
    Rfc822Size(u64),
    /// Non-extensible or extensible MIME body structure.
    Body(BodyStructure<'a>),
    /// Message or MIME section contents.
    BodySection {
        section: FetchSection<'a>,
        origin: Option<u32>,
        data: FetchNString<'a>,
    },
    /// Transfer-decoded section contents.
    Binary {
        section: FetchSection<'a>,
        origin: Option<u32>,
        data: FetchBinaryData<'a>,
    },
    /// Transfer-decoded section size.
    BinarySize {
        section: FetchSection<'a>,
        size: u32,
    },
    /// Non-zero 32-bit message unique identifier.
    Uid(u32),
    /// RFC 7162 positive per-message modification sequence.
    ModSeq(u64),
    /// Extension item preserved as one bounded value.
    Other { name: &'a [u8], value: &'a [u8] },
}

/// Allocation-free iterator over validated FETCH response items.
#[derive(Clone, Debug)]
pub struct FetchResponseItemIter<'a> {
    remaining: &'a [u8],
    max_depth: usize,
}

impl<'a> Iterator for FetchResponseItemIter<'a> {
    type Item = FetchResponseItem<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining.is_empty() {
            return None;
        }
        let parsed = match parse_item(self.remaining, self.max_depth) {
            Ok(parsed) => parsed,
            Err(error) => {
                debug_assert!(
                    false,
                    "validated FETCH response item became invalid: {error}"
                );
                self.remaining = b"";
                return None;
            }
        };
        self.remaining = if parsed.end == self.remaining.len() {
            b""
        } else {
            self.remaining.get(parsed.end + 1..).unwrap_or_else(|| {
                debug_assert!(false, "validated FETCH response separator disappeared");
                b""
            })
        };
        Some(parsed.item)
    }
}

struct ParsedItem<'a> {
    item: FetchResponseItem<'a>,
    end: usize,
    nesting_depth: usize,
}

fn validate_items(input: &[u8], max_depth: usize) -> Result<usize, ProtocolError> {
    if input.first() != Some(&b'(') || input.get(1).is_none_or(|byte| matches!(byte, b' ' | b')')) {
        return Err(invalid("IMAP FETCH response attribute list"));
    }
    let mut cursor = 1usize;
    let mut greatest_depth = 0usize;
    loop {
        let parsed =
            parse_item(&input[cursor..], max_depth).map_err(|error| shift_error(error, cursor))?;
        cursor += parsed.end;
        greatest_depth = greatest_depth.max(parsed.nesting_depth);
        match input.get(cursor) {
            Some(b')') if cursor + 1 == input.len() => return Ok(greatest_depth),
            Some(b' ')
                if input
                    .get(cursor + 1)
                    .is_some_and(|byte| !matches!(byte, b' ' | b')')) =>
            {
                cursor += 1;
            }
            Some(b')') => {
                return Err(invalid("trailing IMAP FETCH response data").at(cursor + 1));
            }
            None => return Err(invalid("unterminated IMAP FETCH response list").at(cursor)),
            _ => return Err(invalid("IMAP FETCH response item separator").at(cursor)),
        }
    }
}

#[allow(clippy::too_many_lines)]
fn parse_item(input: &[u8], max_depth: usize) -> Result<ParsedItem<'_>, ProtocolError> {
    if starts_ascii(input, b"BODYSTRUCTURE ") {
        let value_start = b"BODYSTRUCTURE ".len();
        let body = parse_body(input, value_start, max_depth, true)?;
        return Ok(ParsedItem {
            item: FetchResponseItem::Body(body.value),
            end: body.end,
            nesting_depth: body.nesting_depth,
        });
    }
    if starts_ascii(input, b"BODY[") {
        let section = parse_section(input, b"BODY".len(), false)?;
        let (origin, cursor) = parse_optional_origin(input, section.end)?;
        let value_start = required_space(input, cursor, "IMAP BODY response separator")?;
        let data = parse_nstring(input, value_start)?;
        return Ok(ParsedItem {
            item: FetchResponseItem::BodySection {
                section: section.section,
                origin,
                data: data.value,
            },
            end: data.end,
            nesting_depth: 0,
        });
    }
    if starts_ascii(input, b"BINARY.SIZE[") {
        let section = parse_section(input, b"BINARY.SIZE".len(), true)?;
        let value_start = required_space(input, section.end, "IMAP BINARY.SIZE separator")?;
        let (size, end) = parse_number_token(input, value_start, MAX_NUMBER, false, false)?;
        return Ok(ParsedItem {
            item: FetchResponseItem::BinarySize {
                section: section.section,
                size: u32::try_from(size).expect("number parser enforces u32"),
            },
            end,
            nesting_depth: 0,
        });
    }
    if starts_ascii(input, b"BINARY[") {
        let section = parse_section(input, b"BINARY".len(), true)?;
        let (origin, cursor) = parse_optional_origin(input, section.end)?;
        let value_start = required_space(input, cursor, "IMAP BINARY response separator")?;
        let (data, end) = parse_binary_data(input, value_start)?;
        return Ok(ParsedItem {
            item: FetchResponseItem::Binary {
                section: section.section,
                origin,
                data,
            },
            end,
            nesting_depth: 0,
        });
    }

    let name_end = input
        .iter()
        .position(|byte| *byte == b' ')
        .ok_or_else(|| invalid("IMAP FETCH response item value"))?;
    let name = &input[..name_end];
    validate_atom(name, "IMAP FETCH response item name")?;
    let value_start = required_space(input, name_end, "IMAP FETCH response item separator")?;

    let (item, end, nesting_depth) = if name.eq_ignore_ascii_case(b"FLAGS") {
        let end = parse_flag_list(input, value_start)?;
        (
            FetchResponseItem::Flags(FetchFlags {
                wire: &input[value_start..end],
            }),
            end,
            0,
        )
    } else if name.eq_ignore_ascii_case(b"ENVELOPE") {
        let envelope = parse_envelope(input, value_start)?;
        (FetchResponseItem::Envelope(envelope.value), envelope.end, 0)
    } else if name.eq_ignore_ascii_case(b"INTERNALDATE") {
        let end = value_start
            .checked_add(DATE_TIME_LEN)
            .ok_or_else(|| invalid("IMAP INTERNALDATE length"))?;
        let value = input
            .get(value_start..end)
            .ok_or_else(|| invalid("truncated IMAP INTERNALDATE").at(value_start))?;
        validate_date_time(value).map_err(|error| shift_error(error, value_start))?;
        (FetchResponseItem::InternalDate(value), end, 0)
    } else if name.eq_ignore_ascii_case(b"RFC822") {
        let value = parse_nstring(input, value_start)?;
        (FetchResponseItem::Rfc822(value.value), value.end, 0)
    } else if name.eq_ignore_ascii_case(b"RFC822.HEADER") {
        let value = parse_nstring(input, value_start)?;
        (FetchResponseItem::Rfc822Header(value.value), value.end, 0)
    } else if name.eq_ignore_ascii_case(b"RFC822.TEXT") {
        let value = parse_nstring(input, value_start)?;
        (FetchResponseItem::Rfc822Text(value.value), value.end, 0)
    } else if name.eq_ignore_ascii_case(b"RFC822.SIZE") {
        let (size, end) = parse_number_token(input, value_start, MAX_NUMBER64, false, false)?;
        (FetchResponseItem::Rfc822Size(size), end, 0)
    } else if name.eq_ignore_ascii_case(b"BODY") {
        let body = parse_body(input, value_start, max_depth, false)?;
        (
            FetchResponseItem::Body(body.value),
            body.end,
            body.nesting_depth,
        )
    } else if name.eq_ignore_ascii_case(b"UID") {
        let (uid, end) = parse_number_token(input, value_start, MAX_NUMBER, true, true)?;
        (
            FetchResponseItem::Uid(u32::try_from(uid).expect("number parser enforces u32")),
            end,
            0,
        )
    } else if name.eq_ignore_ascii_case(b"MODSEQ") {
        if input.get(value_start) != Some(&b'(') {
            return Err(invalid("IMAP MODSEQ response opener").at(value_start));
        }
        let (value, cursor) =
            parse_number_token(input, value_start + 1, MAX_NUMBER64, true, false)?;
        if input.get(cursor) != Some(&b')') {
            return Err(invalid("IMAP MODSEQ response terminator").at(cursor));
        }
        (FetchResponseItem::ModSeq(value), cursor + 1, 0)
    } else {
        let opaque = parse_opaque_value(input, value_start, max_depth)?;
        (
            FetchResponseItem::Other {
                name,
                value: &input[value_start..opaque.end],
            },
            opaque.end,
            opaque.nesting_depth,
        )
    };
    Ok(ParsedItem {
        item,
        end,
        nesting_depth,
    })
}

fn parse_optional_origin(
    input: &[u8],
    start: usize,
) -> Result<(Option<u32>, usize), ProtocolError> {
    if input.get(start) != Some(&b'<') {
        return Ok((None, start));
    }
    let (origin, end) = parse_number_token(input, start + 1, MAX_NUMBER, false, false)?;
    if input.get(end) != Some(&b'>') {
        return Err(invalid("IMAP FETCH origin terminator").at(end));
    }
    Ok((
        Some(u32::try_from(origin).expect("number parser enforces u32")),
        end + 1,
    ))
}

struct ParsedOpaque {
    end: usize,
    nesting_depth: usize,
}

fn parse_opaque_value(
    input: &[u8],
    start: usize,
    max_depth: usize,
) -> Result<ParsedOpaque, ProtocolError> {
    if input.get(start) != Some(&b'(') {
        return Ok(ParsedOpaque {
            end: parse_opaque_scalar(input, start)?,
            nesting_depth: 0,
        });
    }
    let mut cursor = start;
    let mut depth = 0usize;
    let mut greatest_depth = 0usize;
    let mut expecting_value = true;
    let mut allow_empty = false;
    loop {
        let Some(byte) = input.get(cursor) else {
            return Err(invalid("unterminated IMAP FETCH extension value").at(cursor));
        };
        if expecting_value {
            match byte {
                b'(' => {
                    depth = depth
                        .checked_add(1)
                        .ok_or_else(|| nesting_too_deep(cursor))?;
                    if depth > max_depth {
                        return Err(nesting_too_deep(cursor));
                    }
                    greatest_depth = greatest_depth.max(depth);
                    cursor += 1;
                    allow_empty = true;
                }
                b')' if allow_empty => {
                    cursor += 1;
                    depth -= 1;
                    if depth == 0 {
                        return Ok(ParsedOpaque {
                            end: cursor,
                            nesting_depth: greatest_depth,
                        });
                    }
                    expecting_value = false;
                    allow_empty = false;
                }
                b' ' | b')' => {
                    return Err(invalid("IMAP FETCH extension value separator").at(cursor));
                }
                _ => {
                    cursor = parse_opaque_scalar(input, cursor)?;
                    expecting_value = false;
                    allow_empty = false;
                }
            }
        } else {
            match byte {
                b')' => {
                    cursor += 1;
                    depth -= 1;
                    if depth == 0 {
                        return Ok(ParsedOpaque {
                            end: cursor,
                            nesting_depth: greatest_depth,
                        });
                    }
                }
                b' ' => {
                    if input
                        .get(cursor + 1)
                        .is_none_or(|byte| matches!(byte, b' ' | b')'))
                    {
                        return Err(invalid("IMAP FETCH extension value separator").at(cursor));
                    }
                    cursor += 1;
                    expecting_value = true;
                }
                _ => return Err(invalid("IMAP FETCH extension value terminator").at(cursor)),
            }
        }
    }
}

fn parse_opaque_scalar(input: &[u8], start: usize) -> Result<usize, ProtocolError> {
    match input.get(start) {
        Some(b'"' | b'{') => parse_string(input, start).map(|parsed| parsed.end),
        Some(b'~') => parse_literal8(input, start).map(|parsed| parsed.end),
        None | Some(b' ' | b'(' | b')') => {
            Err(invalid("empty IMAP FETCH extension scalar").at(start))
        }
        Some(_) => {
            let end = input[start..]
                .iter()
                .position(|byte| matches!(byte, b' ' | b')'))
                .map_or(input.len(), |offset| start + offset);
            let value = &input[start..end];
            if value.iter().any(|byte| {
                !byte.is_ascii() || byte.is_ascii_control() || matches!(byte, b'(' | b'{' | b'"')
            }) {
                Err(invalid("IMAP FETCH extension scalar").at(start))
            } else {
                Ok(end)
            }
        }
    }
}
