use bytes::Bytes;
use mail_protocol_core::wire::eq_ascii;
use mail_protocol_core::{ErrorKind, ProtocolError};

use crate::astring::{AStringKind, parse_astring_prefix};
use crate::codec::validate_tag;
use crate::tagged_ext::{LiteralPolicy, parse_value, validate_label};
use crate::{AString, SequenceSetRef};

/// Default maximum nesting accepted in an ESEARCH extension value.
pub const DEFAULT_ESEARCH_MAX_DEPTH: usize = 64;

/// One typed result item in an [`ESearchResponse`].
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum ESearchItem<'a> {
    /// Lowest matching message number or UID.
    Min(u32),
    /// Highest matching message number or UID.
    Max(u32),
    /// All matching message numbers or UIDs.
    All(SequenceSetRef<'a>),
    /// Number of matching messages.
    Count(u32),
    /// An extension item preserved as exact borrowed wire slices.
    Other {
        /// Extension label.
        name: &'a [u8],
        /// Complete extension value, including delimiters for strings or lists.
        value: &'a [u8],
    },
}

/// Validated, zero-copy ESEARCH response data.
///
/// Parsing performs one bounded validation pass. Result items are then decoded
/// lazily without allocating a collection proportional to the number of items.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ESearchResponse {
    wire: Bytes,
    tag: Option<AString>,
    uid: bool,
    items_start: usize,
    known: KnownItems,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct KnownItems {
    min: u32,
    max: u32,
    count: u32,
    all_start: usize,
    all_end: usize,
    present: u8,
}

const HAS_MIN: u8 = 1 << 0;
const HAS_MAX: u8 = 1 << 1;
const HAS_ALL: u8 = 1 << 2;
const HAS_COUNT: u8 = 1 << 3;

impl KnownItems {
    const fn new() -> Self {
        Self {
            min: 0,
            max: 0,
            count: 0,
            all_start: 0,
            all_end: 0,
            present: 0,
        }
    }

    const fn contains(&self, flag: u8) -> bool {
        self.present & flag != 0
    }
}

impl ESearchResponse {
    /// Parses complete untagged response data beginning with `ESEARCH`.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed correlators, invalid result values,
    /// non-synchronizing server literals, or extension nesting deeper than 64.
    pub fn parse(wire: &Bytes) -> Result<Self, ProtocolError> {
        Self::parse_with_max_depth(wire, DEFAULT_ESEARCH_MAX_DEPTH)
    }

    /// Parses ESEARCH data using an explicit extension nesting limit.
    ///
    /// A limit of zero still accepts scalar values but rejects any parenthesized
    /// extension value.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed syntax, invalid numeric boundaries, or an
    /// extension value exceeding `max_depth`.
    pub fn parse_with_max_depth(wire: &Bytes, max_depth: usize) -> Result<Self, ProtocolError> {
        const NAME: &[u8] = b"ESEARCH";
        if wire.len() < NAME.len() || !eq_ascii(&wire[..NAME.len()], NAME) {
            return Err(invalid("IMAP ESEARCH response name"));
        }
        if wire.len() > NAME.len() && wire[NAME.len()] != b' ' {
            return Err(invalid("IMAP ESEARCH response separator"));
        }

        let mut cursor = NAME.len();
        let tag = if wire.get(cursor..cursor + 2) == Some(b" (") {
            let (tag, end) = parse_correlator(wire, cursor)?;
            cursor = end;
            Some(tag)
        } else {
            None
        };

        let uid = if keyword_after_space(wire, cursor, b"UID") {
            cursor += b" UID".len();
            true
        } else {
            false
        };
        let items_start = cursor;
        let mut known = KnownItems::new();

        while cursor < wire.len() {
            if wire[cursor] != b' ' {
                return Err(invalid("IMAP ESEARCH item separator").at(cursor));
            }
            cursor += 1;
            let parsed = parse_item(wire, cursor, max_depth)?;
            match parsed.item {
                ESearchItem::Min(value) if !known.contains(HAS_MIN) => {
                    known.min = value;
                    known.present |= HAS_MIN;
                }
                ESearchItem::Max(value) if !known.contains(HAS_MAX) => {
                    known.max = value;
                    known.present |= HAS_MAX;
                }
                ESearchItem::All(value) if !known.contains(HAS_ALL) => {
                    known.all_start = parsed.end - value.as_bytes().len();
                    known.all_end = parsed.end;
                    known.present |= HAS_ALL;
                }
                ESearchItem::Count(value) if !known.contains(HAS_COUNT) => {
                    known.count = value;
                    known.present |= HAS_COUNT;
                }
                _ => {}
            }
            cursor = parsed.end;
        }

        Ok(Self {
            wire: wire.clone(),
            tag,
            uid,
            items_start,
            known,
        })
    }

    /// Returns the exact validated response data beginning with `ESEARCH`.
    pub fn as_bytes(&self) -> &[u8] {
        &self.wire
    }

    /// Returns the optional command-tag correlator.
    pub const fn tag(&self) -> Option<&AString> {
        self.tag.as_ref()
    }

    /// Returns whether numeric results are UIDs rather than message sequence numbers.
    pub const fn is_uid(&self) -> bool {
        self.uid
    }

    /// Iterates through result items without allocating.
    pub fn items(&self) -> ESearchItemIter<'_> {
        ESearchItemIter {
            wire: &self.wire,
            cursor: self.items_start,
        }
    }

    /// Returns the first MIN result, if present.
    pub const fn min(&self) -> Option<u32> {
        if self.known.contains(HAS_MIN) {
            Some(self.known.min)
        } else {
            None
        }
    }

    /// Returns the first MAX result, if present.
    pub const fn max(&self) -> Option<u32> {
        if self.known.contains(HAS_MAX) {
            Some(self.known.max)
        } else {
            None
        }
    }

    /// Returns the first ALL result, if present.
    pub fn all(&self) -> Option<SequenceSetRef<'_>> {
        if !self.known.contains(HAS_ALL) {
            return None;
        }
        Some(SequenceSetRef::from_validated_ranges(
            &self.wire[self.known.all_start..self.known.all_end],
        ))
    }

    /// Returns the first COUNT result, if present.
    pub const fn count(&self) -> Option<u32> {
        if self.known.contains(HAS_COUNT) {
            Some(self.known.count)
        } else {
            None
        }
    }
}

/// Allocation-free iterator over validated ESEARCH result items.
#[derive(Clone, Debug)]
pub struct ESearchItemIter<'a> {
    wire: &'a [u8],
    cursor: usize,
}

impl<'a> Iterator for ESearchItemIter<'a> {
    type Item = ESearchItem<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.cursor == self.wire.len() {
            return None;
        }
        let item_start = self.cursor.checked_add(1)?;
        let parsed = parse_item(self.wire, item_start, usize::MAX).ok()?;
        self.cursor = parsed.end;
        Some(parsed.item)
    }
}

struct ParsedItem<'a> {
    item: ESearchItem<'a>,
    end: usize,
}

fn parse_correlator(wire: &Bytes, start: usize) -> Result<(AString, usize), ProtocolError> {
    let mut cursor = start + 2;
    if wire
        .get(cursor..cursor + 3)
        .is_none_or(|name| !eq_ascii(name, b"TAG"))
    {
        return Err(invalid("IMAP ESEARCH TAG correlator").at(cursor));
    }
    cursor += 3;
    if wire.get(cursor) != Some(&b' ') {
        return Err(invalid("IMAP ESEARCH TAG separator").at(cursor));
    }
    cursor += 1;
    let end = if matches!(wire.get(cursor), Some(b'\"' | b'{')) {
        cursor + parse_astring_prefix(&wire[cursor..])?.end
    } else {
        wire[cursor..]
            .iter()
            .position(|byte| *byte == b')')
            .map(|offset| cursor + offset)
            .ok_or_else(|| invalid("IMAP ESEARCH TAG correlator terminator").at(wire.len()))?
    };
    let tag = AString::parse(&wire.slice(cursor..end))?;
    if matches!(
        tag.kind(),
        AStringKind::Literal {
            non_synchronizing: true
        }
    ) {
        return Err(invalid("non-synchronizing IMAP server literal").at(cursor));
    }
    if wire.get(end) != Some(&b')') {
        return Err(invalid("IMAP ESEARCH TAG correlator terminator").at(end));
    }
    validate_tag(tag.decoded().as_ref())?;
    Ok((tag, end + 1))
}

fn keyword_after_space(wire: &[u8], cursor: usize, keyword: &[u8]) -> bool {
    let start = cursor.saturating_add(1);
    let end = start.saturating_add(keyword.len());
    wire.get(cursor) == Some(&b' ')
        && wire
            .get(start..end)
            .is_some_and(|value| eq_ascii(value, keyword))
        && matches!(wire.get(end), None | Some(b' '))
}

fn parse_item(
    wire: &[u8],
    start: usize,
    max_depth: usize,
) -> Result<ParsedItem<'_>, ProtocolError> {
    let name_end = wire[start..]
        .iter()
        .position(|byte| *byte == b' ')
        .map(|offset| start + offset)
        .ok_or_else(|| invalid("IMAP ESEARCH item value").at(wire.len()))?;
    let name = &wire[start..name_end];
    validate_label(name).map_err(|error| error.at(start))?;

    let value_start = name_end + 1;
    let kind = result_kind(name);
    let end = if kind.is_some() {
        parse_known_value_end(wire, value_start)?
    } else {
        parse_value(
            wire,
            value_start,
            max_depth,
            LiteralPolicy::RejectNonSynchronizing,
        )?
        .end
    };
    let value = &wire[value_start..end];
    let item = match kind {
        Some(ResultKind::Min) => ESearchItem::Min(parse_number(value, true, "IMAP ESEARCH MIN")?),
        Some(ResultKind::Max) => ESearchItem::Max(parse_number(value, true, "IMAP ESEARCH MAX")?),
        Some(ResultKind::All) => {
            let set = SequenceSetRef::parse(value)?;
            if set.is_saved_search() {
                return Err(invalid("IMAP ESEARCH ALL saved-search marker").at(value_start));
            }
            ESearchItem::All(set)
        }
        Some(ResultKind::Count) => {
            ESearchItem::Count(parse_number(value, false, "IMAP ESEARCH COUNT")?)
        }
        None => ESearchItem::Other { name, value },
    };
    Ok(ParsedItem { item, end })
}

#[derive(Clone, Copy)]
enum ResultKind {
    Min,
    Max,
    All,
    Count,
}

fn result_kind(name: &[u8]) -> Option<ResultKind> {
    if eq_ascii(name, b"MIN") {
        Some(ResultKind::Min)
    } else if eq_ascii(name, b"MAX") {
        Some(ResultKind::Max)
    } else if eq_ascii(name, b"ALL") {
        Some(ResultKind::All)
    } else if eq_ascii(name, b"COUNT") {
        Some(ResultKind::Count)
    } else {
        None
    }
}

fn parse_known_value_end(wire: &[u8], start: usize) -> Result<usize, ProtocolError> {
    let end = wire[start..]
        .iter()
        .position(|byte| matches!(byte, b' ' | b')'))
        .map_or(wire.len(), |offset| start + offset);
    if end == start {
        Err(invalid("missing IMAP ESEARCH result value").at(start))
    } else {
        Ok(end)
    }
}

fn parse_number(value: &[u8], non_zero: bool, context: &'static str) -> Result<u32, ProtocolError> {
    if value.is_empty()
        || non_zero && value.first() == Some(&b'0')
        || !value.iter().all(u8::is_ascii_digit)
    {
        return Err(invalid(context));
    }
    let number = value.iter().try_fold(0u32, |number, digit| {
        number
            .checked_mul(10)
            .and_then(|number| number.checked_add(u32::from(*digit - b'0')))
    });
    match number {
        Some(0) if non_zero => Err(invalid(context)),
        Some(number) => Ok(number),
        None => Err(invalid(context)),
    }
}

fn invalid(context: &'static str) -> ProtocolError {
    ProtocolError::new(ErrorKind::InvalidSyntax, context)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Sequence, SequenceRange};

    #[test]
    fn parses_rfc_examples_and_empty_correlated_result() {
        let response = ESearchResponse::parse(&Bytes::from_static(
            b"ESEARCH (TAG \"a567\") UID COUNT 17 ALL 4:18,21,28",
        ))
        .unwrap();
        assert_eq!(response.tag().unwrap().decoded().as_ref(), b"a567");
        assert!(response.is_uid());
        assert_eq!(response.count(), Some(17));
        assert_eq!(
            response.all().unwrap().ranges().collect::<Vec<_>>(),
            vec![
                SequenceRange {
                    start: Sequence::Number(4),
                    end: Some(Sequence::Number(18)),
                },
                SequenceRange {
                    start: Sequence::Number(21),
                    end: None,
                },
                SequenceRange {
                    start: Sequence::Number(28),
                    end: None,
                },
            ]
        );

        let empty = ESearchResponse::parse(&Bytes::from_static(b"ESEARCH (TAG \"A284\")")).unwrap();
        assert_eq!(empty.items().count(), 0);
    }

    #[test]
    fn preserves_extensions_and_backing_allocation() {
        let wire = Bytes::from_static(
            b"ESEARCH MIN 2 X-NUM 123 X-SET 4:9 X-LIST (abc \"hello\" (one two) {3}\r\nxyz) X-EMPTY () COUNT 0 MAX 9",
        );
        let response = ESearchResponse::parse(&wire).unwrap();
        assert_eq!(response.as_bytes().as_ptr(), wire.as_ptr());
        assert_eq!(response.min(), Some(2));
        assert_eq!(response.max(), Some(9));
        assert_eq!(response.count(), Some(0));

        let extensions = response
            .items()
            .filter_map(|item| match item {
                ESearchItem::Other { name, value } => Some((name, value)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            extensions,
            vec![
                (&b"X-NUM"[..], &b"123"[..]),
                (&b"X-SET"[..], &b"4:9"[..]),
                (&b"X-LIST"[..], &b"(abc \"hello\" (one two) {3}\r\nxyz)"[..]),
                (&b"X-EMPTY"[..], &b"()"[..]),
            ]
        );
        assert!(extensions[0].1.as_ptr() > wire.as_ptr());
    }

    #[test]
    fn enforces_numeric_sequence_and_spacing_rules() {
        for invalid_wire in [
            b"ESEARCH MIN 0".as_slice(),
            b"ESEARCH MIN 01",
            b"ESEARCH MAX 4294967296",
            b"ESEARCH COUNT -1",
            b"ESEARCH ALL $",
            b"ESEARCH ALL 0",
            b"ESEARCH ALL 01",
            b"ESEARCH  COUNT 1",
            b"ESEARCH COUNT",
            b"ESEARCH 1BAD value",
            b"ESEARCH X (one  two)",
            b"ESEARCH X (one )",
            b"ESEARCH X {3+}\r\nabc",
            b"ESEARCH X abc",
            b"ESEARCH X \"abc\"",
            b"ESEARCH X {3}\r\nabc",
            b"ESEARCH X (one ())",
            b"ESEARCH X 9223372036854775808",
        ] {
            assert!(
                ESearchResponse::parse(&Bytes::copy_from_slice(invalid_wire)).is_err(),
                "accepted {:?}",
                String::from_utf8_lossy(invalid_wire)
            );
        }
    }

    #[test]
    fn validates_correlator_tag_and_exact_grammar() {
        for invalid_wire in [
            b"ESEARCH (TAG)".as_slice(),
            b"ESEARCH (TAGS A1)",
            b"ESEARCH (TAG \"bad tag\")",
            b"ESEARCH (TAG \"\xe9\x82\xae\xe4\xbb\xb6\")",
            b"ESEARCH (TAG {2+}\r\nA1)",
            b"ESEARCH (TAG A1)UID COUNT 1",
            b"ESEARCHING COUNT 1",
        ] {
            assert!(ESearchResponse::parse(&Bytes::copy_from_slice(invalid_wire)).is_err());
        }
    }

    #[test]
    fn extension_nesting_is_bounded() {
        let mut wire = Vec::from(b"ESEARCH X ".as_slice());
        wire.extend(std::iter::repeat_n(b'(', 64));
        wire.push(b'a');
        wire.extend(std::iter::repeat_n(b')', 64));
        let wire = Bytes::from(wire);

        assert!(ESearchResponse::parse_with_max_depth(&wire, 64).is_ok());
        assert_eq!(
            ESearchResponse::parse_with_max_depth(&wire, 63)
                .unwrap_err()
                .kind(),
            ErrorKind::NestingTooDeep
        );
    }

    #[test]
    fn duplicate_items_remain_iterable_and_accessors_use_first() {
        let response = ESearchResponse::parse(&Bytes::from_static(
            b"ESEARCH COUNT 1 COUNT 2 ALL 3 ALL 4 X-N64 9223372036854775807",
        ))
        .unwrap();
        assert_eq!(response.count(), Some(1));
        assert_eq!(response.all().unwrap().as_bytes(), b"3");
        assert_eq!(response.items().count(), 5);
    }

    #[test]
    fn explicit_large_budget_remains_iterative() {
        const DEPTH: usize = 4_096;
        let mut wire = Vec::with_capacity(DEPTH * 2 + 16);
        wire.extend_from_slice(b"ESEARCH X ");
        wire.extend(std::iter::repeat_n(b'(', DEPTH));
        wire.push(b'a');
        wire.extend(std::iter::repeat_n(b')', DEPTH));
        let wire = Bytes::from(wire);

        let parsed = ESearchResponse::parse_with_max_depth(&wire, DEPTH).unwrap();
        assert_eq!(parsed.items().count(), 1);
    }
}
