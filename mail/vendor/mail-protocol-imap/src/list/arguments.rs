use std::ops::Range;

use bytes::Bytes;
use mail_protocol_core::ProtocolError;
use mail_protocol_core::wire::eq_ascii;

use crate::astring::{AStringKind, parse_astring_prefix};
use crate::{AString, Command, CommandBody};

use super::DEFAULT_LIST_MAX_DEPTH;
use super::mailbox::{ListMailbox, ListMailboxIter, parse_list_mailbox_prefix};
use super::options::{
    ListReturnOptionIter, ListSelectionOption, ListSelectionOptionIter, parse_return_options,
    parse_selection_option, parse_selection_options, selection_option_prefilter_bit,
    selection_options_equal,
};
use super::response::{ListExtendedItem, ListResponse};
use super::validation::{
    decode_string_content, invalid, list_interior, require_space, shift_error,
    validate_astring_atom,
};

/// Validated, zero-copy RFC 9051 and RFC 5258 LIST command arguments.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ListArguments {
    wire: Bytes,
    selection_options: Option<Range<usize>>,
    reference: AString,
    pattern: ListMailbox,
    patterns: Range<usize>,
    parenthesized_pattern: bool,
    return_options: Option<Range<usize>>,
    extension_depth: usize,
}

impl ListArguments {
    /// Parses LIST arguments with the default 64-level extension limit.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed selection/return options, invalid mailbox
    /// strings or patterns, invalid STATUS options, or illegal spacing.
    pub fn parse(wire: &Bytes) -> Result<Self, ProtocolError> {
        Self::parse_with_max_depth(wire, DEFAULT_LIST_MAX_DEPTH)
    }

    /// Parses LIST arguments with an explicit extension nesting limit.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::parse`] and `NestingTooDeep` when an
    /// extension option value exceeds `max_depth`.
    pub fn parse_with_max_depth(wire: &Bytes, max_depth: usize) -> Result<Self, ProtocolError> {
        let parsed = parse_list_arguments(wire, max_depth)?;
        let reference_wire = wire.slice(parsed.reference.clone());
        let pattern_wire = wire.slice(parsed.pattern.clone());
        let patterns = parsed.pattern.start..parsed.patterns_end;
        Ok(Self {
            wire: wire.clone(),
            selection_options: parsed.selection_options,
            reference: AString::parse(&reference_wire)?,
            pattern: ListMailbox::parse(&pattern_wire)?,
            patterns,
            parenthesized_pattern: parsed.parenthesized_pattern,
            return_options: parsed.return_options,
            extension_depth: parsed.extension_depth,
        })
    }

    /// Returns the complete bytes following the LIST command name.
    pub const fn as_bytes(&self) -> &Bytes {
        &self.wire
    }

    /// Returns the validated reference mailbox.
    pub const fn reference(&self) -> &AString {
        &self.reference
    }

    /// Returns the first mailbox pattern.
    ///
    /// This preserves the original single-pattern API. Use [`Self::patterns`]
    /// to inspect every RFC 5258 pattern in a parenthesized pattern list.
    pub const fn pattern(&self) -> &ListMailbox {
        &self.pattern
    }

    /// Iterates over one or more mailbox patterns without payload allocation.
    pub fn patterns(&self) -> ListMailboxIter<'_> {
        ListMailboxIter::new(&self.wire, self.patterns.clone())
    }

    /// Returns whether the pattern used the extended parenthesized form.
    pub const fn has_parenthesized_pattern(&self) -> bool {
        self.parenthesized_pattern
    }

    /// Returns the exact optional parenthesized selection-option list.
    pub fn selection_options(&self) -> Option<&[u8]> {
        self.selection_options
            .as_ref()
            .map(|range| &self.wire[range.clone()])
    }

    /// Iterates over selection options without allocating.
    pub fn selection_option_items(&self) -> ListSelectionOptionIter<'_> {
        let remaining = list_interior(&self.wire, self.selection_options.as_ref());
        ListSelectionOptionIter { remaining }
    }

    /// Returns whether this request enables recursive matching.
    pub fn has_recursive_match(&self) -> bool {
        self.selection_option_items()
            .any(|option| option == ListSelectionOption::RecursiveMatch)
    }

    /// Returns the exact optional parenthesized return-option list.
    pub fn return_options(&self) -> Option<&[u8]> {
        self.return_options
            .as_ref()
            .map(|range| &self.wire[range.clone()])
    }

    /// Iterates over return options without allocating.
    pub fn return_option_items(&self) -> ListReturnOptionIter<'_> {
        let remaining = list_interior(&self.wire, self.return_options.as_ref());
        ListReturnOptionIter { remaining }
    }

    /// Returns the greatest LIST extension-value nesting depth observed.
    pub const fn extension_depth(&self) -> usize {
        self.extension_depth
    }

    pub(crate) fn correlates_child_info(&self, response: &ListResponse) -> bool {
        if !self.has_recursive_match() {
            return false;
        }
        let mut found = false;
        for item in response.extended_items() {
            if let ListExtendedItem::ChildInfo { options } = item {
                found = true;
                if !self.matches_child_info_options(options) {
                    return false;
                }
            }
        }
        found
    }

    pub(crate) fn child_info_prefilter(&self) -> u64 {
        self.selection_option_items().fold(0, |mask, option| {
            if matches!(
                option,
                ListSelectionOption::Remote | ListSelectionOption::RecursiveMatch
            ) {
                mask
            } else {
                mask | selection_option_prefilter_bit(option)
            }
        })
    }

    fn matches_child_info_options(&self, options: &[u8]) -> bool {
        let Some(mut remaining) = options
            .strip_prefix(b"(")
            .and_then(|value| value.strip_suffix(b")"))
        else {
            return false;
        };
        while !remaining.is_empty() {
            let Ok(value) = parse_astring_prefix(remaining) else {
                return false;
            };
            if value.kind != AStringKind::Quoted {
                return false;
            }
            let decoded = decode_string_content(&remaining[value.content], true);
            let Ok(child_option) = parse_selection_option(&decoded, usize::MAX) else {
                return false;
            };
            if child_option.end != decoded.len()
                || !self.selection_option_items().any(|request_option| {
                    selection_options_equal(request_option, child_option.item)
                })
            {
                return false;
            }
            remaining = &remaining[value.end..];
            if remaining.is_empty() {
                break;
            }
            let Some(tail) = remaining.strip_prefix(b" ") else {
                return false;
            };
            remaining = tail;
        }
        true
    }
}

impl Command {
    /// Parses typed LIST arguments when this is a LIST command.
    ///
    /// # Errors
    ///
    /// Returns an error if a manually constructed LIST command is invalid.
    pub fn parsed_list_arguments(&self) -> Result<Option<ListArguments>, ProtocolError> {
        match &self.body {
            CommandBody::List { arguments } => ListArguments::parse(arguments).map(Some),
            CommandBody::Raw { name, arguments } if eq_ascii(name, b"LIST") => {
                ListArguments::parse(arguments).map(Some)
            }
            _ => Ok(None),
        }
    }
}

#[inline]
pub(crate) fn validate_list_arguments(input: &[u8], max_depth: usize) -> Result<(), ProtocolError> {
    parse_list_arguments(input, max_depth).map(|_| ())
}

#[derive(Clone, Debug)]
struct ParsedListArguments {
    selection_options: Option<Range<usize>>,
    reference: Range<usize>,
    pattern: Range<usize>,
    patterns_end: usize,
    parenthesized_pattern: bool,
    return_options: Option<Range<usize>>,
    extension_depth: usize,
}

#[inline]
fn parse_list_arguments(
    input: &[u8],
    max_depth: usize,
) -> Result<ParsedListArguments, ProtocolError> {
    if input.is_empty() {
        return Err(invalid("empty IMAP LIST arguments"));
    }
    let mut cursor = 0;
    let mut extension_depth = 0;
    let selection_options = if input.first() == Some(&b'(') {
        let parsed = parse_selection_options(input, max_depth)?;
        cursor = parsed.end;
        extension_depth = extension_depth.max(parsed.extension_depth);
        Some(0..cursor)
    } else {
        None
    };
    if selection_options.is_some() {
        cursor = require_space(input, cursor, "IMAP LIST selection separator")?;
    }

    let reference_start = cursor;
    let reference =
        parse_astring_prefix(&input[cursor..]).map_err(|error| shift_error(error, cursor))?;
    cursor += reference.end;
    let reference_range = reference_start..cursor;
    cursor = require_space(input, cursor, "IMAP LIST reference separator")?;

    let parenthesized_pattern = input.get(cursor) == Some(&b'(');
    let (first_pattern_range, patterns_end) = if parenthesized_pattern {
        cursor += 1;
        if input
            .get(cursor)
            .is_none_or(|byte| matches!(byte, b' ' | b')'))
        {
            return Err(invalid("empty IMAP LIST pattern list").at(cursor));
        }
        let first_start = cursor;
        let mut first_range = None;
        loop {
            let pattern = parse_list_mailbox_prefix(&input[cursor..])
                .map_err(|error| shift_error(error, cursor))?;
            cursor += pattern.end;
            first_range.get_or_insert(first_start..cursor);
            match input.get(cursor) {
                Some(b')') => break,
                Some(b' ')
                    if input
                        .get(cursor + 1)
                        .is_some_and(|byte| !matches!(byte, b' ' | b')')) =>
                {
                    cursor += 1;
                }
                _ => return Err(invalid("IMAP LIST pattern separator").at(cursor)),
            }
        }
        let patterns_end = cursor;
        cursor += 1;
        (
            first_range.expect("non-empty LIST pattern list stores first pattern"),
            patterns_end,
        )
    } else {
        let start = cursor;
        let pattern = parse_list_mailbox_prefix(&input[cursor..])
            .map_err(|error| shift_error(error, cursor))?;
        cursor += pattern.end;
        (start..cursor, cursor)
    };

    let return_options = if cursor == input.len() {
        None
    } else {
        cursor = require_space(input, cursor, "IMAP LIST return separator")?;
        let (name, after_name) = take_atom_token(input, cursor)?;
        if !eq_ascii(name, b"RETURN") {
            return Err(invalid("IMAP LIST RETURN keyword").at(cursor));
        }
        cursor = require_space(input, after_name, "IMAP LIST RETURN separator")?;
        let start = cursor;
        let parsed = parse_return_options(&input[cursor..], max_depth)
            .map_err(|error| shift_error(error, cursor))?;
        cursor += parsed.end;
        extension_depth = extension_depth.max(parsed.extension_depth);
        Some(start..cursor)
    };
    if cursor != input.len() {
        return Err(invalid("trailing IMAP LIST arguments").at(cursor));
    }

    Ok(ParsedListArguments {
        selection_options,
        reference: reference_range,
        pattern: first_pattern_range,
        patterns_end,
        parenthesized_pattern,
        return_options,
        extension_depth,
    })
}

fn take_atom_token(input: &[u8], start: usize) -> Result<(&[u8], usize), ProtocolError> {
    let end = input[start..]
        .iter()
        .position(|byte| *byte == b' ')
        .map_or(input.len(), |offset| start + offset);
    let token = &input[start..end];
    validate_astring_atom(token)?;
    Ok((token, end))
}
