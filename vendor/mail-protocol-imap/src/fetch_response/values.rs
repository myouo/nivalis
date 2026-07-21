use std::borrow::Cow;
use std::ops::Range;

use mail_protocol_core::ProtocolError;

use crate::astring::{AStringKind, parse_astring_prefix};

use super::{MAX_NUMBER, invalid, required_space, shift_error, validate_atom};

/// Wire representation of an IMAP response string.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum FetchStringKind {
    /// A double-quoted UTF-8 string.
    Quoted,
    /// A normal length-prefixed string.
    Literal,
}

/// Borrowed, validated IMAP `string` value.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct FetchString<'a> {
    wire: &'a [u8],
    content: &'a [u8],
    kind: FetchStringKind,
}

impl<'a> FetchString<'a> {
    /// Returns the exact wire value, including quotes or literal marker.
    pub const fn as_bytes(self) -> &'a [u8] {
        self.wire
    }

    /// Returns the encoded quoted content or literal payload.
    pub const fn encoded_content(self) -> &'a [u8] {
        self.content
    }

    /// Returns the selected string representation.
    pub const fn kind(self) -> FetchStringKind {
        self.kind
    }

    /// Returns the logical value, allocating only for quoted escapes.
    pub fn decoded(self) -> Cow<'a, [u8]> {
        if self.kind != FetchStringKind::Quoted || !self.content.contains(&b'\\') {
            return Cow::Borrowed(self.content);
        }
        let mut decoded = Vec::with_capacity(self.content.len());
        let mut cursor = 0usize;
        while cursor < self.content.len() {
            if self.content[cursor] == b'\\' {
                cursor += 1;
            }
            decoded.push(self.content[cursor]);
            cursor += 1;
        }
        Cow::Owned(decoded)
    }
}

/// Borrowed `nstring` value.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum FetchNString<'a> {
    /// The NIL sentinel.
    Nil,
    /// A quoted or literal string.
    String(FetchString<'a>),
}

impl<'a> FetchNString<'a> {
    /// Returns the exact wire value.
    pub const fn as_bytes(self) -> &'a [u8] {
        match self {
            Self::Nil => b"NIL",
            Self::String(value) => value.as_bytes(),
        }
    }

    /// Returns the logical string, or `None` for NIL.
    pub fn decoded(self) -> Option<Cow<'a, [u8]>> {
        match self {
            Self::Nil => None,
            Self::String(value) => Some(value.decoded()),
        }
    }
}

/// Data carried by a BINARY response item.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum FetchBinaryData<'a> {
    /// NIL section data.
    Nil,
    /// A normal quoted or literal string.
    String(FetchString<'a>),
    /// A literal8 payload that may contain NUL octets.
    Literal8 {
        /// Complete literal8 wire representation.
        wire: &'a [u8],
        /// Counted binary payload.
        data: &'a [u8],
    },
}

/// A validated FETCH flag list.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct FetchFlags<'a> {
    pub(super) wire: &'a [u8],
}

impl<'a> FetchFlags<'a> {
    /// Returns the complete parenthesized flag list.
    pub const fn as_bytes(self) -> &'a [u8] {
        self.wire
    }

    /// Iterates over typed flags without allocating.
    pub fn iter(self) -> FetchFlagIter<'a> {
        FetchFlagIter {
            remaining: &self.wire[1..self.wire.len() - 1],
        }
    }
}

/// One message flag returned by FETCH.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum FetchFlag<'a> {
    Answered,
    Flagged,
    Deleted,
    Seen,
    Draft,
    Recent,
    Keyword(&'a [u8]),
    Extension(&'a [u8]),
}

/// Allocation-free iterator over FETCH flags.
#[derive(Clone, Debug)]
pub struct FetchFlagIter<'a> {
    remaining: &'a [u8],
}

impl<'a> Iterator for FetchFlagIter<'a> {
    type Item = FetchFlag<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining.is_empty() {
            return None;
        }
        let end = self
            .remaining
            .iter()
            .position(|byte| *byte == b' ')
            .unwrap_or(self.remaining.len());
        let flag = classify_flag(&self.remaining[..end]);
        self.remaining = if end == self.remaining.len() {
            b""
        } else {
            &self.remaining[end + 1..]
        };
        Some(flag)
    }
}

/// A validated RFC 9051 envelope.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct FetchEnvelope<'a> {
    pub(super) wire: &'a [u8],
}

impl<'a> FetchEnvelope<'a> {
    pub const fn as_bytes(&self) -> &'a [u8] {
        self.wire
    }

    pub fn date(&self) -> FetchNString<'a> {
        self.nstring(0)
    }

    pub fn subject(&self) -> FetchNString<'a> {
        self.nstring(1)
    }

    pub fn from(&self) -> FetchAddressList<'a> {
        self.addresses(2)
    }

    pub fn sender(&self) -> FetchAddressList<'a> {
        self.addresses(3)
    }

    pub fn reply_to(&self) -> FetchAddressList<'a> {
        self.addresses(4)
    }

    pub fn to(&self) -> FetchAddressList<'a> {
        self.addresses(5)
    }

    pub fn cc(&self) -> FetchAddressList<'a> {
        self.addresses(6)
    }

    pub fn bcc(&self) -> FetchAddressList<'a> {
        self.addresses(7)
    }

    pub fn in_reply_to(&self) -> FetchNString<'a> {
        self.nstring(8)
    }

    pub fn message_id(&self) -> FetchNString<'a> {
        self.nstring(9)
    }

    fn nstring(&self, index: usize) -> FetchNString<'a> {
        let field = self.field(index);
        parse_nstring(field, 0)
            .expect("FetchEnvelope stores validated nstrings")
            .value
    }

    fn addresses(&self, index: usize) -> FetchAddressList<'a> {
        let value = self.field(index);
        if value.eq_ignore_ascii_case(b"NIL") {
            FetchAddressList::Nil
        } else {
            FetchAddressList::Addresses(&value[1..value.len() - 1])
        }
    }

    fn field(&self, target: usize) -> &'a [u8] {
        let mut cursor = 1usize;
        for index in 0..=target {
            let start = cursor;
            cursor = if matches!(index, 2..=7) {
                parse_address_list(self.wire, cursor)
                    .expect("FetchEnvelope stores validated address fields")
            } else {
                parse_nstring(self.wire, cursor)
                    .expect("FetchEnvelope stores validated string fields")
                    .end
            };
            if index == target {
                return &self.wire[start..cursor];
            }
            cursor = required_space(self.wire, cursor, "validated envelope separator")
                .expect("FetchEnvelope stores validated separators");
        }
        unreachable!("envelope field target is bounded by accessors")
    }
}

/// NIL or a non-empty sequence of envelope addresses.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum FetchAddressList<'a> {
    Nil,
    Addresses(&'a [u8]),
}

impl<'a> FetchAddressList<'a> {
    pub const fn is_nil(self) -> bool {
        matches!(self, Self::Nil)
    }

    pub const fn iter(self) -> FetchAddressIter<'a> {
        FetchAddressIter {
            remaining: match self {
                Self::Nil => b"",
                Self::Addresses(value) => value,
            },
        }
    }
}

/// One four-field IMAP envelope address.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct FetchAddress<'a> {
    pub name: FetchNString<'a>,
    pub route: FetchNString<'a>,
    pub mailbox: FetchNString<'a>,
    pub host: FetchNString<'a>,
}

/// Allocation-free iterator over an envelope address list.
#[derive(Clone, Debug)]
pub struct FetchAddressIter<'a> {
    remaining: &'a [u8],
}

impl<'a> Iterator for FetchAddressIter<'a> {
    type Item = FetchAddress<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining.is_empty() {
            return None;
        }
        let parsed =
            parse_address(self.remaining, 0).expect("FetchAddressList stores validated addresses");
        self.remaining = &self.remaining[parsed.end..];
        Some(parsed.address)
    }
}

pub(super) struct ParsedNString<'a> {
    pub(super) value: FetchNString<'a>,
    pub(super) end: usize,
}

pub(super) struct ParsedString<'a> {
    pub(super) value: FetchString<'a>,
    pub(super) end: usize,
}

pub(super) struct ParsedEnvelope<'a> {
    pub(super) value: FetchEnvelope<'a>,
    pub(super) end: usize,
}

struct ParsedAddress<'a> {
    address: FetchAddress<'a>,
    end: usize,
}

pub(super) fn parse_string(input: &[u8], start: usize) -> Result<ParsedString<'_>, ProtocolError> {
    let parsed =
        parse_astring_prefix(&input[start..]).map_err(|error| shift_error(error, start))?;
    let kind = match parsed.kind {
        AStringKind::Quoted => FetchStringKind::Quoted,
        AStringKind::Literal {
            non_synchronizing: false,
        } => FetchStringKind::Literal,
        AStringKind::Literal {
            non_synchronizing: true,
        } => return Err(invalid("non-synchronizing IMAP server literal").at(start)),
        AStringKind::Atom => return Err(invalid("IMAP response string").at(start)),
    };
    let end = start + parsed.end;
    Ok(ParsedString {
        value: FetchString {
            wire: &input[start..end],
            content: &input[start + parsed.content.start..start + parsed.content.end],
            kind,
        },
        end,
    })
}

pub(super) fn parse_nstring(
    input: &[u8],
    start: usize,
) -> Result<ParsedNString<'_>, ProtocolError> {
    if input
        .get(start..start + 3)
        .is_some_and(|value| value.eq_ignore_ascii_case(b"NIL"))
    {
        return Ok(ParsedNString {
            value: FetchNString::Nil,
            end: start + 3,
        });
    }
    let parsed = parse_string(input, start)?;
    Ok(ParsedNString {
        value: FetchNString::String(parsed.value),
        end: parsed.end,
    })
}

pub(super) fn parse_binary_data(
    input: &[u8],
    start: usize,
) -> Result<(FetchBinaryData<'_>, usize), ProtocolError> {
    if input.get(start) == Some(&b'~') {
        let parsed = parse_literal8(input, start)?;
        return Ok((
            FetchBinaryData::Literal8 {
                wire: &input[start..parsed.end],
                data: &input[parsed.content],
            },
            parsed.end,
        ));
    }
    let parsed = parse_nstring(input, start)?;
    let value = match parsed.value {
        FetchNString::Nil => FetchBinaryData::Nil,
        FetchNString::String(value) => FetchBinaryData::String(value),
    };
    Ok((value, parsed.end))
}

pub(super) struct ParsedLiteral8 {
    pub(super) end: usize,
    content: Range<usize>,
}

pub(super) fn parse_literal8(input: &[u8], start: usize) -> Result<ParsedLiteral8, ProtocolError> {
    if input.get(start..start + 2) != Some(b"~{") {
        return Err(invalid("IMAP literal8 marker").at(start));
    }
    let mut cursor = start + 2;
    let digits_start = cursor;
    let mut length = 0u64;
    while let Some(digit) = input.get(cursor).filter(|byte| byte.is_ascii_digit()) {
        length = length
            .checked_mul(10)
            .and_then(|value| value.checked_add(u64::from(*digit - b'0')))
            .ok_or_else(|| invalid("IMAP literal8 length").at(digits_start))?;
        cursor += 1;
    }
    if cursor == digits_start || length > MAX_NUMBER {
        return Err(invalid("IMAP literal8 length").at(digits_start));
    }
    if input.get(cursor) != Some(&b'}') || input.get(cursor + 1..cursor + 3) != Some(b"\r\n") {
        return Err(invalid("IMAP literal8 marker").at(cursor));
    }
    let content_start = cursor + 3;
    let payload_len =
        usize::try_from(length).map_err(|_| invalid("IMAP literal8 length").at(digits_start))?;
    let end = content_start
        .checked_add(payload_len)
        .ok_or_else(|| invalid("IMAP literal8 length").at(digits_start))?;
    if end > input.len() {
        return Err(invalid("truncated IMAP literal8").at(content_start));
    }
    Ok(ParsedLiteral8 {
        end,
        content: content_start..end,
    })
}

pub(super) fn parse_flag_list(input: &[u8], start: usize) -> Result<usize, ProtocolError> {
    if input.get(start) != Some(&b'(') {
        return Err(invalid("IMAP FETCH flag list opener").at(start));
    }
    let mut cursor = start + 1;
    if input.get(cursor) == Some(&b')') {
        return Ok(cursor + 1);
    }
    loop {
        let end = input[cursor..]
            .iter()
            .position(|byte| matches!(byte, b' ' | b')'))
            .map_or(input.len(), |offset| cursor + offset);
        validate_flag(&input[cursor..end]).map_err(|error| shift_error(error, cursor))?;
        cursor = end;
        match input.get(cursor) {
            Some(b')') => return Ok(cursor + 1),
            Some(b' ')
                if input
                    .get(cursor + 1)
                    .is_some_and(|byte| !matches!(byte, b' ' | b')')) =>
            {
                cursor += 1;
            }
            None => return Err(invalid("unterminated IMAP FETCH flag list").at(cursor)),
            _ => return Err(invalid("IMAP FETCH flag separator").at(cursor)),
        }
    }
}

fn validate_flag(value: &[u8]) -> Result<(), ProtocolError> {
    if value.is_empty() {
        return Err(invalid("empty IMAP FETCH flag"));
    }
    if let Some(atom) = value.strip_prefix(b"\\") {
        validate_atom(atom, "IMAP FETCH system or extension flag")
    } else {
        validate_atom(value, "IMAP FETCH keyword flag")
    }
}

fn classify_flag(value: &[u8]) -> FetchFlag<'_> {
    if value.eq_ignore_ascii_case(b"\\Answered") {
        FetchFlag::Answered
    } else if value.eq_ignore_ascii_case(b"\\Flagged") {
        FetchFlag::Flagged
    } else if value.eq_ignore_ascii_case(b"\\Deleted") {
        FetchFlag::Deleted
    } else if value.eq_ignore_ascii_case(b"\\Seen") {
        FetchFlag::Seen
    } else if value.eq_ignore_ascii_case(b"\\Draft") {
        FetchFlag::Draft
    } else if value.eq_ignore_ascii_case(b"\\Recent") {
        FetchFlag::Recent
    } else if value.starts_with(b"\\") {
        FetchFlag::Extension(value)
    } else {
        FetchFlag::Keyword(value)
    }
}

pub(super) fn parse_envelope(
    input: &[u8],
    start: usize,
) -> Result<ParsedEnvelope<'_>, ProtocolError> {
    if input.get(start) != Some(&b'(') {
        return Err(invalid("IMAP envelope opener").at(start));
    }
    let mut cursor = start + 1;
    for index in 0..10 {
        cursor = if matches!(index, 2..=7) {
            parse_address_list(input, cursor)?
        } else {
            parse_nstring(input, cursor)?.end
        };
        if index != 9 {
            cursor = required_space(input, cursor, "IMAP envelope field separator")?;
        }
    }
    if input.get(cursor) != Some(&b')') {
        return Err(invalid("IMAP envelope terminator").at(cursor));
    }
    let end = cursor + 1;
    Ok(ParsedEnvelope {
        value: FetchEnvelope {
            wire: &input[start..end],
        },
        end,
    })
}

fn parse_address_list(input: &[u8], start: usize) -> Result<usize, ProtocolError> {
    if input
        .get(start..start + 3)
        .is_some_and(|value| value.eq_ignore_ascii_case(b"NIL"))
    {
        return Ok(start + 3);
    }
    if input.get(start) != Some(&b'(') || input.get(start + 1) != Some(&b'(') {
        return Err(invalid("IMAP envelope address list").at(start));
    }
    let mut cursor = start + 1;
    loop {
        cursor = parse_address(input, cursor)?.end;
        match input.get(cursor) {
            Some(b'(') => {}
            Some(b')') => return Ok(cursor + 1),
            None => return Err(invalid("unterminated IMAP address list").at(cursor)),
            _ => return Err(invalid("IMAP envelope address separator").at(cursor)),
        }
    }
}

fn parse_address(input: &[u8], start: usize) -> Result<ParsedAddress<'_>, ProtocolError> {
    if input.get(start) != Some(&b'(') {
        return Err(invalid("IMAP envelope address opener").at(start));
    }
    let mut cursor = start + 1;
    let name = parse_nstring(input, cursor)?;
    cursor = required_space(input, name.end, "IMAP address name separator")?;
    let route = parse_nstring(input, cursor)?;
    cursor = required_space(input, route.end, "IMAP address route separator")?;
    let mailbox = parse_nstring(input, cursor)?;
    cursor = required_space(input, mailbox.end, "IMAP address mailbox separator")?;
    let host = parse_nstring(input, cursor)?;
    if input.get(host.end) != Some(&b')') {
        return Err(invalid("IMAP envelope address terminator").at(host.end));
    }
    Ok(ParsedAddress {
        address: FetchAddress {
            name: name.value,
            route: route.value,
            mailbox: mailbox.value,
            host: host.value,
        },
        end: host.end + 1,
    })
}
