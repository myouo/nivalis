use std::ops::Range;

use bytes::Bytes;
use mail_protocol_core::{ErrorKind, ProtocolError};

use crate::{
    AStringKind, Command, CommandBody,
    astring::parse_astring_prefix,
    tagged_ext::{LiteralPolicy, parse_value, validate_label},
};

pub(crate) const DEFAULT_MAX_DEPTH: usize = 64;
const USES_SEQUENCE_NUMBERS: u8 = 1 << 0;
const USES_SAVED_RESULT: u8 = 1 << 1;
const RETURN_SAVE: u8 = 1 << 0;
const RETURN_MIN: u8 = 1 << 1;
const RETURN_MAX: u8 = 1 << 2;
const RETURN_ALL: u8 = 1 << 3;
const RETURN_COUNT: u8 = 1 << 4;

/// Portion of a successful SEARCH result assigned to the `$` variable.
///
/// RFC 9051 saves all matches when `SAVE` is used alone or together with
/// `ALL`/`COUNT`. With only `MIN` and/or `MAX`, just those extrema are saved.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum SavedSearchScope {
    /// Every matching message.
    All,
    /// Only the lowest matching message number or UID.
    Minimum,
    /// Only the highest matching message number or UID.
    Maximum,
    /// Both the lowest and highest matching message number or UID.
    MinimumAndMaximum,
}

/// Protocol-visible change to the RFC 9051 `$` result variable.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum SavedSearchUpdate {
    /// The command or response does not change `$`.
    Unchanged,
    /// `$` becomes the empty sequence.
    Reset,
    /// A successful SEARCH replaces `$` with the indicated result portion.
    Replace(SavedSearchScope),
}

/// One RFC 9051 SEARCH RETURN option.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum SearchReturnOption<'a> {
    /// Return the lowest matching message number or UID.
    Min,
    /// Return the highest matching message number or UID.
    Max,
    /// Return all matching message numbers or UIDs.
    All,
    /// Return the number of matches.
    Count,
    /// Save the selected result portion in `$`.
    Save,
    /// Extension option with an optional generic tagged extension parameter.
    Other {
        /// Extension label.
        name: &'a [u8],
        /// Exact tagged-ext-val parameter, including parentheses or literal marker.
        parameters: Option<&'a [u8]>,
    },
}

/// Allocation-free iterator over validated SEARCH RETURN options.
#[derive(Clone, Debug)]
pub struct SearchReturnOptionIter<'a> {
    remaining: &'a [u8],
}

impl<'a> Iterator for SearchReturnOptionIter<'a> {
    type Item = SearchReturnOption<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining.is_empty() {
            return None;
        }
        let parsed = match parse_return_option(self.remaining, usize::MAX) {
            Ok(parsed) => parsed,
            Err(error) => {
                debug_assert!(
                    false,
                    "validated SEARCH RETURN option became invalid: {error}"
                );
                self.remaining = &[];
                return None;
            }
        };
        self.remaining = if parsed.end == self.remaining.len() {
            b""
        } else {
            self.remaining.get(parsed.end + 1..)?
        };
        Some(parsed.item)
    }
}

/// A validated RFC 9051 SEARCH program.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SearchProgram {
    wire: Bytes,
    return_options: Option<Range<usize>>,
    charset: Option<Range<usize>>,
    criteria: Range<usize>,
    nesting_depth: usize,
    return_extension_depth: usize,
    uses_sequence_numbers: bool,
    uses_saved_result: bool,
    saved_result_scope: Option<SavedSearchScope>,
}

impl SearchProgram {
    /// Parses a SEARCH program with the default 64-level nesting limit.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed base RFC 9051 grammar, normal literals
    /// containing NUL, numeric overflow, or excessive recursive nesting.
    pub fn parse(wire: &Bytes) -> Result<Self, ProtocolError> {
        Self::parse_with_max_depth(wire, DEFAULT_MAX_DEPTH)
    }

    /// Parses a SEARCH program with an explicit recursive nesting limit.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::parse`] and rejects a parenthesized,
    /// `NOT`, or `OR` key whose depth exceeds `max_depth`.
    pub fn parse_with_max_depth(wire: &Bytes, max_depth: usize) -> Result<Self, ProtocolError> {
        let parsed = parse_search_program(wire, max_depth)?;
        Ok(Self {
            wire: wire.clone(),
            return_options: parsed.return_options,
            charset: parsed.charset,
            criteria: parsed.criteria,
            nesting_depth: parsed.nesting_depth,
            return_extension_depth: parsed.return_extension_depth,
            uses_sequence_numbers: parsed.uses_sequence_numbers,
            uses_saved_result: parsed.uses_saved_result,
            saved_result_scope: parsed.saved_result_scope,
        })
    }

    /// Returns the complete bytes following the SEARCH command name.
    pub const fn as_bytes(&self) -> &Bytes {
        &self.wire
    }

    /// Returns the optional parenthesized RETURN option list.
    pub fn return_options(&self) -> Option<&[u8]> {
        self.return_options
            .as_ref()
            .map(|range| &self.wire[range.clone()])
    }

    /// Returns the optional charset atom.
    pub fn charset(&self) -> Option<&[u8]> {
        self.charset.as_ref().map(|range| &self.wire[range.clone()])
    }

    /// Returns the validated search criteria in wire form.
    pub fn criteria(&self) -> &[u8] {
        &self.wire[self.criteria.clone()]
    }

    /// Returns the greatest recursive key depth observed while parsing.
    pub const fn nesting_depth(&self) -> usize {
        self.nesting_depth
    }

    /// Returns the greatest tagged extension parameter depth in RETURN.
    pub const fn return_extension_depth(&self) -> usize {
        self.return_extension_depth
    }

    /// Iterates over typed RETURN options without allocating.
    ///
    /// A SEARCH without RETURN yields an empty iterator.
    pub fn return_option_items(&self) -> SearchReturnOptionIter<'_> {
        let remaining = self
            .return_options
            .as_ref()
            .map_or(b"".as_slice(), |range| {
                &self.wire[range.start + 1..range.end - 1]
            });
        SearchReturnOptionIter { remaining }
    }

    /// Returns whether any criterion contains a message sequence-number set.
    ///
    /// The `UID` search key contains UIDs and does not set this flag. Bare
    /// sequence-set criteria do, including when nested below `NOT`, `OR`, or a
    /// parenthesized group.
    pub const fn uses_sequence_numbers(&self) -> bool {
        self.uses_sequence_numbers
    }

    /// Returns whether any criterion reads the RFC 9051 `$` result variable.
    ///
    /// This includes both a bare `$` criterion and `UID $`. Flag keywords such
    /// as `$Junk` are unrelated and do not set this flag.
    pub const fn uses_saved_result(&self) -> bool {
        self.uses_saved_result
    }

    /// Returns what a successful `RETURN (SAVE ...)` writes to `$`.
    ///
    /// `None` means this SEARCH does not modify the saved-search variable.
    pub const fn saved_result_scope(&self) -> Option<SavedSearchScope> {
        self.saved_result_scope
    }
}

impl Command {
    /// Parses the SEARCH program when this is SEARCH or UID SEARCH.
    ///
    /// # Errors
    ///
    /// Returns an error if a manually constructed command is invalid.
    pub fn parsed_search_program(&self) -> Result<Option<SearchProgram>, ProtocolError> {
        match &self.body {
            CommandBody::Search { criteria } => SearchProgram::parse(criteria).map(Some),
            CommandBody::Uid { command, arguments } if command.eq_ignore_ascii_case(b"SEARCH") => {
                SearchProgram::parse(arguments).map(Some)
            }
            _ => Ok(None),
        }
    }
}

pub(crate) fn validate_search_program(input: &[u8], max_depth: usize) -> Result<(), ProtocolError> {
    parse_search_program(input, max_depth).map(|_| ())
}

pub(crate) fn search_metadata(input: &[u8]) -> SearchMetadata {
    parse_search_program(input, DEFAULT_MAX_DEPTH).map_or(SearchMetadata::CONSERVATIVE, |parsed| {
        SearchMetadata {
            uses_sequence_numbers: parsed.uses_sequence_numbers,
            uses_saved_result: parsed.uses_saved_result,
            saved_result_scope: parsed.saved_result_scope,
        }
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SearchMetadata {
    pub(crate) uses_sequence_numbers: bool,
    pub(crate) uses_saved_result: bool,
    pub(crate) saved_result_scope: Option<SavedSearchScope>,
}

impl SearchMetadata {
    const CONSERVATIVE: Self = Self {
        uses_sequence_numbers: true,
        uses_saved_result: true,
        saved_result_scope: None,
    };
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ParsedSearch {
    return_options: Option<Range<usize>>,
    charset: Option<Range<usize>>,
    criteria: Range<usize>,
    nesting_depth: usize,
    return_extension_depth: usize,
    uses_sequence_numbers: bool,
    uses_saved_result: bool,
    saved_result_scope: Option<SavedSearchScope>,
}

fn parse_search_program(input: &[u8], max_depth: usize) -> Result<ParsedSearch, ProtocolError> {
    if input.is_empty() {
        return Err(invalid("empty IMAP SEARCH program"));
    }
    let mut cursor = 0;
    let (return_options, saved_result_scope, return_extension_depth) =
        if starts_token(&input[cursor..], b"RETURN") {
            cursor += b"RETURN".len();
            cursor = required_space(input, cursor, "IMAP SEARCH RETURN separator")?;
            let options_start = cursor;
            let parsed = parse_return_options(&input[cursor..], max_depth)?;
            cursor += parsed.consumed;
            let range = options_start..cursor;
            cursor = required_space(input, cursor, "IMAP SEARCH program separator")?;
            (
                Some(range),
                parsed.saved_result_scope(),
                parsed.nesting_depth,
            )
        } else {
            (None, None, 0)
        };

    let charset = if starts_token(&input[cursor..], b"CHARSET") {
        cursor += b"CHARSET".len();
        cursor = required_space(input, cursor, "IMAP SEARCH CHARSET separator")?;
        let start = cursor;
        let parsed = parse_astring_prefix(&input[cursor..])?;
        if matches!(parsed.kind, AStringKind::Literal { .. }) {
            return Err(invalid("IMAP SEARCH charset"));
        }
        cursor += parsed.end;
        let range = start..cursor;
        cursor = required_space(input, cursor, "IMAP SEARCH criteria separator")?;
        Some(range)
    } else {
        None
    };

    let criteria_start = cursor;
    let mut observed_depth = 0;
    let mut usage = 0u8;
    let mut count = 0usize;
    while cursor < input.len() {
        let consumed = parse_search_key(
            &input[cursor..],
            0,
            max_depth,
            &mut observed_depth,
            &mut usage,
        )?;
        cursor += consumed;
        count += 1;
        if cursor == input.len() {
            break;
        }
        cursor = required_space(input, cursor, "IMAP SEARCH key separator")?;
    }
    if count == 0 {
        return Err(invalid("empty IMAP SEARCH criteria"));
    }
    Ok(ParsedSearch {
        return_options,
        charset,
        criteria: criteria_start..input.len(),
        nesting_depth: observed_depth,
        return_extension_depth,
        uses_sequence_numbers: usage & USES_SEQUENCE_NUMBERS != 0,
        uses_saved_result: usage & USES_SAVED_RESULT != 0,
        saved_result_scope,
    })
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ParsedReturnOptions {
    consumed: usize,
    flags: u8,
    nesting_depth: usize,
}

impl ParsedReturnOptions {
    const fn saved_result_scope(self) -> Option<SavedSearchScope> {
        let save = self.flags & RETURN_SAVE != 0;
        let min = self.flags & RETURN_MIN != 0;
        let max = self.flags & RETURN_MAX != 0;
        let all = self.flags & RETURN_ALL != 0;
        let count = self.flags & RETURN_COUNT != 0;
        if !save {
            None
        } else if all || count || !min && !max {
            Some(SavedSearchScope::All)
        } else if min && max {
            Some(SavedSearchScope::MinimumAndMaximum)
        } else if min {
            Some(SavedSearchScope::Minimum)
        } else {
            Some(SavedSearchScope::Maximum)
        }
    }
}

fn parse_return_options(
    input: &[u8],
    max_depth: usize,
) -> Result<ParsedReturnOptions, ProtocolError> {
    if input.first() != Some(&b'(') {
        return Err(invalid("IMAP SEARCH RETURN options"));
    }
    let mut parsed = ParsedReturnOptions::default();
    let mut cursor = 1usize;
    if input.get(cursor) == Some(&b')') {
        parsed.consumed = cursor + 1;
        return Ok(parsed);
    }
    loop {
        let option = parse_return_option(&input[cursor..], max_depth)?;
        parsed.flags |= return_option_flag(option.item);
        parsed.nesting_depth = parsed.nesting_depth.max(option.nesting_depth);
        cursor += option.end;
        match input.get(cursor) {
            Some(b')') => {
                parsed.consumed = cursor + 1;
                return Ok(parsed);
            }
            Some(b' ')
                if input
                    .get(cursor + 1)
                    .is_some_and(|byte| !matches!(byte, b' ' | b')')) =>
            {
                cursor += 1;
            }
            None => return Err(invalid("unterminated IMAP SEARCH RETURN options").at(cursor)),
            _ => return Err(invalid("IMAP SEARCH RETURN option separator").at(cursor)),
        }
    }
}

struct ParsedReturnOption<'a> {
    item: SearchReturnOption<'a>,
    end: usize,
    nesting_depth: usize,
}

fn parse_return_option(
    input: &[u8],
    max_depth: usize,
) -> Result<ParsedReturnOption<'_>, ProtocolError> {
    let name_end = input
        .iter()
        .position(|byte| matches!(byte, b' ' | b')'))
        .unwrap_or(input.len());
    let name = &input[..name_end];
    let known = known_return_option(name);
    if known.is_none() {
        validate_label(name)?;
    }
    let mut end = name_end;
    let mut parameters = None;
    let mut nesting_depth = 0usize;
    if known.is_none()
        && input.get(end) == Some(&b' ')
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
        nesting_depth = value.nesting_depth;
        parameters = Some(&input[value_start..end]);
    }
    Ok(ParsedReturnOption {
        item: known.unwrap_or(SearchReturnOption::Other { name, parameters }),
        end,
        nesting_depth,
    })
}

fn known_return_option(name: &[u8]) -> Option<SearchReturnOption<'_>> {
    match name {
        [first, second, third]
            if first.eq_ignore_ascii_case(&b'M')
                && second.eq_ignore_ascii_case(&b'I')
                && third.eq_ignore_ascii_case(&b'N') =>
        {
            Some(SearchReturnOption::Min)
        }
        [first, second, third]
            if first.eq_ignore_ascii_case(&b'M')
                && second.eq_ignore_ascii_case(&b'A')
                && third.eq_ignore_ascii_case(&b'X') =>
        {
            Some(SearchReturnOption::Max)
        }
        [first, second, third]
            if first.eq_ignore_ascii_case(&b'A')
                && second.eq_ignore_ascii_case(&b'L')
                && third.eq_ignore_ascii_case(&b'L') =>
        {
            Some(SearchReturnOption::All)
        }
        [first, second, third, fourth]
            if first.eq_ignore_ascii_case(&b'S')
                && second.eq_ignore_ascii_case(&b'A')
                && third.eq_ignore_ascii_case(&b'V')
                && fourth.eq_ignore_ascii_case(&b'E') =>
        {
            Some(SearchReturnOption::Save)
        }
        [first, second, third, fourth, fifth]
            if first.eq_ignore_ascii_case(&b'C')
                && second.eq_ignore_ascii_case(&b'O')
                && third.eq_ignore_ascii_case(&b'U')
                && fourth.eq_ignore_ascii_case(&b'N')
                && fifth.eq_ignore_ascii_case(&b'T') =>
        {
            Some(SearchReturnOption::Count)
        }
        _ => None,
    }
}

const fn return_option_flag(option: SearchReturnOption<'_>) -> u8 {
    match option {
        SearchReturnOption::Save => RETURN_SAVE,
        SearchReturnOption::Min => RETURN_MIN,
        SearchReturnOption::Max => RETURN_MAX,
        SearchReturnOption::All => RETURN_ALL,
        SearchReturnOption::Count => RETURN_COUNT,
        SearchReturnOption::Other { .. } => 0,
    }
}

#[allow(clippy::too_many_lines)]
fn parse_search_key(
    input: &[u8],
    depth: usize,
    max_depth: usize,
    observed_depth: &mut usize,
    usage: &mut u8,
) -> Result<usize, ProtocolError> {
    if input.first() == Some(&b'(') {
        let next_depth = enter_depth(depth, max_depth, observed_depth)?;
        let mut cursor = 1;
        let mut count = 0usize;
        loop {
            if input.get(cursor) == Some(&b')') {
                if count == 0 {
                    return Err(invalid("empty IMAP SEARCH key group"));
                }
                return Ok(cursor + 1);
            }
            let consumed = parse_search_key(
                &input[cursor..],
                next_depth,
                max_depth,
                observed_depth,
                usage,
            )?;
            cursor += consumed;
            count += 1;
            match input.get(cursor) {
                Some(b')') => return Ok(cursor + 1),
                Some(b' ')
                    if input
                        .get(cursor + 1)
                        .is_some_and(|byte| !matches!(byte, b' ' | b')')) =>
                {
                    cursor += 1;
                }
                _ => return Err(invalid("IMAP SEARCH key group separator").at(cursor)),
            }
        }
    }

    let name_end = token_end(input);
    let name = &input[..name_end];
    if name.is_empty() {
        return Err(invalid("IMAP SEARCH key"));
    }
    match classify_key(name) {
        Some(SearchKeyKind::NoArgument) => return Ok(name_end),
        Some(SearchKeyKind::AString) => {
            return parse_one_argument(input, name_end, parse_astring_argument);
        }
        Some(SearchKeyKind::Date) => {
            return parse_one_argument(input, name_end, parse_date_argument);
        }
        Some(SearchKeyKind::Header) => {
            let first_start = required_space(input, name_end, "IMAP SEARCH HEADER separator")?;
            let first_end = parse_astring_argument(&input[first_start..])?;
            let second_start = required_space(
                input,
                first_start + first_end,
                "IMAP SEARCH HEADER separator",
            )?;
            let second_end = parse_astring_argument(&input[second_start..])?;
            return Ok(second_start + second_end);
        }
        Some(SearchKeyKind::Keyword) => {
            return parse_one_argument(input, name_end, parse_atom_argument);
        }
        Some(SearchKeyKind::Number64) => {
            return parse_one_argument(input, name_end, parse_number64_argument);
        }
        Some(SearchKeyKind::Uid) => {
            let start = required_space(input, name_end, "IMAP SEARCH UID separator")?;
            let end = parse_sequence_argument(&input[start..])?;
            if input.get(start..start + end) == Some(b"$") {
                *usage |= USES_SAVED_RESULT;
            }
            return Ok(start + end);
        }
        Some(SearchKeyKind::Not) => {
            let next_depth = enter_depth(depth, max_depth, observed_depth)?;
            let argument_start = required_space(input, name_end, "IMAP SEARCH NOT separator")?;
            let argument_end = parse_search_key(
                &input[argument_start..],
                next_depth,
                max_depth,
                observed_depth,
                usage,
            )?;
            return Ok(argument_start + argument_end);
        }
        Some(SearchKeyKind::Or) => {
            let mut operator_count = 1usize;
            let mut operand_depth = enter_depth(depth, max_depth, observed_depth)?;
            let mut operand_start = required_space(input, name_end, "IMAP SEARCH OR separator")?;
            while starts_token(&input[operand_start..], b"OR") {
                operator_count += 1;
                operand_depth = enter_depth(operand_depth, max_depth, observed_depth)?;
                operand_start = required_space(
                    input,
                    operand_start + b"OR".len(),
                    "IMAP SEARCH OR separator",
                )?;
            }

            let first_end = parse_search_key(
                &input[operand_start..],
                operand_depth,
                max_depth,
                observed_depth,
                usage,
            )?;
            let mut cursor = operand_start + first_end;
            for index in 0..operator_count {
                cursor = required_space(input, cursor, "IMAP SEARCH OR separator")?;
                let end = parse_search_key(
                    &input[cursor..],
                    operand_depth - index,
                    max_depth,
                    observed_depth,
                    usage,
                )?;
                cursor += end;
            }
            return Ok(cursor);
        }
        None => {}
    }
    if validate_sequence_set(name).is_ok() {
        if name == b"$" {
            *usage |= USES_SAVED_RESULT;
        } else {
            *usage |= USES_SEQUENCE_NUMBERS;
        }
        return Ok(name_end);
    }
    if name.first().is_some_and(u8::is_ascii_digit)
        || name.iter().any(|byte| matches!(byte, b',' | b':'))
    {
        return Err(invalid("IMAP SEARCH sequence-set"));
    }
    validate_atom(name, "IMAP SEARCH extension key")?;
    Ok(name_end)
}

fn parse_one_argument(
    input: &[u8],
    name_end: usize,
    parser: fn(&[u8]) -> Result<usize, ProtocolError>,
) -> Result<usize, ProtocolError> {
    let start = required_space(input, name_end, "IMAP SEARCH key argument separator")?;
    parser(&input[start..]).map(|end| start + end)
}

fn parse_astring_argument(input: &[u8]) -> Result<usize, ProtocolError> {
    parse_astring_prefix(input).map(|parsed| parsed.end)
}

fn parse_atom_argument(input: &[u8]) -> Result<usize, ProtocolError> {
    let end = token_end(input);
    validate_atom(&input[..end], "IMAP SEARCH atom argument")?;
    Ok(end)
}

fn parse_number64_argument(input: &[u8]) -> Result<usize, ProtocolError> {
    let end = token_end(input);
    parse_number64(&input[..end])?;
    Ok(end)
}

fn parse_sequence_argument(input: &[u8]) -> Result<usize, ProtocolError> {
    let end = token_end(input);
    validate_sequence_set(&input[..end])?;
    Ok(end)
}

fn parse_date_argument(input: &[u8]) -> Result<usize, ProtocolError> {
    let end = token_end(input);
    validate_date(&input[..end])?;
    Ok(end)
}

fn validate_date(input: &[u8]) -> Result<(), ProtocolError> {
    let value = if input.len() >= 2 && input.first() == Some(&b'"') && input.last() == Some(&b'"') {
        &input[1..input.len() - 1]
    } else {
        input
    };
    let mut parts = value.split(|byte| *byte == b'-');
    let day = parts.next().unwrap_or_default();
    let month = parts.next().unwrap_or_default();
    let year = parts.next().unwrap_or_default();
    if parts.next().is_some()
        || day.is_empty()
        || day.len() > 2
        || !day.iter().all(u8::is_ascii_digit)
        || year.len() != 4
        || !year.iter().all(u8::is_ascii_digit)
        || !is_month(month)
    {
        Err(invalid("IMAP SEARCH date"))
    } else {
        Ok(())
    }
}

fn is_month(input: &[u8]) -> bool {
    [
        b"Jan".as_slice(),
        b"Feb",
        b"Mar",
        b"Apr",
        b"May",
        b"Jun",
        b"Jul",
        b"Aug",
        b"Sep",
        b"Oct",
        b"Nov",
        b"Dec",
    ]
    .iter()
    .any(|month| input.eq_ignore_ascii_case(month))
}

fn parse_number64(input: &[u8]) -> Result<u64, ProtocolError> {
    if input.is_empty() || !input.iter().all(u8::is_ascii_digit) {
        return Err(invalid("IMAP SEARCH number64"));
    }
    let mut value = 0u64;
    for digit in input {
        value = value
            .checked_mul(10)
            .and_then(|number| number.checked_add(u64::from(*digit - b'0')))
            .ok_or_else(|| invalid("IMAP SEARCH number64"))?;
    }
    if value > i64::MAX as u64 {
        Err(invalid("IMAP SEARCH number64"))
    } else {
        Ok(value)
    }
}

fn validate_sequence_set(input: &[u8]) -> Result<(), ProtocolError> {
    if input == b"$" {
        return Ok(());
    }
    if input.is_empty() {
        return Err(invalid("IMAP SEARCH sequence-set"));
    }
    let mut cursor = 0usize;
    loop {
        cursor = validate_sequence_at(input, cursor)?;
        if input.get(cursor) == Some(&b':') {
            cursor = validate_sequence_at(input, cursor + 1)?;
        }
        match input.get(cursor) {
            None => return Ok(()),
            Some(b',') if cursor + 1 < input.len() => cursor += 1,
            _ => return Err(invalid("IMAP SEARCH sequence-set")),
        }
    }
}

fn validate_sequence_at(input: &[u8], mut cursor: usize) -> Result<usize, ProtocolError> {
    if input.get(cursor) == Some(&b'*') {
        return Ok(cursor + 1);
    }
    if input
        .get(cursor)
        .is_none_or(|byte| *byte == b'0' || !byte.is_ascii_digit())
    {
        return Err(invalid("IMAP SEARCH sequence-set"));
    }
    let mut value = 0u64;
    while let Some(digit) = input.get(cursor).filter(|byte| byte.is_ascii_digit()) {
        value = value * 10 + u64::from(*digit - b'0');
        if value > u64::from(u32::MAX) {
            return Err(invalid("IMAP SEARCH sequence-set"));
        }
        cursor += 1;
    }
    Ok(cursor)
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
        Err(invalid(context))
    } else {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SearchKeyKind {
    NoArgument,
    AString,
    Date,
    Header,
    Keyword,
    Number64,
    Uid,
    Not,
    Or,
}

fn classify_key(input: &[u8]) -> Option<SearchKeyKind> {
    let first = input.first()?.to_ascii_uppercase();
    let kind = match (first, input.len()) {
        (b'A', 3) if input.eq_ignore_ascii_case(b"ALL") => SearchKeyKind::NoArgument,
        (b'A', 8) if input.eq_ignore_ascii_case(b"ANSWERED") => SearchKeyKind::NoArgument,
        (b'B', 6) if input.eq_ignore_ascii_case(b"BEFORE") => SearchKeyKind::Date,
        (b'B', 3) if input.eq_ignore_ascii_case(b"BCC") => SearchKeyKind::AString,
        (b'B', 4) if input.eq_ignore_ascii_case(b"BODY") => SearchKeyKind::AString,
        (b'C', 2) if input.eq_ignore_ascii_case(b"CC") => SearchKeyKind::AString,
        (b'D', 7) if input.eq_ignore_ascii_case(b"DELETED") => SearchKeyKind::NoArgument,
        (b'D', 5) if input.eq_ignore_ascii_case(b"DRAFT") => SearchKeyKind::NoArgument,
        (b'F', 7) if input.eq_ignore_ascii_case(b"FLAGGED") => SearchKeyKind::NoArgument,
        (b'F', 4) if input.eq_ignore_ascii_case(b"FROM") => SearchKeyKind::AString,
        (b'H', 6) if input.eq_ignore_ascii_case(b"HEADER") => SearchKeyKind::Header,
        (b'K', 7) if input.eq_ignore_ascii_case(b"KEYWORD") => SearchKeyKind::Keyword,
        (b'L', 6) if input.eq_ignore_ascii_case(b"LARGER") => SearchKeyKind::Number64,
        (b'N', 3) if input.eq_ignore_ascii_case(b"NOT") => SearchKeyKind::Not,
        (b'O', 2) if input.eq_ignore_ascii_case(b"ON") => SearchKeyKind::Date,
        (b'O', 2) if input.eq_ignore_ascii_case(b"OR") => SearchKeyKind::Or,
        (b'S', 4) if input.eq_ignore_ascii_case(b"SEEN") => SearchKeyKind::NoArgument,
        (b'S', 10) if input.eq_ignore_ascii_case(b"SENTBEFORE") => SearchKeyKind::Date,
        (b'S', 6) if input.eq_ignore_ascii_case(b"SENTON") => SearchKeyKind::Date,
        (b'S', 9) if input.eq_ignore_ascii_case(b"SENTSINCE") => SearchKeyKind::Date,
        (b'S', 5) if input.eq_ignore_ascii_case(b"SINCE") => SearchKeyKind::Date,
        (b'S', 7) if input.eq_ignore_ascii_case(b"SMALLER") => SearchKeyKind::Number64,
        (b'S', 7) if input.eq_ignore_ascii_case(b"SUBJECT") => SearchKeyKind::AString,
        (b'T', 4) if input.eq_ignore_ascii_case(b"TEXT") => SearchKeyKind::AString,
        (b'T', 2) if input.eq_ignore_ascii_case(b"TO") => SearchKeyKind::AString,
        (b'U', 3) if input.eq_ignore_ascii_case(b"UID") => SearchKeyKind::Uid,
        (b'U', 10) if input.eq_ignore_ascii_case(b"UNANSWERED") => SearchKeyKind::NoArgument,
        (b'U', 9) if input.eq_ignore_ascii_case(b"UNDELETED") => SearchKeyKind::NoArgument,
        (b'U', 7) if input.eq_ignore_ascii_case(b"UNDRAFT") => SearchKeyKind::NoArgument,
        (b'U', 9) if input.eq_ignore_ascii_case(b"UNFLAGGED") => SearchKeyKind::NoArgument,
        (b'U', 6) if input.eq_ignore_ascii_case(b"UNSEEN") => SearchKeyKind::NoArgument,
        (b'U', 9) if input.eq_ignore_ascii_case(b"UNKEYWORD") => SearchKeyKind::Keyword,
        _ => return None,
    };
    Some(kind)
}

fn enter_depth(
    depth: usize,
    max_depth: usize,
    observed_depth: &mut usize,
) -> Result<usize, ProtocolError> {
    let next = depth + 1;
    if next > max_depth {
        return Err(ProtocolError::new(
            ErrorKind::NestingTooDeep,
            "IMAP SEARCH nesting",
        ));
    }
    *observed_depth = (*observed_depth).max(next);
    Ok(next)
}

fn required_space(
    input: &[u8],
    cursor: usize,
    context: &'static str,
) -> Result<usize, ProtocolError> {
    if input.get(cursor) != Some(&b' ')
        || input.get(cursor + 1).is_none()
        || input.get(cursor + 1) == Some(&b' ')
    {
        Err(invalid(context).at(cursor))
    } else {
        Ok(cursor + 1)
    }
}

fn token_end(input: &[u8]) -> usize {
    input
        .iter()
        .position(|byte| matches!(byte, b' ' | b')'))
        .unwrap_or(input.len())
}

fn starts_token(input: &[u8], token: &[u8]) -> bool {
    input.len() >= token.len()
        && input[..token.len()].eq_ignore_ascii_case(token)
        && (input.len() == token.len() || input[token.len()] == b' ')
}

const fn invalid(context: &'static str) -> ProtocolError {
    ProtocolError::new(ErrorKind::InvalidSyntax, context)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_base_search_and_literal_arguments() {
        let wire = Bytes::from_static(
            b"RETURN (MIN MAX COUNT SAVE) CHARSET UTF-8 OR FROM \"Alice\" (NOT SEEN HEADER Subject {5}\r\nhello) UID 1:20,*",
        );
        let program = SearchProgram::parse(&wire).unwrap();
        assert_eq!(
            program.return_options(),
            Some(b"(MIN MAX COUNT SAVE)".as_slice())
        );
        assert_eq!(program.charset(), Some(b"UTF-8".as_slice()));
        assert!(program.criteria().starts_with(b"OR FROM"));
        assert_eq!(program.nesting_depth(), 3);
        assert!(!program.uses_sequence_numbers());
        assert!(!program.uses_saved_result());
        assert_eq!(program.saved_result_scope(), Some(SavedSearchScope::All));

        let quoted = SearchProgram::parse(&Bytes::from_static(b"CHARSET \"UTF-8\" ALL"))
            .expect("quoted charset is valid RFC 9051 grammar");
        assert_eq!(quoted.charset(), Some(b"\"UTF-8\"".as_slice()));

        let sequence = SearchProgram::parse(&Bytes::from_static(b"OR UNSEEN (NOT 1:4)"))
            .expect("nested message sequence-set is valid");
        assert!(sequence.uses_sequence_numbers());
    }

    #[test]
    fn derives_rfc9051_saved_result_scopes_and_reads() {
        for (wire, expected) in [
            (b"RETURN (SAVE) ALL".as_slice(), SavedSearchScope::All),
            (b"RETURN (ALL SAVE) ALL", SavedSearchScope::All),
            (b"RETURN (COUNT SAVE) ALL", SavedSearchScope::All),
            (b"RETURN (SAVE MIN) ALL", SavedSearchScope::Minimum),
            (b"RETURN (MAX SAVE) ALL", SavedSearchScope::Maximum),
            (
                b"RETURN (MAX SAVE MIN) ALL",
                SavedSearchScope::MinimumAndMaximum,
            ),
        ] {
            let parsed = SearchProgram::parse(&Bytes::copy_from_slice(wire)).unwrap();
            assert_eq!(parsed.saved_result_scope(), Some(expected), "{wire:?}");
        }

        let reads = SearchProgram::parse(&Bytes::from_static(
            b"RETURN (SAVE) OR $ (UID $ KEYWORD $Junk)",
        ))
        .unwrap();
        assert!(reads.uses_saved_result());
        assert!(!reads.uses_sequence_numbers());

        let keyword = SearchProgram::parse(&Bytes::from_static(b"KEYWORD $Junk")).unwrap();
        assert!(!keyword.uses_saved_result());
        assert_eq!(keyword.saved_result_scope(), None);
    }

    #[test]
    fn parses_typed_return_extensions_with_bounded_parameters() {
        let wire = Bytes::from_static(
            b"RETURN (MIN X-SCALAR 123 X-LIST (one (two three) {4+}\r\ntest) SAVE MAX) ALL",
        );
        let parsed = SearchProgram::parse(&wire).unwrap();
        assert_eq!(parsed.return_extension_depth(), 2);
        assert_eq!(
            parsed.return_option_items().collect::<Vec<_>>(),
            vec![
                SearchReturnOption::Min,
                SearchReturnOption::Other {
                    name: b"X-SCALAR",
                    parameters: Some(b"123"),
                },
                SearchReturnOption::Other {
                    name: b"X-LIST",
                    parameters: Some(b"(one (two three) {4+}\r\ntest)"),
                },
                SearchReturnOption::Save,
                SearchReturnOption::Max,
            ]
        );
        assert_eq!(
            parsed.saved_result_scope(),
            Some(SavedSearchScope::MinimumAndMaximum)
        );
        assert_eq!(
            SearchProgram::parse_with_max_depth(&wire, 1)
                .unwrap_err()
                .kind(),
            ErrorKind::NestingTooDeep
        );

        let empty = SearchProgram::parse(&Bytes::from_static(b"RETURN (X-EMPTY ()) ALL")).unwrap();
        assert_eq!(
            empty.return_option_items().collect::<Vec<_>>(),
            vec![SearchReturnOption::Other {
                name: b"X-EMPTY",
                parameters: Some(b"()"),
            }]
        );
    }

    #[test]
    fn command_exposes_uid_search_program() {
        let command = Command {
            tag: Bytes::from_static(b"A1"),
            body: CommandBody::Uid {
                command: Bytes::from_static(b"SEARCH"),
                arguments: Bytes::from_static(b"RETURN (SAVE MIN) UID $"),
            },
        };
        let parsed = command.parsed_search_program().unwrap().unwrap();
        assert!(parsed.uses_saved_result());
        assert_eq!(parsed.saved_result_scope(), Some(SavedSearchScope::Minimum));
    }

    #[test]
    fn enforces_recursive_nesting_limit() {
        let wire = Bytes::from_static(b"(((ALL)))");
        assert!(SearchProgram::parse_with_max_depth(&wire, 3).is_ok());
        let error = SearchProgram::parse_with_max_depth(&wire, 2).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::NestingTooDeep);

        let wire = Bytes::from_static(b"NOT NOT NOT ALL");
        assert!(SearchProgram::parse_with_max_depth(&wire, 3).is_ok());
        assert_eq!(
            SearchProgram::parse_with_max_depth(&wire, 2)
                .unwrap_err()
                .kind(),
            ErrorKind::NestingTooDeep
        );

        let wire =
            Bytes::from_static(b"OR OR OR OR SUBJECT \"q\" FROM \"q\" TO \"q\" CC \"q\" BCC \"q\"");
        let parsed = SearchProgram::parse_with_max_depth(&wire, 4).unwrap();
        assert_eq!(parsed.nesting_depth(), 4);
        assert_eq!(
            SearchProgram::parse_with_max_depth(&wire, 3)
                .unwrap_err()
                .kind(),
            ErrorKind::NestingTooDeep
        );

        let right_nested =
            SearchProgram::parse(&Bytes::from_static(b"OR ALL OR SEEN DELETED")).unwrap();
        assert_eq!(right_nested.nesting_depth(), 2);
    }

    #[test]
    fn rejects_invalid_base_search_grammar() {
        for wire in [
            b"".as_slice(),
            b"RETURN (MIN)  ALL",
            b"RETURN (MIN  MAX) ALL",
            b"RETURN (1BAD) ALL",
            b"RETURN (X+BAD) ALL",
            b"RETURN (MIN 1) ALL",
            b"RETURN (X-LIST (one  two)) ALL",
            b"RETURN (X-LIST (one ())) ALL",
            b"RETURN (X-NUM 9223372036854775808) ALL",
            b"RETURN (X-LITERAL {1}\r\nx) ALL",
            b"RETURN",
            b"CHARSET",
            b"CHARSET UTF-8",
            b"CHARSET {5}\r\nUTF-8 ALL",
            b"OR ALL",
            b"OR OR ALL ALL",
            b"NOT",
            b"()",
            b"(ALL  SEEN)",
            b"(ALL )",
            b"HEADER Subject",
            b"FROM \"bad\\escape\"",
            b"BEFORE 1-Not-2026",
            b"LARGER 9223372036854775808",
            b"UID 0",
            b"UID 01",
            b"UID 4294967296",
            b"01",
            b"4294967296",
            b"1,,2",
            b"1,",
            b"1:",
            b":1",
            b"1::2",
            b"1:*:2",
            b"*0",
            b"1a",
        ] {
            assert!(
                SearchProgram::parse(&Bytes::copy_from_slice(wire)).is_err(),
                "{wire:?}"
            );
        }
    }

    #[test]
    fn accepts_sequence_set_u32_boundaries() {
        for wire in [
            b"1".as_slice(),
            b"4294967295",
            b"1:4294967295",
            b"4294967295:*",
            b"1,2:4,4294967295,*",
            b"UID 1,2:4,4294967295:*",
        ] {
            SearchProgram::parse(&Bytes::copy_from_slice(wire)).unwrap_or_else(|error| {
                panic!("valid sequence-set {wire:?} was rejected: {error}")
            });
        }
    }
}
