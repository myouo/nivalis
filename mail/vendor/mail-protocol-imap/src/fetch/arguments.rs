use std::ops::Range;

use bytes::Bytes;
use mail_protocol_core::wire::{eq_ascii, slice_ref as slice_for};
use mail_protocol_core::{ErrorKind, ProtocolError};

use crate::{
    Command, CommandBody, SequenceSetRef,
    tagged_ext::{LiteralPolicy, parse_value, validate_label},
};

use super::section::{FetchPartial, FetchSection, parse_optional_partial, parse_section};

/// Default maximum nesting accepted in generic FETCH modifier parameters.
pub const DEFAULT_FETCH_MAX_DEPTH: usize = 64;
pub(super) const MAX_NUMBER64: u64 = i64::MAX as u64;

/// A validated RFC 9051 FETCH item list and optional extension modifiers.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct FetchArguments {
    wire: Bytes,
    items: Range<usize>,
    modifiers: Option<Range<usize>>,
    form: FetchItemForm,
    extension_depth: usize,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum FetchItemForm {
    Macro(FetchMacro),
    Single,
    List,
}

impl FetchArguments {
    /// Parses FETCH items and optional extension modifiers without copying.
    ///
    /// This validates the complete RFC 9051 base item grammar. Generic
    /// modifiers use the extension-friendly tagged value grammar; known
    /// CHANGEDSINCE/VANISHED modifiers receive their RFC 7162 numeric and
    /// combination checks when parsed through a [`Command`].
    ///
    /// # Errors
    ///
    /// Returns an error for invalid macros, attributes, sections, partials,
    /// header field lists, modifiers, spacing, or nesting.
    pub fn parse(wire: &Bytes) -> Result<Self, ProtocolError> {
        Self::parse_with_max_depth(wire, DEFAULT_FETCH_MAX_DEPTH)
    }

    /// Parses FETCH arguments with an explicit extension nesting limit.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::parse`] and `NestingTooDeep` when a
    /// generic modifier parameter exceeds `max_depth`.
    pub fn parse_with_max_depth(wire: &Bytes, max_depth: usize) -> Result<Self, ProtocolError> {
        parse_owned(wire, max_depth, None)
    }

    /// Returns the exact validated wire value after the sequence-set.
    pub const fn as_bytes(&self) -> &Bytes {
        &self.wire
    }

    /// Returns the requested macro, or `None` for explicit attributes.
    pub const fn fetch_macro(&self) -> Option<FetchMacro> {
        match self.form {
            FetchItemForm::Macro(value) => Some(value),
            FetchItemForm::Single | FetchItemForm::List => None,
        }
    }

    /// Returns the exact macro, single attribute, or parenthesized item list.
    pub fn items_bytes(&self) -> &[u8] {
        &self.wire[self.items.clone()]
    }

    /// Iterates over requested attributes without allocating.
    ///
    /// A macro yields no attributes; use [`Self::fetch_macro`] to inspect it.
    pub fn attributes(&self) -> FetchAttributeIter<'_> {
        let remaining = match self.form {
            FetchItemForm::Macro(_) => b"".as_slice(),
            FetchItemForm::Single => &self.wire[self.items.clone()],
            FetchItemForm::List => &self.wire[self.items.start + 1..self.items.end - 1],
        };
        FetchAttributeIter { remaining }
    }

    /// Returns the exact parenthesized modifier list when present.
    pub fn modifiers_bytes(&self) -> Option<&[u8]> {
        self.modifiers
            .as_ref()
            .map(|range| &self.wire[range.clone()])
    }

    /// Iterates over extension modifiers without allocating.
    pub fn modifiers(&self) -> FetchModifierIter<'_> {
        let remaining = self.modifiers.as_ref().map_or(b"".as_slice(), |range| {
            &self.wire[range.start + 1..range.end - 1]
        });
        FetchModifierIter { remaining }
    }

    /// Returns the greatest generic modifier parameter depth observed.
    pub const fn extension_depth(&self) -> usize {
        self.extension_depth
    }
}

/// One RFC 9051 FETCH macro.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum FetchMacro {
    /// `(FLAGS INTERNALDATE RFC822.SIZE ENVELOPE)`.
    All,
    /// `(FLAGS INTERNALDATE RFC822.SIZE)`.
    Fast,
    /// `(FLAGS INTERNALDATE RFC822.SIZE ENVELOPE BODY)`.
    Full,
}

/// One validated FETCH request attribute.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum FetchAttribute<'a> {
    /// Message envelope.
    Envelope,
    /// Current message flags.
    Flags,
    /// Internal delivery date.
    InternalDate,
    /// RFC 5322 message size.
    Rfc822Size,
    /// Complete RFC 5322 message, as named by `IMAP4rev1`.
    Rfc822,
    /// RFC 5322 header, as named by `IMAP4rev1`.
    Rfc822Header,
    /// RFC 5322 body text, as named by `IMAP4rev1`.
    Rfc822Text,
    /// Non-extensible body structure.
    Body,
    /// Extensible body structure.
    BodyStructure,
    /// Message unique identifier.
    Uid,
    /// RFC 7162 per-message modification sequence.
    ModSeq,
    /// Message text or one MIME/message section.
    BodySection {
        /// Whether reading must avoid implicitly setting `\\Seen`.
        peek: bool,
        /// Validated section specifier.
        section: FetchSection<'a>,
        /// Optional 63-bit byte range.
        partial: Option<FetchPartial>,
    },
    /// Transfer-decoded leaf body section.
    Binary {
        /// Whether reading must avoid implicitly setting `\\Seen`.
        peek: bool,
        /// Empty or numeric-only section specifier.
        section: FetchSection<'a>,
        /// Optional range in decoded octets.
        partial: Option<FetchPartial>,
    },
    /// Transfer-decoded size of a leaf body section.
    BinarySize {
        /// Empty or numeric-only section specifier.
        section: FetchSection<'a>,
    },
    /// Extension attribute preserved as one validated atom.
    Other(&'a [u8]),
}

/// Allocation-free iterator over validated FETCH attributes.
#[derive(Clone, Debug)]
pub struct FetchAttributeIter<'a> {
    remaining: &'a [u8],
}

impl<'a> Iterator for FetchAttributeIter<'a> {
    type Item = FetchAttribute<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining.is_empty() {
            return None;
        }
        let parsed = match parse_attribute(self.remaining) {
            Ok(parsed) => parsed,
            Err(error) => {
                debug_assert!(false, "validated FETCH attribute became invalid: {error}");
                self.remaining = b"";
                return None;
            }
        };
        self.remaining = if parsed.end == self.remaining.len() {
            b""
        } else {
            self.remaining.get(parsed.end + 1..).unwrap_or_else(|| {
                debug_assert!(false, "validated FETCH separator disappeared");
                b""
            })
        };
        Some(parsed.attribute)
    }
}

/// One FETCH extension modifier.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum FetchModifier<'a> {
    /// RFC 7162 CHANGEDSINCE modification sequence.
    ChangedSince(u64),
    /// RFC 7162 UID FETCH VANISHED request.
    Vanished,
    /// Generic modifier with an optional tagged extension parameter.
    Other {
        /// Modifier label.
        name: &'a [u8],
        /// Exact optional parameter.
        parameters: Option<&'a [u8]>,
    },
}

/// Allocation-free iterator over validated FETCH modifiers.
#[derive(Clone, Debug)]
pub struct FetchModifierIter<'a> {
    remaining: &'a [u8],
}

impl<'a> Iterator for FetchModifierIter<'a> {
    type Item = FetchModifier<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining.is_empty() {
            return None;
        }
        let parsed = match parse_modifier(self.remaining, usize::MAX) {
            Ok(parsed) => parsed,
            Err(error) => {
                debug_assert!(false, "validated FETCH modifier became invalid: {error}");
                self.remaining = b"";
                return None;
            }
        };
        self.remaining = if parsed.end == self.remaining.len() {
            b""
        } else {
            self.remaining.get(parsed.end + 1..).unwrap_or_else(|| {
                debug_assert!(false, "validated FETCH modifier separator disappeared");
                b""
            })
        };
        Some(parsed.modifier)
    }
}

impl Command {
    /// Parses FETCH items for a direct FETCH or UID FETCH command.
    ///
    /// # Errors
    ///
    /// Returns an error if a manually constructed command contains an invalid
    /// sequence-set, FETCH argument, or context-invalid VANISHED modifier.
    pub fn parsed_fetch_arguments(&self) -> Result<Option<FetchArguments>, ProtocolError> {
        match &self.body {
            CommandBody::Fetch { items, .. } => {
                parse_owned(items, DEFAULT_FETCH_MAX_DEPTH, Some(false)).map(Some)
            }
            CommandBody::Uid { command, arguments } if eq_ascii(command, b"FETCH") => {
                let (sequence, items) = split_sequence_and_items(arguments)?;
                SequenceSetRef::parse(sequence)?;
                let items = slice_for(arguments, items);
                parse_owned(&items, DEFAULT_FETCH_MAX_DEPTH, Some(true)).map(Some)
            }
            _ => Ok(None),
        }
    }
}

pub(crate) fn validate_fetch_arguments(
    input: &[u8],
    max_depth: usize,
    uid: bool,
) -> Result<(), ProtocolError> {
    let parsed = parse_arguments(input, max_depth, Some(uid))?;
    if parsed.consumed == input.len() {
        Ok(())
    } else {
        Err(invalid("trailing IMAP FETCH arguments").at(parsed.consumed))
    }
}

pub(crate) fn validate_uid_fetch_arguments(
    input: &[u8],
    max_depth: usize,
) -> Result<(), ProtocolError> {
    let (sequence, items) = split_sequence_and_items(input)?;
    SequenceSetRef::parse(sequence)?;
    validate_fetch_arguments(items, max_depth, true)
}

pub(super) fn parse_owned(
    wire: &Bytes,
    max_depth: usize,
    uid: Option<bool>,
) -> Result<FetchArguments, ProtocolError> {
    let parsed = parse_arguments(wire, max_depth, uid)?;
    if parsed.consumed != wire.len() {
        return Err(invalid("trailing IMAP FETCH arguments").at(parsed.consumed));
    }
    Ok(FetchArguments {
        wire: wire.clone(),
        items: parsed.items,
        modifiers: parsed.modifiers,
        form: parsed.form,
        extension_depth: parsed.extension_depth,
    })
}

struct ParsedArguments {
    consumed: usize,
    items: Range<usize>,
    modifiers: Option<Range<usize>>,
    form: FetchItemForm,
    extension_depth: usize,
}

fn parse_arguments(
    input: &[u8],
    max_depth: usize,
    uid: Option<bool>,
) -> Result<ParsedArguments, ProtocolError> {
    if input.is_empty() {
        return Err(invalid("empty IMAP FETCH arguments"));
    }
    let parsed_items = parse_items(input)?;
    let items = 0..parsed_items.end;
    if parsed_items.end == input.len() {
        return Ok(ParsedArguments {
            consumed: parsed_items.end,
            items,
            modifiers: None,
            form: parsed_items.form,
            extension_depth: 0,
        });
    }
    if input.get(parsed_items.end) != Some(&b' ') || input.get(parsed_items.end + 1) != Some(&b'(')
    {
        return Err(invalid("IMAP FETCH modifier separator").at(parsed_items.end));
    }
    let modifier_start = parsed_items.end + 1;
    let modifiers = parse_modifier_list(&input[modifier_start..], max_depth, uid)?;
    let consumed = modifier_start + modifiers.consumed;
    Ok(ParsedArguments {
        consumed,
        items,
        modifiers: Some(modifier_start..consumed),
        form: parsed_items.form,
        extension_depth: modifiers.nesting_depth,
    })
}

struct ParsedItems {
    end: usize,
    form: FetchItemForm,
}

fn parse_items(input: &[u8]) -> Result<ParsedItems, ProtocolError> {
    if input.first() == Some(&b'(') {
        let end = parse_attribute_list(input)?;
        return Ok(ParsedItems {
            end,
            form: FetchItemForm::List,
        });
    }
    let token_end = atom_end(input);
    let token = &input[..token_end];
    if let Some(value) = parse_macro(token) {
        return Ok(ParsedItems {
            end: token_end,
            form: FetchItemForm::Macro(value),
        });
    }
    let attribute = parse_attribute(input)?;
    Ok(ParsedItems {
        end: attribute.end,
        form: FetchItemForm::Single,
    })
}

fn parse_attribute_list(input: &[u8]) -> Result<usize, ProtocolError> {
    if input.get(1).is_none_or(|byte| matches!(byte, b' ' | b')')) {
        return Err(invalid("empty IMAP FETCH attribute list").at(1));
    }
    let mut cursor = 1usize;
    loop {
        let parsed =
            parse_attribute(&input[cursor..]).map_err(|error| shift_error(error, cursor))?;
        cursor += parsed.end;
        match input.get(cursor) {
            Some(b')') => return Ok(cursor + 1),
            Some(b' ')
                if input
                    .get(cursor + 1)
                    .is_some_and(|byte| !matches!(byte, b' ' | b')')) =>
            {
                cursor += 1;
            }
            None => return Err(invalid("unterminated IMAP FETCH attribute list").at(cursor)),
            _ => return Err(invalid("IMAP FETCH attribute separator").at(cursor)),
        }
    }
}

struct ParsedAttribute<'a> {
    attribute: FetchAttribute<'a>,
    end: usize,
}

fn parse_attribute(input: &[u8]) -> Result<ParsedAttribute<'_>, ProtocolError> {
    if input
        .first()
        .is_some_and(|byte| byte.eq_ignore_ascii_case(&b'B'))
    {
        if starts_ascii(input, b"BODY.PEEK[") {
            return parse_body_section(input, true, b"BODY.PEEK".len());
        }
        if starts_ascii(input, b"BODY[") {
            return parse_body_section(input, false, b"BODY".len());
        }
        if starts_ascii(input, b"BINARY.PEEK[") {
            return parse_binary(input, true, b"BINARY.PEEK".len());
        }
        if starts_ascii(input, b"BINARY.SIZE[") {
            let parsed = parse_section(input, b"BINARY.SIZE".len(), true)?;
            return Ok(ParsedAttribute {
                attribute: FetchAttribute::BinarySize {
                    section: parsed.section,
                },
                end: parsed.end,
            });
        }
        if starts_ascii(input, b"BINARY[") {
            return parse_binary(input, false, b"BINARY".len());
        }
    }

    let end = atom_end(input);
    let token = &input[..end];
    if token.is_empty() {
        return Err(invalid("empty IMAP FETCH attribute"));
    }
    let attribute = if eq_ascii(token, b"ENVELOPE") {
        FetchAttribute::Envelope
    } else if eq_ascii(token, b"FLAGS") {
        FetchAttribute::Flags
    } else if eq_ascii(token, b"INTERNALDATE") {
        FetchAttribute::InternalDate
    } else if eq_ascii(token, b"RFC822.SIZE") {
        FetchAttribute::Rfc822Size
    } else if eq_ascii(token, b"BODY") {
        FetchAttribute::Body
    } else if eq_ascii(token, b"BODYSTRUCTURE") {
        FetchAttribute::BodyStructure
    } else if eq_ascii(token, b"UID") {
        FetchAttribute::Uid
    } else if eq_ascii(token, b"MODSEQ") {
        FetchAttribute::ModSeq
    } else if eq_ascii(token, b"RFC822") {
        FetchAttribute::Rfc822
    } else if eq_ascii(token, b"RFC822.HEADER") {
        FetchAttribute::Rfc822Header
    } else if eq_ascii(token, b"RFC822.TEXT") {
        FetchAttribute::Rfc822Text
    } else {
        if parse_macro(token).is_some()
            || starts_ascii(token, b"BODY")
            || starts_ascii(token, b"BINARY")
        {
            return Err(invalid("IMAP FETCH attribute").at(0));
        }
        validate_atom(token, "IMAP FETCH extension attribute")?;
        FetchAttribute::Other(token)
    };
    Ok(ParsedAttribute { attribute, end })
}

fn parse_body_section(
    input: &[u8],
    peek: bool,
    section_start: usize,
) -> Result<ParsedAttribute<'_>, ProtocolError> {
    let parsed = parse_section(input, section_start, false)?;
    let (partial, end) = parse_optional_partial(input, parsed.end)?;
    Ok(ParsedAttribute {
        attribute: FetchAttribute::BodySection {
            peek,
            section: parsed.section,
            partial,
        },
        end,
    })
}

fn parse_binary(
    input: &[u8],
    peek: bool,
    section_start: usize,
) -> Result<ParsedAttribute<'_>, ProtocolError> {
    let parsed = parse_section(input, section_start, true)?;
    let (partial, end) = parse_optional_partial(input, parsed.end)?;
    Ok(ParsedAttribute {
        attribute: FetchAttribute::Binary {
            peek,
            section: parsed.section,
            partial,
        },
        end,
    })
}

struct ParsedModifierList {
    consumed: usize,
    nesting_depth: usize,
}

fn parse_modifier_list(
    input: &[u8],
    max_depth: usize,
    uid: Option<bool>,
) -> Result<ParsedModifierList, ProtocolError> {
    if input.first() != Some(&b'(') || input.get(1).is_none_or(|byte| matches!(byte, b' ' | b')')) {
        return Err(invalid("empty IMAP FETCH modifier list"));
    }
    let mut cursor = 1usize;
    let mut nesting_depth = 0usize;
    let mut changed_since = false;
    let mut vanished = false;
    loop {
        let parsed = parse_modifier(&input[cursor..], max_depth)
            .map_err(|error| shift_error(error, cursor))?;
        nesting_depth = nesting_depth.max(parsed.nesting_depth);
        match parsed.modifier {
            FetchModifier::ChangedSince(_) => {
                if changed_since {
                    return Err(invalid("duplicate IMAP CHANGEDSINCE modifier").at(cursor));
                }
                changed_since = true;
            }
            FetchModifier::Vanished => {
                if vanished {
                    return Err(invalid("duplicate IMAP VANISHED modifier").at(cursor));
                }
                vanished = true;
            }
            FetchModifier::Other { .. } => {}
        }
        cursor += parsed.end;
        match input.get(cursor) {
            Some(b')') => {
                if vanished {
                    if uid == Some(false) {
                        return Err(invalid("VANISHED requires UID FETCH").at(cursor));
                    }
                    if uid.is_some() && !changed_since {
                        return Err(invalid("VANISHED requires CHANGEDSINCE").at(cursor));
                    }
                }
                return Ok(ParsedModifierList {
                    consumed: cursor + 1,
                    nesting_depth,
                });
            }
            Some(b' ')
                if input
                    .get(cursor + 1)
                    .is_some_and(|byte| !matches!(byte, b' ' | b')')) =>
            {
                cursor += 1;
            }
            None => return Err(invalid("unterminated IMAP FETCH modifier list").at(cursor)),
            _ => return Err(invalid("IMAP FETCH modifier separator").at(cursor)),
        }
    }
}

struct ParsedModifier<'a> {
    modifier: FetchModifier<'a>,
    end: usize,
    nesting_depth: usize,
}

fn parse_modifier(input: &[u8], max_depth: usize) -> Result<ParsedModifier<'_>, ProtocolError> {
    let name_end = input
        .iter()
        .position(|byte| matches!(byte, b' ' | b')'))
        .unwrap_or(input.len());
    let name = &input[..name_end];
    if eq_ascii(name, b"CHANGEDSINCE") {
        if input.get(name_end) != Some(&b' ') {
            return Err(invalid("IMAP CHANGEDSINCE value").at(name_end));
        }
        let value_start = name_end + 1;
        let value_end = input[value_start..]
            .iter()
            .position(|byte| matches!(byte, b' ' | b')'))
            .map_or(input.len(), |offset| value_start + offset);
        let value = parse_number(&input[value_start..value_end], MAX_NUMBER64, true, false)?;
        return Ok(ParsedModifier {
            modifier: FetchModifier::ChangedSince(value),
            end: value_end,
            nesting_depth: 0,
        });
    }
    if eq_ascii(name, b"VANISHED") {
        return Ok(ParsedModifier {
            modifier: FetchModifier::Vanished,
            end: name_end,
            nesting_depth: 0,
        });
    }
    validate_label(name)?;
    let mut end = name_end;
    let mut parameters = None;
    let mut nesting_depth = 0usize;
    if input.get(end) == Some(&b' ')
        && input.get(end + 1).is_some_and(|byte| {
            *byte == b'(' || byte.is_ascii_digit() || matches!(byte, b'*' | b'$')
        })
    {
        let value_start = end + 1;
        let value = parse_value(
            input,
            value_start,
            max_depth,
            LiteralPolicy::AllowNonSynchronizing,
        )?;
        end = value.end;
        parameters = Some(&input[value_start..end]);
        nesting_depth = value.nesting_depth;
    }
    Ok(ParsedModifier {
        modifier: FetchModifier::Other { name, parameters },
        end,
        nesting_depth,
    })
}

pub(crate) fn split_sequence_and_items(input: &[u8]) -> Result<(&[u8], &[u8]), ProtocolError> {
    let end = input
        .iter()
        .position(|byte| *byte == b' ')
        .ok_or_else(|| invalid("IMAP FETCH arguments"))?;
    let sequence = &input[..end];
    if input.get(end + 1).is_none() || input.get(end + 1) == Some(&b' ') {
        return Err(invalid("IMAP FETCH separator").at(end));
    }
    Ok((sequence, &input[end + 1..]))
}

fn parse_macro(input: &[u8]) -> Option<FetchMacro> {
    if eq_ascii(input, b"ALL") {
        Some(FetchMacro::All)
    } else if eq_ascii(input, b"FAST") {
        Some(FetchMacro::Fast)
    } else if eq_ascii(input, b"FULL") {
        Some(FetchMacro::Full)
    } else {
        None
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

fn validate_atom(input: &[u8], context: &'static str) -> Result<(), ProtocolError> {
    if input.is_empty()
        || input.iter().any(|byte| {
            !byte.is_ascii()
                || byte.is_ascii_control()
                || matches!(
                    byte,
                    b' ' | b'(' | b')' | b'{' | b'%' | b'*' | b'"' | b'\\' | b']'
                )
        })
    {
        Err(ProtocolError::new(ErrorKind::InvalidSyntax, context))
    } else {
        Ok(())
    }
}

fn atom_end(input: &[u8]) -> usize {
    input
        .iter()
        .position(|byte| matches!(byte, b' ' | b')'))
        .unwrap_or(input.len())
}

fn starts_ascii(input: &[u8], expected: &[u8]) -> bool {
    input
        .get(..expected.len())
        .is_some_and(|prefix| eq_ascii(prefix, expected))
}

fn shift_error(error: ProtocolError, start: usize) -> ProtocolError {
    error.at(start.saturating_add(error.offset().unwrap_or(0)))
}

const fn invalid(context: &'static str) -> ProtocolError {
    ProtocolError::new(ErrorKind::InvalidSyntax, context)
}
