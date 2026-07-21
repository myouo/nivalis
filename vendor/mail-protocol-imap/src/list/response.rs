use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::ops::Range;

use bytes::Bytes;
use mail_protocol_core::ProtocolError;
use mail_protocol_core::wire::eq_ascii;

use crate::AString;
use crate::astring::{AStringKind, parse_astring_prefix};
use crate::tagged_ext::{LiteralPolicy, parse_value};

use super::DEFAULT_LIST_MAX_DEPTH;
use super::options::{ListSelectionOption, parse_selection_option, selection_option_prefilter_bit};
use super::validation::{
    decode_string_content, invalid, is_atom_char, list_interior, next_list_item, parenthesized_end,
    require_space, shift_error, validate_astring_atom,
};

/// One typed mailbox-name attribute in a LIST response.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum ListAttribute<'a> {
    NoInferiors,
    HasChildren,
    HasNoChildren,
    Subscribed,
    Remote,
    NonExistent,
    NoSelect,
    Marked,
    Unmarked,
    Other(&'a [u8]),
}

/// Allocation-free iterator over validated LIST attributes.
#[derive(Clone, Debug)]
pub struct ListAttributeIter<'a> {
    remaining: &'a [u8],
}

impl<'a> Iterator for ListAttributeIter<'a> {
    type Item = ListAttribute<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining.is_empty() {
            return None;
        }
        let end = self
            .remaining
            .iter()
            .position(|byte| *byte == b' ')
            .unwrap_or(self.remaining.len());
        let attribute = parse_attribute(&self.remaining[..end]).ok()?;
        self.remaining = next_list_item(self.remaining, end)?;
        Some(attribute)
    }
}

/// One LIST response extended-data item.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum ListExtendedItem<'a> {
    ChildInfo { options: &'a [u8] },
    OldName { mailbox: &'a [u8] },
    Other { name: &'a [u8], value: &'a [u8] },
}

/// Allocation-free iterator over validated LIST response extended data.
#[derive(Clone, Debug)]
pub struct ListExtendedItemIter<'a> {
    remaining: &'a [u8],
}

impl<'a> Iterator for ListExtendedItemIter<'a> {
    type Item = ListExtendedItem<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining.is_empty() {
            return None;
        }
        let parsed = match parse_extended_item(self.remaining, usize::MAX) {
            Ok(parsed) => parsed,
            Err(error) => {
                debug_assert!(
                    false,
                    "validated LIST extended item became invalid: {error}"
                );
                self.remaining = &[];
                return None;
            }
        };
        self.remaining = next_list_item(self.remaining, parsed.end)?;
        Some(parsed.item)
    }
}

/// Validated, zero-copy untagged LIST or legacy LSUB response data.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ListResponse {
    wire: Bytes,
    attributes: Range<usize>,
    delimiter_wire: Range<usize>,
    delimiter: Option<u8>,
    mailbox: AString,
    extended: Option<Range<usize>>,
    extension_depth: usize,
}

impl ListResponse {
    /// Parses complete untagged response data beginning with `LIST`.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid or duplicate attributes, conflicting
    /// flags, malformed delimiter/mailbox strings, or malformed extended data.
    pub fn parse(wire: &Bytes) -> Result<Self, ProtocolError> {
        Self::parse_with_max_depth(wire, DEFAULT_LIST_MAX_DEPTH)
    }

    /// Parses LIST response data with an explicit extension nesting limit.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::parse`] and `NestingTooDeep` when a
    /// generic extended-data value exceeds `max_depth`.
    pub fn parse_with_max_depth(wire: &Bytes, max_depth: usize) -> Result<Self, ProtocolError> {
        Self::parse_named_with_max_depth(wire, b"LIST", max_depth)
    }

    pub(crate) fn parse_lsub_with_max_depth(
        wire: &Bytes,
        max_depth: usize,
    ) -> Result<Self, ProtocolError> {
        Self::parse_named_with_max_depth(wire, b"LSUB", max_depth)
    }

    fn parse_named_with_max_depth(
        wire: &Bytes,
        name: &[u8],
        max_depth: usize,
    ) -> Result<Self, ProtocolError> {
        let parsed = parse_list_response(wire, name, max_depth)?;
        let mailbox_wire = wire.slice(parsed.mailbox.clone());
        Ok(Self {
            wire: wire.clone(),
            attributes: parsed.attributes,
            delimiter_wire: parsed.delimiter_wire,
            delimiter: parsed.delimiter,
            mailbox: AString::parse(&mailbox_wire)?,
            extended: parsed.extended,
            extension_depth: parsed.extension_depth,
        })
    }

    pub const fn as_bytes(&self) -> &Bytes {
        &self.wire
    }

    pub fn attributes_wire(&self) -> &[u8] {
        &self.wire[self.attributes.clone()]
    }

    pub fn attributes(&self) -> ListAttributeIter<'_> {
        ListAttributeIter {
            remaining: list_interior(&self.wire, Some(&self.attributes)),
        }
    }

    pub const fn delimiter(&self) -> Option<u8> {
        self.delimiter
    }

    pub fn delimiter_wire(&self) -> &[u8] {
        &self.wire[self.delimiter_wire.clone()]
    }

    pub const fn mailbox(&self) -> &AString {
        &self.mailbox
    }

    pub fn extended_data(&self) -> Option<&[u8]> {
        self.extended
            .as_ref()
            .map(|range| &self.wire[range.clone()])
    }

    pub fn extended_items(&self) -> ListExtendedItemIter<'_> {
        ListExtendedItemIter {
            remaining: list_interior(&self.wire, self.extended.as_ref()),
        }
    }

    pub fn has_child_info(&self) -> bool {
        self.extended_items()
            .any(|item| matches!(item, ListExtendedItem::ChildInfo { .. }))
    }

    pub(crate) fn child_info_prefilter(&self) -> Option<u64> {
        let mut found = false;
        let mut mask = 0u64;
        for item in self.extended_items() {
            let ListExtendedItem::ChildInfo { options } = item else {
                continue;
            };
            found = true;
            let Some(item_mask) = child_info_options_prefilter(options) else {
                debug_assert!(false, "validated IMAP CHILDINFO became invalid");
                return Some(u64::MAX);
            };
            mask |= item_mask;
        }
        found.then_some(mask)
    }

    pub const fn extension_depth(&self) -> usize {
        self.extension_depth
    }

    pub fn exists(&self) -> bool {
        !self
            .attributes()
            .any(|attribute| attribute == ListAttribute::NonExistent)
    }

    pub fn is_selectable(&self) -> bool {
        !self.attributes().any(|attribute| {
            matches!(
                attribute,
                ListAttribute::NonExistent | ListAttribute::NoSelect
            )
        })
    }

    pub fn has_children(&self) -> Option<bool> {
        let mut no_children = false;
        for attribute in self.attributes() {
            match attribute {
                ListAttribute::HasChildren => return Some(true),
                ListAttribute::HasNoChildren | ListAttribute::NoInferiors => no_children = true,
                _ => {}
            }
        }
        no_children.then_some(false)
    }
}

fn child_info_options_prefilter(options: &[u8]) -> Option<u64> {
    let mut remaining = options.strip_prefix(b"(")?.strip_suffix(b")")?;
    let mut mask = 0u64;
    while !remaining.is_empty() {
        let value = parse_astring_prefix(remaining).ok()?;
        if value.kind != AStringKind::Quoted {
            return None;
        }
        let decoded = decode_string_content(&remaining[value.content.clone()], true);
        let option = parse_selection_option(&decoded, usize::MAX).ok()?;
        if option.end != decoded.len() {
            return None;
        }
        mask |= selection_option_prefilter_bit(option.item);
        remaining = &remaining[value.end..];
        if remaining.is_empty() {
            break;
        }
        remaining = remaining.strip_prefix(b" ")?;
    }
    Some(mask)
}

#[derive(Clone, Debug)]
struct ParsedListResponse {
    attributes: Range<usize>,
    delimiter_wire: Range<usize>,
    delimiter: Option<u8>,
    mailbox: Range<usize>,
    extended: Option<Range<usize>>,
    extension_depth: usize,
}

fn parse_list_response(
    input: &[u8],
    name: &[u8],
    max_depth: usize,
) -> Result<ParsedListResponse, ProtocolError> {
    if input.len() <= name.len() || !eq_ascii(&input[..name.len()], name) {
        return Err(invalid("IMAP LIST response name"));
    }
    let mut cursor = require_space(input, name.len(), "IMAP LIST response separator")?;
    let attributes_start = cursor;
    let attributes_end = parenthesized_end(&input[cursor..])
        .ok_or_else(|| invalid("IMAP LIST response attributes").at(cursor))?
        + cursor;
    validate_attributes(&input[cursor..attributes_end])?;
    cursor = attributes_end;
    let attributes = attributes_start..cursor;

    cursor = require_space(input, cursor, "IMAP LIST delimiter separator")?;
    let delimiter_start = cursor;
    let (delimiter, delimiter_end) = parse_delimiter(input, cursor)?;
    cursor = delimiter_end;
    let delimiter_wire = delimiter_start..cursor;

    cursor = require_space(input, cursor, "IMAP LIST mailbox separator")?;
    let mailbox_start = cursor;
    let mailbox =
        parse_astring_prefix(&input[cursor..]).map_err(|error| shift_error(error, cursor))?;
    if matches!(
        mailbox.kind,
        AStringKind::Literal {
            non_synchronizing: true
        }
    ) {
        return Err(invalid("non-synchronizing IMAP LIST response literal").at(cursor));
    }
    cursor += mailbox.end;
    let mailbox_range = mailbox_start..cursor;

    let mut extension_depth = 0;
    let extended = if cursor == input.len() {
        None
    } else {
        cursor = require_space(input, cursor, "IMAP LIST extended-data separator")?;
        let start = cursor;
        let parsed = parse_extended_list(&input[cursor..], max_depth)
            .map_err(|error| shift_error(error, cursor))?;
        cursor += parsed.end;
        extension_depth = parsed.extension_depth;
        Some(start..cursor)
    };
    if cursor != input.len() {
        return Err(invalid("trailing IMAP LIST response data").at(cursor));
    }
    Ok(ParsedListResponse {
        attributes,
        delimiter_wire,
        delimiter,
        mailbox: mailbox_range,
        extended,
        extension_depth,
    })
}

fn parse_delimiter(input: &[u8], start: usize) -> Result<(Option<u8>, usize), ProtocolError> {
    if input
        .get(start..start + 3)
        .is_some_and(|value| eq_ascii(value, b"NIL"))
    {
        return Ok((None, start + 3));
    }
    let parsed =
        parse_astring_prefix(&input[start..]).map_err(|error| shift_error(error, start))?;
    if parsed.kind != AStringKind::Quoted {
        return Err(invalid("IMAP LIST hierarchy delimiter").at(start));
    }
    let content = &input[start + parsed.content.start..start + parsed.content.end];
    let decoded = decode_string_content(content, true);
    if decoded.len() != 1 {
        return Err(invalid("IMAP LIST hierarchy delimiter").at(start));
    }
    Ok((Some(decoded[0]), start + parsed.end))
}

fn validate_attributes(input: &[u8]) -> Result<(), ProtocolError> {
    if input.len() < 2 || input.first() != Some(&b'(') || input.last() != Some(&b')') {
        return Err(invalid("IMAP LIST response attributes"));
    }
    let content = &input[1..input.len() - 1];
    if content.is_empty() {
        return Ok(());
    }
    let mut cursor = 0;
    let mut known = 0u16;
    let mut selectability = false;
    let mut child = false;
    let mut extensions = HashSet::new();
    loop {
        let end = content[cursor..]
            .iter()
            .position(|byte| *byte == b' ')
            .map_or(content.len(), |offset| cursor + offset);
        let wire = &content[cursor..end];
        let attribute = parse_attribute(wire)?;
        let bit = attribute_bit(attribute);
        if bit != 0 {
            if known & bit != 0 {
                return Err(invalid("duplicate IMAP LIST attribute").at(cursor + 1));
            }
            known |= bit;
        } else if !extensions.insert(AsciiCaseless(wire)) {
            return Err(invalid("duplicate IMAP LIST attribute").at(cursor + 1));
        }
        if matches!(
            attribute,
            ListAttribute::NonExistent
                | ListAttribute::NoSelect
                | ListAttribute::Marked
                | ListAttribute::Unmarked
        ) {
            if selectability {
                return Err(
                    invalid("conflicting IMAP LIST selectability attributes").at(cursor + 1)
                );
            }
            selectability = true;
        }
        if matches!(
            attribute,
            ListAttribute::HasChildren | ListAttribute::HasNoChildren
        ) {
            if child {
                return Err(invalid("conflicting IMAP LIST child attributes").at(cursor + 1));
            }
            child = true;
        }
        if end == content.len() {
            break;
        }
        if content
            .get(end + 1)
            .is_none_or(|byte| matches!(byte, b' ' | b')'))
        {
            return Err(invalid("IMAP LIST attribute separator").at(end + 1));
        }
        cursor = end + 1;
    }
    Ok(())
}

fn parse_attribute(input: &[u8]) -> Result<ListAttribute<'_>, ProtocolError> {
    if input.len() < 2
        || input.first() != Some(&b'\\')
        || !input[1..].iter().all(|byte| is_atom_char(*byte))
    {
        return Err(invalid("IMAP LIST attribute"));
    }
    Ok(if eq_ascii(input, b"\\Noinferiors") {
        ListAttribute::NoInferiors
    } else if eq_ascii(input, b"\\HasChildren") {
        ListAttribute::HasChildren
    } else if eq_ascii(input, b"\\HasNoChildren") {
        ListAttribute::HasNoChildren
    } else if eq_ascii(input, b"\\Subscribed") {
        ListAttribute::Subscribed
    } else if eq_ascii(input, b"\\Remote") {
        ListAttribute::Remote
    } else if eq_ascii(input, b"\\NonExistent") {
        ListAttribute::NonExistent
    } else if eq_ascii(input, b"\\Noselect") {
        ListAttribute::NoSelect
    } else if eq_ascii(input, b"\\Marked") {
        ListAttribute::Marked
    } else if eq_ascii(input, b"\\Unmarked") {
        ListAttribute::Unmarked
    } else {
        ListAttribute::Other(input)
    })
}

const fn attribute_bit(attribute: ListAttribute<'_>) -> u16 {
    match attribute {
        ListAttribute::NoInferiors => 1 << 0,
        ListAttribute::HasChildren => 1 << 1,
        ListAttribute::HasNoChildren => 1 << 2,
        ListAttribute::Subscribed => 1 << 3,
        ListAttribute::Remote => 1 << 4,
        ListAttribute::NonExistent => 1 << 5,
        ListAttribute::NoSelect => 1 << 6,
        ListAttribute::Marked => 1 << 7,
        ListAttribute::Unmarked => 1 << 8,
        ListAttribute::Other(_) => 0,
    }
}

#[derive(Clone, Copy, Debug)]
struct ParsedExtendedList {
    end: usize,
    extension_depth: usize,
}

fn parse_extended_list(
    input: &[u8],
    max_depth: usize,
) -> Result<ParsedExtendedList, ProtocolError> {
    if input.first() != Some(&b'(') {
        return Err(invalid("IMAP LIST extended-data list"));
    }
    if input.get(1) == Some(&b')') {
        return Ok(ParsedExtendedList {
            end: 2,
            extension_depth: 0,
        });
    }
    let mut cursor = 1;
    let mut extension_depth = 0;
    loop {
        let parsed = parse_extended_item(&input[cursor..], max_depth)
            .map_err(|error| shift_error(error, cursor))?;
        cursor += parsed.end;
        extension_depth = extension_depth.max(parsed.extension_depth);
        match input.get(cursor) {
            Some(b')') => {
                cursor += 1;
                break;
            }
            Some(b' ')
                if input
                    .get(cursor + 1)
                    .is_some_and(|byte| *byte != b' ' && *byte != b')') =>
            {
                cursor += 1;
            }
            _ => return Err(invalid("IMAP LIST extended-data separator").at(cursor)),
        }
    }
    Ok(ParsedExtendedList {
        end: cursor,
        extension_depth,
    })
}

#[derive(Clone, Copy, Debug)]
struct ParsedExtendedItem<'a> {
    item: ListExtendedItem<'a>,
    end: usize,
    extension_depth: usize,
}

fn parse_extended_item(
    input: &[u8],
    max_depth: usize,
) -> Result<ParsedExtendedItem<'_>, ProtocolError> {
    let tag = parse_astring_prefix(input)?;
    if matches!(
        tag.kind,
        AStringKind::Literal {
            non_synchronizing: true
        }
    ) {
        return Err(invalid("non-synchronizing IMAP LIST extended tag"));
    }
    let tag_wire = &input[..tag.end];
    let tag_content = &input[tag.content.clone()];
    let decoded_tag = decode_string_content(tag_content, tag.kind == AStringKind::Quoted);
    validate_astring_atom(&decoded_tag)?;
    let value_start = require_space(input, tag.end, "IMAP LIST extended item separator")?;
    let parsed_value = parse_value(
        input,
        value_start,
        max_depth,
        LiteralPolicy::RejectNonSynchronizing,
    )?;
    let value = &input[value_start..parsed_value.end];
    let item = if decoded_tag.eq_ignore_ascii_case(b"CHILDINFO") {
        validate_child_info(value)?;
        ListExtendedItem::ChildInfo { options: value }
    } else if decoded_tag.eq_ignore_ascii_case(b"OLDNAME") {
        let mailbox = validate_old_name(value)?;
        ListExtendedItem::OldName { mailbox }
    } else {
        ListExtendedItem::Other {
            name: tag_wire,
            value,
        }
    };
    Ok(ParsedExtendedItem {
        item,
        end: parsed_value.end,
        extension_depth: parsed_value.nesting_depth,
    })
}

fn validate_child_info(input: &[u8]) -> Result<(), ProtocolError> {
    if input.len() < 4 || input.first() != Some(&b'(') || input.last() != Some(&b')') {
        return Err(invalid("IMAP LIST CHILDINFO"));
    }
    let mut cursor = 1;
    let end = input.len() - 1;
    loop {
        let parsed =
            parse_astring_prefix(&input[cursor..]).map_err(|error| shift_error(error, cursor))?;
        if parsed.kind != AStringKind::Quoted {
            return Err(invalid("IMAP LIST CHILDINFO selection option").at(cursor));
        }
        let content = &input[cursor + parsed.content.start..cursor + parsed.content.end];
        let decoded = decode_string_content(content, true);
        let option = parse_selection_option(&decoded, usize::MAX)?;
        if option.end != decoded.len()
            || matches!(
                option.item,
                ListSelectionOption::Remote | ListSelectionOption::RecursiveMatch
            )
        {
            return Err(invalid("IMAP LIST CHILDINFO base selection option").at(cursor));
        }
        cursor += parsed.end;
        if cursor == end {
            break;
        }
        cursor = require_space(input, cursor, "IMAP LIST CHILDINFO separator")?;
        if cursor >= end {
            return Err(invalid("IMAP LIST CHILDINFO separator").at(cursor));
        }
    }
    Ok(())
}

fn validate_old_name(input: &[u8]) -> Result<&[u8], ProtocolError> {
    if input.len() < 3 || input.first() != Some(&b'(') || input.last() != Some(&b')') {
        return Err(invalid("IMAP LIST OLDNAME"));
    }
    let mailbox = &input[1..input.len() - 1];
    let parsed = parse_astring_prefix(mailbox)?;
    if parsed.end != mailbox.len()
        || matches!(
            parsed.kind,
            AStringKind::Literal {
                non_synchronizing: true
            }
        )
    {
        return Err(invalid("IMAP LIST OLDNAME mailbox"));
    }
    Ok(mailbox)
}

#[derive(Clone, Copy)]
struct AsciiCaseless<'a>(&'a [u8]);

impl PartialEq for AsciiCaseless<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.0.eq_ignore_ascii_case(other.0)
    }
}

impl Eq for AsciiCaseless<'_> {}

impl Hash for AsciiCaseless<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        for byte in self.0 {
            state.write_u8(byte.to_ascii_uppercase());
        }
    }
}
