use bytes::Bytes;
use mail_protocol_core::{ErrorKind, ProtocolError};

use crate::{
    AString, Command, CommandBody,
    astring::{ParsedAString, parse_astring_prefix},
    tagged_ext::{LiteralPolicy, parse_value, validate_label},
};

const MAX_NUMBER: u64 = u32::MAX as u64;
const MAX_NUMBER64: u64 = i64::MAX as u64;
/// Default maximum nesting accepted in STATUS extension values.
pub const DEFAULT_STATUS_MAX_DEPTH: usize = 64;

/// A validated RFC 9051 STATUS item list.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct StatusItems {
    wire: Bytes,
}

impl StatusItems {
    /// Parses one non-empty parenthesized STATUS item list without copying.
    ///
    /// Unknown atom names are retained for extension compatibility.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty list, invalid atom, tab, repeated space,
    /// trailing space, missing parenthesis, or trailing data.
    pub fn parse(wire: &Bytes) -> Result<Self, ProtocolError> {
        validate_status_items(wire)?;
        Ok(Self { wire: wire.clone() })
    }

    /// Returns the exact parenthesized wire value.
    pub const fn as_bytes(&self) -> &Bytes {
        &self.wire
    }

    /// Iterates over known and extension item names without allocating.
    pub fn iter(&self) -> StatusItemIter<'_> {
        StatusItemIter {
            remaining: &self.wire[1..self.wire.len() - 1],
        }
    }
}

impl<'a> IntoIterator for &'a StatusItems {
    type Item = StatusItem<'a>;
    type IntoIter = StatusItemIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// One requested STATUS attribute.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum StatusItem<'a> {
    /// Message count.
    Messages,
    /// Count of messages with the legacy `IMAP4rev1` `\\Recent` flag.
    Recent,
    /// Predicted next UID.
    UidNext,
    /// UID validity value.
    UidValidity,
    /// Count of unseen messages.
    Unseen,
    /// Count of messages marked deleted.
    Deleted,
    /// Approximate mailbox size in octets.
    Size,
    /// Highest persistent RFC 7162 mailbox modification sequence, or zero.
    HighestModSeq,
    /// Storage in octets currently occupied by messages marked `\\Deleted`.
    DeletedStorage,
    /// Syntactically valid extension item.
    Other(&'a [u8]),
}

/// Allocation-free iterator returned by [`StatusItems::iter`].
#[derive(Clone, Debug)]
pub struct StatusItemIter<'a> {
    remaining: &'a [u8],
}

impl<'a> Iterator for StatusItemIter<'a> {
    type Item = StatusItem<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining.is_empty() {
            return None;
        }
        let end = self
            .remaining
            .iter()
            .position(|byte| *byte == b' ')
            .unwrap_or(self.remaining.len());
        let item = &self.remaining[..end];
        self.remaining = if end == self.remaining.len() {
            &[]
        } else {
            &self.remaining[end + 1..]
        };
        Some(parse_item(item))
    }
}

/// Parsed untagged `STATUS` mailbox data.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct MailboxStatus {
    mailbox: AString,
    values: StatusValues,
}

impl MailboxStatus {
    /// Parses a mailbox followed by a parenthesized status value list.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid mailbox astring, separator, base status
    /// value, numeric width, or extension value structure.
    pub fn parse(input: &Bytes) -> Result<Self, ProtocolError> {
        Self::parse_with_max_depth(input, DEFAULT_STATUS_MAX_DEPTH)
    }

    /// Parses STATUS data with an explicit extension nesting limit.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::parse`] and rejects extension values
    /// whose tagged-ext-val depth exceeds `max_depth`.
    pub fn parse_with_max_depth(input: &Bytes, max_depth: usize) -> Result<Self, ProtocolError> {
        let ParsedAString { end, .. } = parse_astring_prefix(input)?;
        if input.get(end) != Some(&b' ')
            || input.get(end + 1).is_none()
            || input.get(end + 1) == Some(&b' ')
        {
            return Err(invalid("IMAP STATUS response separator").at(end));
        }
        let mailbox_wire = input.slice(..end);
        let values_wire = input.slice(end + 1..);
        Ok(Self {
            mailbox: AString::parse(&mailbox_wire)?,
            values: StatusValues::parse_with_max_depth(&values_wire, max_depth)?,
        })
    }

    /// Returns the zero-copy mailbox astring.
    pub const fn mailbox(&self) -> &AString {
        &self.mailbox
    }

    /// Returns the validated status values.
    pub const fn values(&self) -> &StatusValues {
        &self.values
    }
}

/// Validated parenthesized values in an untagged STATUS response.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct StatusValues {
    wire: Bytes,
}

impl StatusValues {
    /// Parses base RFC 9051 values and structurally safe extension values.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed pairs, values outside the RFC `number`
    /// or `number64` ranges, zero UID values, or unbalanced extension data.
    pub fn parse(wire: &Bytes) -> Result<Self, ProtocolError> {
        Self::parse_with_max_depth(wire, DEFAULT_STATUS_MAX_DEPTH)
    }

    /// Parses values with an explicit tagged extension nesting limit.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::parse`] and `NestingTooDeep` when an
    /// extension value exceeds `max_depth`.
    pub fn parse_with_max_depth(wire: &Bytes, max_depth: usize) -> Result<Self, ProtocolError> {
        validate_status_values(wire, max_depth)?;
        Ok(Self { wire: wire.clone() })
    }

    /// Returns the exact parenthesized wire value.
    pub const fn as_bytes(&self) -> &Bytes {
        &self.wire
    }

    /// Iterates over validated base and extension values without allocating.
    pub fn iter(&self) -> StatusValueIter<'_> {
        StatusValueIter {
            remaining: &self.wire[1..self.wire.len() - 1],
        }
    }
}

impl<'a> IntoIterator for &'a StatusValues {
    type Item = StatusValue<'a>;
    type IntoIter = StatusValueIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// One value in an untagged STATUS response.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum StatusValue<'a> {
    /// Message count.
    Messages(u32),
    /// Count of messages with the legacy `IMAP4rev1` `\\Recent` flag.
    Recent(u32),
    /// Predicted next non-zero UID.
    UidNext(u32),
    /// Non-zero UID validity value.
    UidValidity(u32),
    /// Count of unseen messages.
    Unseen(u32),
    /// Count of messages marked deleted.
    Deleted(u32),
    /// Approximate mailbox size in octets.
    Size(u64),
    /// Highest persistent RFC 7162 mailbox modification sequence, or zero.
    HighestModSeq(u64),
    /// Storage in octets currently occupied by messages marked `\\Deleted`.
    DeletedStorage(u64),
    /// Extension value preserved in wire form.
    Other {
        /// Extension attribute name.
        name: &'a [u8],
        /// One generic extension value.
        value: &'a [u8],
    },
}

/// Allocation-free iterator returned by [`StatusValues::iter`].
#[derive(Clone, Debug)]
pub struct StatusValueIter<'a> {
    remaining: &'a [u8],
}

impl<'a> Iterator for StatusValueIter<'a> {
    type Item = StatusValue<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining.is_empty() {
            return None;
        }
        let Ok((value, end)) = parse_status_value(self.remaining, usize::MAX) else {
            debug_assert!(false, "validated STATUS values became invalid");
            self.remaining = &[];
            return None;
        };
        self.remaining = if end == self.remaining.len() {
            &[]
        } else {
            &self.remaining[end + 1..]
        };
        Some(value)
    }
}

impl Command {
    /// Parses the item list when this is a STATUS command.
    ///
    /// # Errors
    ///
    /// Returns an error if a manually constructed STATUS command contains an
    /// invalid item list. Decoded commands have already been validated.
    pub fn parsed_status_items(&self) -> Result<Option<StatusItems>, ProtocolError> {
        match &self.body {
            CommandBody::Status { items, .. } => StatusItems::parse(items).map(Some),
            _ => Ok(None),
        }
    }
}

pub(crate) fn validate_status_items(input: &[u8]) -> Result<(), ProtocolError> {
    if input.len() < 3 || input.first() != Some(&b'(') || input.last() != Some(&b')') {
        return Err(invalid("IMAP STATUS item list"));
    }
    let items = &input[1..input.len() - 1];
    if items.is_empty() {
        return Err(invalid("empty IMAP STATUS item list"));
    }
    let mut previous_space = true;
    for byte in items {
        if *byte == b' ' {
            if previous_space {
                return Err(invalid("IMAP STATUS item separator"));
            }
            previous_space = true;
        } else {
            if !is_atom_char(*byte) {
                return Err(invalid("IMAP STATUS item"));
            }
            previous_space = false;
        }
    }
    if previous_space {
        return Err(invalid("IMAP STATUS item separator"));
    }
    Ok(())
}

fn validate_status_values(input: &[u8], max_depth: usize) -> Result<(), ProtocolError> {
    if input.len() < 5 || input.first() != Some(&b'(') || input.last() != Some(&b')') {
        return Err(invalid("IMAP STATUS response values"));
    }
    let values = &input[1..input.len() - 1];
    let mut cursor = 0;
    while cursor < values.len() {
        let (_, consumed) = parse_status_value(&values[cursor..], max_depth)?;
        cursor += consumed;
        if cursor == values.len() {
            break;
        }
        if values.get(cursor) != Some(&b' ')
            || values.get(cursor + 1).is_none()
            || values.get(cursor + 1) == Some(&b' ')
        {
            return Err(invalid("IMAP STATUS response value separator").at(cursor));
        }
        cursor += 1;
    }
    Ok(())
}

fn parse_status_value(
    input: &[u8],
    max_depth: usize,
) -> Result<(StatusValue<'_>, usize), ProtocolError> {
    let Some(name_end) = input.iter().position(|byte| *byte == b' ') else {
        return Err(invalid("IMAP STATUS response value"));
    };
    let name = &input[..name_end];
    let known = known_status_value(name);
    if known.is_none() {
        validate_label(name)?;
    }
    let value_input = &input[name_end + 1..];
    if value_input.is_empty() || value_input.first() == Some(&b' ') {
        return Err(invalid("IMAP STATUS response value separator").at(name_end));
    }
    let value_end = if known.is_some() {
        value_input
            .iter()
            .position(|byte| *byte == b' ')
            .unwrap_or(value_input.len())
    } else {
        parse_value(
            value_input,
            0,
            max_depth,
            LiteralPolicy::RejectNonSynchronizing,
        )?
        .end
    };
    let value = &value_input[..value_end];
    let parsed = match known {
        Some(KnownStatusValue::Messages) => StatusValue::Messages(parse_number32(value, false)?),
        Some(KnownStatusValue::Recent) => StatusValue::Recent(parse_number32(value, false)?),
        Some(KnownStatusValue::UidNext) => StatusValue::UidNext(parse_number32(value, true)?),
        Some(KnownStatusValue::UidValidity) => {
            StatusValue::UidValidity(parse_number32(value, true)?)
        }
        Some(KnownStatusValue::Unseen) => StatusValue::Unseen(parse_number32(value, false)?),
        Some(KnownStatusValue::Deleted) => StatusValue::Deleted(parse_number32(value, false)?),
        Some(KnownStatusValue::Size) => {
            StatusValue::Size(parse_number(value, MAX_NUMBER64, false)?)
        }
        Some(KnownStatusValue::HighestModSeq) => {
            StatusValue::HighestModSeq(parse_number(value, MAX_NUMBER64, false)?)
        }
        Some(KnownStatusValue::DeletedStorage) => {
            StatusValue::DeletedStorage(parse_number(value, MAX_NUMBER64, false)?)
        }
        None => StatusValue::Other { name, value },
    };
    Ok((parsed, name_end + 1 + value_end))
}

#[derive(Clone, Copy)]
enum KnownStatusValue {
    Messages,
    Recent,
    UidNext,
    UidValidity,
    Unseen,
    Deleted,
    Size,
    HighestModSeq,
    DeletedStorage,
}

fn known_status_value(name: &[u8]) -> Option<KnownStatusValue> {
    if name.eq_ignore_ascii_case(b"MESSAGES") {
        Some(KnownStatusValue::Messages)
    } else if name.eq_ignore_ascii_case(b"RECENT") {
        Some(KnownStatusValue::Recent)
    } else if name.eq_ignore_ascii_case(b"UIDNEXT") {
        Some(KnownStatusValue::UidNext)
    } else if name.eq_ignore_ascii_case(b"UIDVALIDITY") {
        Some(KnownStatusValue::UidValidity)
    } else if name.eq_ignore_ascii_case(b"UNSEEN") {
        Some(KnownStatusValue::Unseen)
    } else if name.eq_ignore_ascii_case(b"DELETED") {
        Some(KnownStatusValue::Deleted)
    } else if name.eq_ignore_ascii_case(b"SIZE") {
        Some(KnownStatusValue::Size)
    } else if name.eq_ignore_ascii_case(b"HIGHESTMODSEQ") {
        Some(KnownStatusValue::HighestModSeq)
    } else if name.eq_ignore_ascii_case(b"DELETED-STORAGE") {
        Some(KnownStatusValue::DeletedStorage)
    } else {
        None
    }
}

fn parse_number(input: &[u8], maximum: u64, non_zero: bool) -> Result<u64, ProtocolError> {
    if input.is_empty() || !input.iter().all(u8::is_ascii_digit) {
        return Err(invalid("IMAP STATUS numeric value"));
    }
    let mut value = 0u64;
    for digit in input {
        value = value
            .checked_mul(10)
            .and_then(|number| number.checked_add(u64::from(*digit - b'0')))
            .ok_or_else(|| invalid("IMAP STATUS numeric value"))?;
    }
    if value > maximum || (non_zero && value == 0) {
        Err(invalid("IMAP STATUS numeric value"))
    } else {
        Ok(value)
    }
}

fn parse_number32(input: &[u8], non_zero: bool) -> Result<u32, ProtocolError> {
    u32::try_from(parse_number(input, MAX_NUMBER, non_zero)?)
        .map_err(|_| invalid("IMAP STATUS numeric value"))
}

fn parse_item(input: &[u8]) -> StatusItem<'_> {
    if input.eq_ignore_ascii_case(b"MESSAGES") {
        StatusItem::Messages
    } else if input.eq_ignore_ascii_case(b"RECENT") {
        StatusItem::Recent
    } else if input.eq_ignore_ascii_case(b"UIDNEXT") {
        StatusItem::UidNext
    } else if input.eq_ignore_ascii_case(b"UIDVALIDITY") {
        StatusItem::UidValidity
    } else if input.eq_ignore_ascii_case(b"UNSEEN") {
        StatusItem::Unseen
    } else if input.eq_ignore_ascii_case(b"DELETED") {
        StatusItem::Deleted
    } else if input.eq_ignore_ascii_case(b"SIZE") {
        StatusItem::Size
    } else if input.eq_ignore_ascii_case(b"HIGHESTMODSEQ") {
        StatusItem::HighestModSeq
    } else if input.eq_ignore_ascii_case(b"DELETED-STORAGE") {
        StatusItem::DeletedStorage
    } else {
        StatusItem::Other(input)
    }
}

const fn is_atom_char(byte: u8) -> bool {
    byte.is_ascii()
        && !byte.is_ascii_control()
        && !matches!(
            byte,
            b' ' | b'(' | b')' | b'{' | b'%' | b'*' | b'"' | b'\\' | b']'
        )
}

const fn invalid(context: &'static str) -> ProtocolError {
    ProtocolError::new(ErrorKind::InvalidSyntax, context)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_and_extension_items_without_copying() {
        let wire = Bytes::from_static(
            b"(MESSAGES RECENT UIDNEXT UIDVALIDITY UNSEEN DELETED SIZE HIGHESTMODSEQ DELETED-STORAGE X-COUNT)",
        );
        let pointer = wire.as_ptr();
        let items = StatusItems::parse(&wire).unwrap();
        assert_eq!(items.as_bytes().as_ptr(), pointer);
        assert_eq!(
            items.iter().collect::<Vec<_>>(),
            vec![
                StatusItem::Messages,
                StatusItem::Recent,
                StatusItem::UidNext,
                StatusItem::UidValidity,
                StatusItem::Unseen,
                StatusItem::Deleted,
                StatusItem::Size,
                StatusItem::HighestModSeq,
                StatusItem::DeletedStorage,
                StatusItem::Other(b"X-COUNT"),
            ]
        );
    }

    #[test]
    fn known_items_are_case_insensitive() {
        let wire =
            Bytes::from_static(b"(messages recent UidNext size highestmodseq deleted-storage)");
        assert_eq!(
            StatusItems::parse(&wire)
                .unwrap()
                .iter()
                .collect::<Vec<_>>(),
            vec![
                StatusItem::Messages,
                StatusItem::Recent,
                StatusItem::UidNext,
                StatusItem::Size,
                StatusItem::HighestModSeq,
                StatusItem::DeletedStorage,
            ]
        );
    }

    #[test]
    fn rejects_malformed_lists() {
        for wire in [
            b"".as_slice(),
            b"()",
            b"MESSAGES",
            b"(MESSAGES",
            b"(MESSAGES) trailing",
            b"(MESSAGES  SIZE)",
            b"(MESSAGES )",
            b"(MESSAGES\tSIZE)",
            b"(MESSAGES])",
        ] {
            assert!(
                StatusItems::parse(&Bytes::copy_from_slice(wire)).is_err(),
                "{wire:?}"
            );
        }
    }

    #[test]
    fn parses_mailbox_status_base_values_and_extensions() {
        let wire = Bytes::from_static(
            b"{5}\r\nINBOX (MESSAGES 231 RECENT 1 UIDNEXT 44292 UIDVALIDITY 7 UNSEEN 3 DELETED 2 SIZE 99 HIGHESTMODSEQ 715194045007 DELETED-STORAGE 42 X-EXT (one \"two three\"))",
        );
        let status = MailboxStatus::parse(&wire).unwrap();
        assert_eq!(status.mailbox().decoded(), b"INBOX".as_slice());
        assert_eq!(
            status.values().iter().collect::<Vec<_>>(),
            vec![
                StatusValue::Messages(231),
                StatusValue::Recent(1),
                StatusValue::UidNext(44_292),
                StatusValue::UidValidity(7),
                StatusValue::Unseen(3),
                StatusValue::Deleted(2),
                StatusValue::Size(99),
                StatusValue::HighestModSeq(715_194_045_007),
                StatusValue::DeletedStorage(42),
                StatusValue::Other {
                    name: b"X-EXT",
                    value: b"(one \"two three\")",
                },
            ]
        );
    }

    #[test]
    fn enforces_status_number_width_and_non_zero_uids() {
        for valid in [
            b"INBOX (MESSAGES 4294967295)".as_slice(),
            b"INBOX (UIDNEXT 4294967295)",
            b"INBOX (SIZE 9223372036854775807)",
            b"INBOX (DELETED-STORAGE 9223372036854775807)",
            b"INBOX (HIGHESTMODSEQ 0)",
            b"INBOX (HIGHESTMODSEQ 9223372036854775807)",
        ] {
            assert!(MailboxStatus::parse(&Bytes::copy_from_slice(valid)).is_ok());
        }

        for invalid_wire in [
            b"INBOX (MESSAGES 4294967296)".as_slice(),
            b"INBOX (UIDNEXT 0)",
            b"INBOX (UIDVALIDITY 0)",
            b"INBOX (SIZE 9223372036854775808)",
            b"INBOX (DELETED-STORAGE 9223372036854775808)",
            b"INBOX (HIGHESTMODSEQ 9223372036854775808)",
            b"INBOX (MESSAGES -1)",
            b"INBOX (MESSAGES 1  UIDNEXT 2)",
            b"INBOX ()",
            b"INBOX (X-EXT (unterminated)",
        ] {
            assert!(
                MailboxStatus::parse(&Bytes::copy_from_slice(invalid_wire)).is_err(),
                "{invalid_wire:?}"
            );
        }
    }

    #[test]
    fn status_extensions_share_bounded_tagged_value_grammar() {
        const DEPTH: usize = 4_096;

        let mut value = Vec::from(b"(X-NEST ".as_slice());
        value.extend(std::iter::repeat_n(b'(', DEFAULT_STATUS_MAX_DEPTH));
        value.push(b'x');
        value.extend(std::iter::repeat_n(b')', DEFAULT_STATUS_MAX_DEPTH));
        value.push(b')');
        let value = Bytes::from(value);
        assert!(StatusValues::parse(&value).is_ok());
        assert_eq!(
            StatusValues::parse_with_max_depth(&value, DEFAULT_STATUS_MAX_DEPTH - 1)
                .unwrap_err()
                .kind(),
            ErrorKind::NestingTooDeep
        );

        for invalid_wire in [
            b"(X-LIST (one  two))".as_slice(),
            b"(X-LIST (one ()))",
            b"(1BAD 7)",
            b"(X-LITERAL ({1+}\r\nx))",
        ] {
            assert!(
                StatusValues::parse(&Bytes::copy_from_slice(invalid_wire)).is_err(),
                "{invalid_wire:?}"
            );
        }

        let mut deep = Vec::from(b"(X-NEST ".as_slice());
        deep.extend(std::iter::repeat_n(b'(', DEPTH));
        deep.push(b'x');
        deep.extend(std::iter::repeat_n(b')', DEPTH));
        deep.push(b')');
        let deep = Bytes::from(deep);
        assert!(StatusValues::parse_with_max_depth(&deep, DEPTH).is_ok());
    }
}
