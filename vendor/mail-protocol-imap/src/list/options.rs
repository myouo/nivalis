use mail_protocol_core::ProtocolError;
use mail_protocol_core::wire::eq_ascii;

use crate::astring::parse_astring_prefix;
use crate::status_items::validate_status_items;

use super::validation::{
    invalid, nesting_too_deep, next_list_item, parenthesized_end, require_space, shift_error,
    validate_astring_atom,
};

/// One RFC 9051 LIST selection option.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum ListSelectionOption<'a> {
    Subscribed,
    Remote,
    RecursiveMatch,
    Other {
        name: &'a [u8],
        parameters: Option<&'a [u8]>,
    },
}

/// Allocation-free iterator over validated LIST selection options.
#[derive(Clone, Debug)]
pub struct ListSelectionOptionIter<'a> {
    pub(super) remaining: &'a [u8],
}

impl<'a> Iterator for ListSelectionOptionIter<'a> {
    type Item = ListSelectionOption<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining.is_empty() {
            return None;
        }
        let parsed = match parse_selection_option(self.remaining, usize::MAX) {
            Ok(parsed) => parsed,
            Err(error) => {
                debug_assert!(
                    false,
                    "validated LIST selection option became invalid: {error}"
                );
                self.remaining = &[];
                return None;
            }
        };
        self.remaining = next_list_item(self.remaining, parsed.end)?;
        Some(parsed.item)
    }
}

/// One RFC 9051 LIST return option.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum ListReturnOption<'a> {
    Subscribed,
    Children,
    Status {
        items: &'a [u8],
    },
    Other {
        name: &'a [u8],
        parameters: Option<&'a [u8]>,
    },
}

/// Allocation-free iterator over validated LIST return options.
#[derive(Clone, Debug)]
pub struct ListReturnOptionIter<'a> {
    pub(super) remaining: &'a [u8],
}

impl<'a> Iterator for ListReturnOptionIter<'a> {
    type Item = ListReturnOption<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining.is_empty() {
            return None;
        }
        let parsed = match parse_return_option(self.remaining, usize::MAX) {
            Ok(parsed) => parsed,
            Err(error) => {
                debug_assert!(
                    false,
                    "validated LIST return option became invalid: {error}"
                );
                self.remaining = &[];
                return None;
            }
        };
        self.remaining = next_list_item(self.remaining, parsed.end)?;
        Some(parsed.item)
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct ParsedOptionList {
    pub(super) end: usize,
    pub(super) extension_depth: usize,
}

#[inline]
pub(super) fn parse_selection_options(
    input: &[u8],
    max_depth: usize,
) -> Result<ParsedOptionList, ProtocolError> {
    parse_option_list(input, max_depth, true)
}

#[inline]
pub(super) fn parse_return_options(
    input: &[u8],
    max_depth: usize,
) -> Result<ParsedOptionList, ProtocolError> {
    parse_option_list(input, max_depth, false)
}

#[inline]
fn parse_option_list(
    input: &[u8],
    max_depth: usize,
    selection: bool,
) -> Result<ParsedOptionList, ProtocolError> {
    if input.first() != Some(&b'(') {
        return Err(invalid("IMAP LIST option list"));
    }
    if input.get(1) == Some(&b')') {
        return Ok(ParsedOptionList {
            end: 2,
            extension_depth: 0,
        });
    }
    let mut cursor = 1;
    let mut extension_depth = 0;
    let mut subscribed = false;
    let mut recursive = false;
    let mut extension_base_candidate = false;
    loop {
        let parsed = if selection {
            let parsed = parse_selection_option(&input[cursor..], max_depth)
                .map_err(|error| shift_error(error, cursor))?;
            subscribed |= matches!(parsed.item, ListSelectionOption::Subscribed);
            recursive |= matches!(parsed.item, ListSelectionOption::RecursiveMatch);
            extension_base_candidate |= matches!(parsed.item, ListSelectionOption::Other { .. });
            ParsedAnyOption {
                end: parsed.end,
                extension_depth: parsed.extension_depth,
            }
        } else {
            let parsed = parse_return_option(&input[cursor..], max_depth)
                .map_err(|error| shift_error(error, cursor))?;
            ParsedAnyOption {
                end: parsed.end,
                extension_depth: parsed.extension_depth,
            }
        };
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
            _ => return Err(invalid("IMAP LIST option separator").at(cursor)),
        }
    }
    if selection && recursive && !subscribed && !extension_base_candidate {
        return Err(invalid("IMAP LIST RECURSIVEMATCH requires a base option"));
    }
    Ok(ParsedOptionList {
        end: cursor,
        extension_depth,
    })
}

#[derive(Clone, Copy, Debug)]
struct ParsedAnyOption {
    end: usize,
    extension_depth: usize,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct ParsedSelectionOption<'a> {
    pub(super) item: ListSelectionOption<'a>,
    pub(super) end: usize,
    extension_depth: usize,
}

#[inline]
pub(super) fn parse_selection_option(
    input: &[u8],
    max_depth: usize,
) -> Result<ParsedSelectionOption<'_>, ProtocolError> {
    let (name, name_end) = take_option_name(input)?;
    if eq_ascii(name, b"SUBSCRIBED") {
        return Ok(ParsedSelectionOption {
            item: ListSelectionOption::Subscribed,
            end: name_end,
            extension_depth: 0,
        });
    }
    if eq_ascii(name, b"REMOTE") {
        return Ok(ParsedSelectionOption {
            item: ListSelectionOption::Remote,
            end: name_end,
            extension_depth: 0,
        });
    }
    if eq_ascii(name, b"RECURSIVEMATCH") {
        return Ok(ParsedSelectionOption {
            item: ListSelectionOption::RecursiveMatch,
            end: name_end,
            extension_depth: 0,
        });
    }
    let (parameters, end, extension_depth) =
        parse_optional_option_value(input, name_end, max_depth)?;
    Ok(ParsedSelectionOption {
        item: ListSelectionOption::Other { name, parameters },
        end,
        extension_depth,
    })
}

#[derive(Clone, Copy, Debug)]
struct ParsedReturnOption<'a> {
    item: ListReturnOption<'a>,
    end: usize,
    extension_depth: usize,
}

#[inline]
fn parse_return_option(
    input: &[u8],
    max_depth: usize,
) -> Result<ParsedReturnOption<'_>, ProtocolError> {
    let (name, name_end) = take_option_name(input)?;
    if eq_ascii(name, b"SUBSCRIBED") {
        return Ok(ParsedReturnOption {
            item: ListReturnOption::Subscribed,
            end: name_end,
            extension_depth: 0,
        });
    }
    if eq_ascii(name, b"CHILDREN") {
        return Ok(ParsedReturnOption {
            item: ListReturnOption::Children,
            end: name_end,
            extension_depth: 0,
        });
    }
    if eq_ascii(name, b"STATUS") {
        let value_start = require_space(input, name_end, "IMAP LIST STATUS separator")?;
        let end = parenthesized_end(&input[value_start..])
            .ok_or_else(|| invalid("IMAP LIST STATUS item list").at(value_start))?
            + value_start;
        let items = &input[value_start..end];
        validate_status_items(items).map_err(|error| shift_error(error, value_start))?;
        return Ok(ParsedReturnOption {
            item: ListReturnOption::Status { items },
            end,
            extension_depth: 0,
        });
    }
    let (parameters, end, extension_depth) =
        parse_optional_option_value(input, name_end, max_depth)?;
    Ok(ParsedReturnOption {
        item: ListReturnOption::Other { name, parameters },
        end,
        extension_depth,
    })
}

fn parse_optional_option_value(
    input: &[u8],
    name_end: usize,
    max_depth: usize,
) -> Result<(Option<&[u8]>, usize, usize), ProtocolError> {
    if input.get(name_end..name_end + 2) != Some(b" (") {
        return Ok((None, name_end, 0));
    }
    let start = name_end + 1;
    let parsed = parse_option_value(input, start, max_depth)?;
    Ok((Some(&input[start..parsed.end]), parsed.end, parsed.depth))
}

#[derive(Clone, Copy, Debug)]
struct ParsedOptionValue {
    end: usize,
    depth: usize,
}

fn parse_option_value(
    input: &[u8],
    start: usize,
    max_depth: usize,
) -> Result<ParsedOptionValue, ProtocolError> {
    if input.get(start) != Some(&b'(') {
        return Err(invalid("IMAP LIST option value").at(start));
    }
    let mut cursor = start;
    let mut depth = 0usize;
    let mut greatest_depth = 0usize;
    let mut expecting_component = true;
    loop {
        let Some(byte) = input.get(cursor) else {
            return Err(invalid("IMAP LIST option value terminator").at(cursor));
        };
        if expecting_component {
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
                }
                b' ' | b')' => return Err(invalid("IMAP LIST option value component").at(cursor)),
                _ => {
                    cursor = parse_option_component(input, cursor)?;
                    expecting_component = false;
                }
            }
        } else {
            match byte {
                b')' => {
                    cursor += 1;
                    depth -= 1;
                    if depth == 0 {
                        return Ok(ParsedOptionValue {
                            end: cursor,
                            depth: greatest_depth,
                        });
                    }
                }
                b' ' if input
                    .get(cursor + 1)
                    .is_some_and(|next| *next != b' ' && *next != b')') =>
                {
                    cursor += 1;
                    expecting_component = true;
                }
                _ => return Err(invalid("IMAP LIST option value separator").at(cursor)),
            }
        }
    }
}

fn parse_option_component(input: &[u8], start: usize) -> Result<usize, ProtocolError> {
    if matches!(input.get(start), Some(b'"' | b'{')) {
        let parsed =
            parse_astring_prefix(&input[start..]).map_err(|error| shift_error(error, start))?;
        return Ok(start + parsed.end);
    }
    let end = input[start..]
        .iter()
        .position(|byte| matches!(byte, b' ' | b')'))
        .map_or(input.len(), |offset| start + offset);
    let parsed = parse_astring_prefix(&input[start..end])?;
    if parsed.end != end - start {
        return Err(invalid("IMAP LIST option value component").at(start + parsed.end));
    }
    Ok(end)
}

#[inline]
fn take_option_name(input: &[u8]) -> Result<(&[u8], usize), ProtocolError> {
    let end = input
        .iter()
        .position(|byte| matches!(byte, b' ' | b')'))
        .unwrap_or(input.len());
    let name = &input[..end];
    validate_astring_atom(name)?;
    Ok((name, end))
}

pub(super) fn selection_options_equal(
    left: ListSelectionOption<'_>,
    right: ListSelectionOption<'_>,
) -> bool {
    match (left, right) {
        (ListSelectionOption::Subscribed, ListSelectionOption::Subscribed)
        | (ListSelectionOption::Remote, ListSelectionOption::Remote)
        | (ListSelectionOption::RecursiveMatch, ListSelectionOption::RecursiveMatch) => true,
        (
            ListSelectionOption::Other {
                name: left_name,
                parameters: left_parameters,
            },
            ListSelectionOption::Other {
                name: right_name,
                parameters: right_parameters,
            },
        ) => eq_ascii(left_name, right_name) && left_parameters == right_parameters,
        _ => false,
    }
}

pub(super) fn selection_option_prefilter_bit(option: ListSelectionOption<'_>) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let (name, parameters) = match option {
        ListSelectionOption::Subscribed => (b"SUBSCRIBED".as_slice(), None),
        ListSelectionOption::Remote => (b"REMOTE".as_slice(), None),
        ListSelectionOption::RecursiveMatch => (b"RECURSIVEMATCH".as_slice(), None),
        ListSelectionOption::Other { name, parameters } => (name, parameters),
    };
    let mut hash = OFFSET;
    for byte in name {
        hash ^= u64::from(byte.to_ascii_uppercase());
        hash = hash.wrapping_mul(PRIME);
    }
    hash ^= 0xff;
    hash = hash.wrapping_mul(PRIME);
    if let Some(parameters) = parameters {
        for byte in parameters {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(PRIME);
        }
    }
    1u64 << (hash & 63)
}
