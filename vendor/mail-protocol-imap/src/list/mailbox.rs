use std::borrow::Cow;
use std::ops::Range;

use bytes::Bytes;
use mail_protocol_core::ProtocolError;

use crate::astring::{AStringKind, parse_astring_prefix};

use super::validation::{decode_string_content, invalid, is_list_char};

/// Wire representation selected for a LIST mailbox pattern.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum ListMailboxKind {
    /// An unquoted mailbox pattern, which can include `*`, `%`, and `]`.
    Atom,
    /// A double-quoted string.
    Quoted,
    /// A length-prefixed string.
    Literal {
        /// Whether the marker uses the `{n+}` non-synchronizing form.
        non_synchronizing: bool,
    },
}

/// A validated, zero-copy RFC 9051 `list-mailbox` value.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ListMailbox {
    wire: Bytes,
    content: Range<usize>,
    kind: ListMailboxKind,
}

impl ListMailbox {
    /// Parses exactly one LIST mailbox name or wildcard pattern.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty bare value, invalid list characters,
    /// malformed quoted/literal strings, or trailing data.
    pub fn parse(wire: &Bytes) -> Result<Self, ProtocolError> {
        let parsed = parse_list_mailbox_prefix(wire)?;
        if parsed.end != wire.len() {
            return Err(invalid("trailing IMAP LIST mailbox data").at(parsed.end));
        }
        Ok(Self::from_parsed(wire.clone(), parsed))
    }

    pub(super) fn from_parsed(wire: Bytes, parsed: ParsedListMailbox) -> Self {
        Self {
            wire,
            content: parsed.content,
            kind: parsed.kind,
        }
    }

    /// Returns the exact encoded value.
    pub const fn as_bytes(&self) -> &Bytes {
        &self.wire
    }

    /// Returns the selected wire representation.
    pub const fn kind(&self) -> ListMailboxKind {
        self.kind
    }

    /// Returns the encoded atom, quoted content, or literal payload.
    pub fn encoded_content(&self) -> &[u8] {
        &self.wire[self.content.clone()]
    }

    /// Returns the logical value, allocating only when quoted escapes occur.
    pub fn decoded(&self) -> Cow<'_, [u8]> {
        decode_string_content(self.encoded_content(), self.kind == ListMailboxKind::Quoted)
    }
}

/// Allocation-free iterator over one or more validated LIST mailbox patterns.
#[derive(Clone, Debug)]
pub struct ListMailboxIter<'a> {
    wire: &'a Bytes,
    cursor: usize,
    end: usize,
}

impl<'a> ListMailboxIter<'a> {
    pub(super) fn new(wire: &'a Bytes, range: Range<usize>) -> Self {
        Self {
            wire,
            cursor: range.start,
            end: range.end,
        }
    }
}

impl Iterator for ListMailboxIter<'_> {
    type Item = ListMailbox;

    fn next(&mut self) -> Option<Self::Item> {
        if self.cursor == self.end {
            return None;
        }
        let start = self.cursor;
        let parsed = match parse_list_mailbox_prefix(&self.wire[start..self.end]) {
            Ok(parsed) => parsed,
            Err(error) => {
                debug_assert!(
                    false,
                    "validated LIST mailbox pattern became invalid: {error}"
                );
                self.cursor = self.end;
                return None;
            }
        };
        let value_end = start + parsed.end;
        self.cursor = if value_end == self.end {
            self.end
        } else if self.wire.get(value_end) == Some(&b' ') && self.wire.get(value_end + 1).is_some()
        {
            value_end + 1
        } else {
            debug_assert!(false, "validated LIST mailbox separator disappeared");
            self.end
        };
        Some(ListMailbox::from_parsed(
            self.wire.slice(start..value_end),
            parsed,
        ))
    }
}

#[derive(Clone, Debug)]
pub(super) struct ParsedListMailbox {
    pub(super) end: usize,
    pub(super) content: Range<usize>,
    pub(super) kind: ListMailboxKind,
}

#[inline]
pub(super) fn parse_list_mailbox_prefix(input: &[u8]) -> Result<ParsedListMailbox, ProtocolError> {
    if matches!(input.first(), Some(b'"' | b'{')) {
        let parsed = parse_astring_prefix(input)?;
        let kind = match parsed.kind {
            AStringKind::Atom => ListMailboxKind::Atom,
            AStringKind::Quoted => ListMailboxKind::Quoted,
            AStringKind::Literal { non_synchronizing } => {
                ListMailboxKind::Literal { non_synchronizing }
            }
        };
        return Ok(ParsedListMailbox {
            end: parsed.end,
            content: parsed.content,
            kind,
        });
    }
    let end = input
        .iter()
        .position(|byte| matches!(byte, b' ' | b')'))
        .unwrap_or(input.len());
    let value = &input[..end];
    if value.is_empty() || value.iter().any(|byte| !is_list_char(*byte)) {
        return Err(invalid("IMAP LIST mailbox"));
    }
    Ok(ParsedListMailbox {
        end,
        content: 0..end,
        kind: ListMailboxKind::Atom,
    })
}
