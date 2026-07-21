use std::borrow::Cow;
use std::ops::Range;

use mail_protocol_core::{ErrorKind, ProtocolError};

#[inline]
pub(super) fn require_space(
    input: &[u8],
    cursor: usize,
    context: &'static str,
) -> Result<usize, ProtocolError> {
    if input.get(cursor) != Some(&b' ')
        || input.get(cursor + 1).is_none()
        || input.get(cursor + 1) == Some(&b' ')
    {
        return Err(invalid(context).at(cursor));
    }
    Ok(cursor + 1)
}

#[inline]
pub(super) fn parenthesized_end(input: &[u8]) -> Option<usize> {
    if input.first() != Some(&b'(') {
        return None;
    }
    input
        .iter()
        .position(|byte| *byte == b')')
        .map(|end| end + 1)
}

pub(super) fn next_list_item(input: &[u8], end: usize) -> Option<&[u8]> {
    if end == input.len() {
        Some(&[])
    } else {
        input.get(end + 1..)
    }
}

pub(super) fn list_interior<'a>(wire: &'a [u8], range: Option<&Range<usize>>) -> &'a [u8] {
    range.map_or(&[], |range| &wire[range.start + 1..range.end - 1])
}

pub(super) fn decode_string_content(content: &[u8], quoted: bool) -> Cow<'_, [u8]> {
    if !quoted || !content.contains(&b'\\') {
        return Cow::Borrowed(content);
    }
    let mut decoded = Vec::with_capacity(content.len());
    let mut cursor = 0;
    while cursor < content.len() {
        if content[cursor] == b'\\' {
            cursor += 1;
        }
        decoded.push(content[cursor]);
        cursor += 1;
    }
    Cow::Owned(decoded)
}

#[inline]
pub(super) fn validate_astring_atom(input: &[u8]) -> Result<(), ProtocolError> {
    if input.is_empty() || input.iter().any(|byte| !is_atom_char(*byte)) {
        return Err(invalid("IMAP LIST extension atom"));
    }
    Ok(())
}

#[inline]
pub(super) const fn is_atom_char(byte: u8) -> bool {
    byte.is_ascii()
        && !byte.is_ascii_control()
        && !matches!(
            byte,
            b' ' | b'(' | b')' | b'{' | b'%' | b'*' | b'"' | b'\\' | b']'
        )
}

#[inline]
pub(super) const fn is_list_char(byte: u8) -> bool {
    is_atom_char(byte) || matches!(byte, b'%' | b'*' | b']')
}

#[inline]
pub(super) fn shift_error(error: ProtocolError, start: usize) -> ProtocolError {
    error.at(start.saturating_add(error.offset().unwrap_or(0)))
}

#[inline]
pub(super) const fn invalid(context: &'static str) -> ProtocolError {
    ProtocolError::new(ErrorKind::InvalidSyntax, context)
}

#[inline]
pub(super) fn nesting_too_deep(offset: usize) -> ProtocolError {
    ProtocolError::new(ErrorKind::NestingTooDeep, "IMAP LIST extension nesting").at(offset)
}
