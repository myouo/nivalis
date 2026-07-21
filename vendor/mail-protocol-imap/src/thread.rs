use std::ops::Range;

use bytes::{Bytes, BytesMut};
use mail_protocol_core::{ErrorKind, ProtocolError};

use crate::astring::parse_astring_prefix;
use crate::{AString, AStringKind, Command, CommandBody, SearchProgram};

/// Default maximum nesting accepted in THREAD search criteria and responses.
pub const DEFAULT_THREAD_MAX_DEPTH: usize = 64;

/// Default maximum combined list and message nodes in one THREAD response.
pub const DEFAULT_THREAD_MAX_NODES: usize = 65_536;

/// RFC 5256 threading algorithm.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum ThreadAlgorithm<'a> {
    /// The ORDEREDSUBJECT algorithm.
    OrderedSubject,
    /// The REFERENCES algorithm.
    References,
    /// An extension algorithm name.
    Other(&'a [u8]),
}

impl<'a> ThreadAlgorithm<'a> {
    /// Returns the algorithm name.
    pub const fn name(self) -> &'a [u8] {
        match self {
            Self::OrderedSubject => b"ORDEREDSUBJECT",
            Self::References => b"REFERENCES",
            Self::Other(name) => name,
        }
    }
}

/// Validated, zero-copy RFC 5256 THREAD command arguments.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ThreadArguments {
    wire: Bytes,
    algorithm: Range<usize>,
    charset: AString,
    search: SearchProgram,
}

impl ThreadArguments {
    /// Parses THREAD arguments using the default search nesting limit.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid algorithm, invalid charset, missing
    /// search criteria, or an embedded SEARCH `RETURN` or `CHARSET` prefix.
    pub fn parse(wire: &Bytes) -> Result<Self, ProtocolError> {
        Self::parse_with_max_depth(wire, DEFAULT_THREAD_MAX_DEPTH)
    }

    /// Parses THREAD arguments with an explicit search nesting limit.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::parse`] and `NestingTooDeep` when a
    /// search key exceeds `max_depth`.
    pub fn parse_with_max_depth(wire: &Bytes, max_depth: usize) -> Result<Self, ProtocolError> {
        let parsed = validate_thread_arguments(wire, max_depth)?;
        let charset = AString::parse(&wire.slice(parsed.charset.clone()))?;
        let search = SearchProgram::parse_with_max_depth(&wire.slice(parsed.search), max_depth)?;
        Ok(Self {
            wire: wire.clone(),
            algorithm: parsed.algorithm,
            charset,
            search,
        })
    }

    /// Returns the exact bytes following the THREAD command name.
    pub const fn as_bytes(&self) -> &Bytes {
        &self.wire
    }

    /// Returns the typed threading algorithm.
    pub fn algorithm(&self) -> ThreadAlgorithm<'_> {
        classify_algorithm(&self.wire[self.algorithm.clone()])
    }

    /// Returns the mandatory charset.
    pub const fn charset(&self) -> &AString {
        &self.charset
    }

    /// Returns the validated search program.
    pub const fn search_program(&self) -> &SearchProgram {
        &self.search
    }

    /// Appends the validated arguments exactly as received.
    pub fn encode(&self, destination: &mut BytesMut) {
        destination.extend_from_slice(&self.wire);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ParsedThreadArguments {
    pub(crate) algorithm: Range<usize>,
    pub(crate) charset: Range<usize>,
    pub(crate) search: Range<usize>,
}

/// Validates THREAD arguments while borrowing a complete command frame.
pub(crate) fn validate_thread_arguments(
    input: &[u8],
    max_depth: usize,
) -> Result<ParsedThreadArguments, ProtocolError> {
    let algorithm_end = input
        .iter()
        .position(|byte| *byte == b' ')
        .unwrap_or(input.len());
    validate_atom(&input[..algorithm_end], "IMAP THREAD algorithm")?;
    let algorithm = 0..algorithm_end;

    let mut cursor = require_space(input, algorithm_end, "IMAP THREAD algorithm separator")?;
    let charset_start = cursor;
    let parsed_charset = parse_astring_prefix(&input[charset_start..])
        .map_err(|error| shift_error(error, charset_start))?;
    if matches!(parsed_charset.kind, AStringKind::Literal { .. }) {
        return Err(invalid("literal IMAP THREAD charset").at(charset_start));
    }
    cursor = charset_start + parsed_charset.end;
    let charset = charset_start..cursor;

    cursor = require_space(input, cursor, "IMAP THREAD search separator")?;
    if cursor == input.len() {
        return Err(invalid("missing IMAP THREAD search criteria").at(cursor));
    }
    crate::search::validate_search_program(&input[cursor..], max_depth)
        .map_err(|error| shift_error(error, cursor))?;
    let first_end = input[cursor..]
        .iter()
        .position(|byte| *byte == b' ')
        .map_or(input.len(), |offset| cursor + offset);
    if input[cursor..first_end].eq_ignore_ascii_case(b"RETURN")
        || input[cursor..first_end].eq_ignore_ascii_case(b"CHARSET")
    {
        return Err(invalid("prefixed IMAP THREAD search program").at(cursor));
    }

    Ok(ParsedThreadArguments {
        algorithm,
        charset,
        search: cursor..input.len(),
    })
}

impl Command {
    /// Returns typed arguments for direct or UID THREAD commands.
    ///
    /// # Errors
    ///
    /// Returns an error if a manually constructed UID or raw command contains
    /// invalid THREAD arguments.
    pub fn parsed_thread_arguments(&self) -> Result<Option<ThreadArguments>, ProtocolError> {
        match &self.body {
            CommandBody::Thread { arguments } => Ok(Some(arguments.clone())),
            CommandBody::Uid { command, arguments } if command.eq_ignore_ascii_case(b"THREAD") => {
                ThreadArguments::parse(arguments).map(Some)
            }
            CommandBody::Raw { name, arguments } if name.eq_ignore_ascii_case(b"THREAD") => {
                ThreadArguments::parse(arguments).map(Some)
            }
            _ => Ok(None),
        }
    }
}

/// One event in the depth-first wire order of a THREAD response forest.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum ThreadEvent {
    /// Start of one parenthesized thread list.
    ListStart,
    /// A message sequence number in the current list.
    Message(u32),
    /// End of one parenthesized thread list.
    ListEnd,
}

/// Allocation-free iterator over a validated THREAD response forest.
#[derive(Clone, Debug)]
pub struct ThreadEventIter<'a> {
    remaining: &'a [u8],
}

impl Iterator for ThreadEventIter<'_> {
    type Item = ThreadEvent;

    fn next(&mut self) -> Option<Self::Item> {
        while self.remaining.first() == Some(&b' ') {
            self.remaining = &self.remaining[1..];
        }
        match self.remaining.first().copied()? {
            b'(' => {
                self.remaining = &self.remaining[1..];
                Some(ThreadEvent::ListStart)
            }
            b')' => {
                self.remaining = &self.remaining[1..];
                Some(ThreadEvent::ListEnd)
            }
            b'0'..=b'9' => {
                let end = self
                    .remaining
                    .iter()
                    .position(|byte| !byte.is_ascii_digit())
                    .unwrap_or(self.remaining.len());
                let value = parse_nz_number(&self.remaining[..end], 0).ok()?;
                self.remaining = &self.remaining[end..];
                Some(ThreadEvent::Message(value))
            }
            _ => {
                debug_assert!(false, "validated THREAD response became invalid");
                self.remaining = b"";
                None
            }
        }
    }
}

/// Validated, zero-copy RFC 5256 THREAD response data.
///
/// Validation uses an explicit stack and independently limits nesting and the
/// combined number of list/message nodes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ThreadResponse {
    wire: Bytes,
    threads: Option<Range<usize>>,
    message_count: usize,
    list_count: usize,
    maximum_depth: usize,
}

impl ThreadResponse {
    /// Parses complete untagged response data beginning with `THREAD` using
    /// default depth and node budgets.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed tree grammar, zero or overflowing message
    /// numbers, excessive nesting, or a response exceeding the node budget.
    pub fn parse(wire: &Bytes) -> Result<Self, ProtocolError> {
        Self::parse_with_limits(wire, DEFAULT_THREAD_MAX_DEPTH, DEFAULT_THREAD_MAX_NODES)
    }

    /// Parses THREAD response data with explicit nesting and node budgets.
    ///
    /// `max_nodes` counts both parenthesized lists and message-number nodes.
    /// Empty `THREAD` data consumes no budget. `max_depth` can tighten, but not
    /// raise, the hard [`DEFAULT_THREAD_MAX_DEPTH`] security limit. This keeps
    /// validation iterative and allocation-free even for caller-supplied
    /// limits.
    ///
    /// # Errors
    ///
    /// Returns `NestingTooDeep` when `max_depth` is exceeded and
    /// `FrameTooLarge` when `max_nodes` is exceeded, in addition to syntax
    /// errors.
    pub fn parse_with_limits(
        wire: &Bytes,
        max_depth: usize,
        max_nodes: usize,
    ) -> Result<Self, ProtocolError> {
        const NAME: &[u8] = b"THREAD";
        if wire.len() < NAME.len() || !wire[..NAME.len()].eq_ignore_ascii_case(NAME) {
            return Err(invalid("IMAP THREAD response name"));
        }
        if wire.len() == NAME.len() {
            return Ok(Self {
                wire: wire.clone(),
                threads: None,
                message_count: 0,
                list_count: 0,
                maximum_depth: 0,
            });
        }
        if wire.get(NAME.len()) != Some(&b' ') {
            return Err(invalid("IMAP THREAD response separator").at(NAME.len()));
        }
        let threads_start = NAME.len() + 1;
        if wire.get(threads_start) != Some(&b'(') {
            return Err(invalid("missing IMAP THREAD list").at(threads_start));
        }

        let max_depth = max_depth.min(DEFAULT_THREAD_MAX_DEPTH);
        let mut cursor = threads_start;
        let mut budget = NodeBudget::new(max_nodes);
        let mut stats = ThreadStats::default();
        let mut stack = [ListFrame {
            mode: ListMode::NeedContent,
        }; DEFAULT_THREAD_MAX_DEPTH];
        while cursor < wire.len() {
            if wire.get(cursor) != Some(&b'(') {
                return Err(invalid("IMAP THREAD root-list separator").at(cursor));
            }
            cursor =
                validate_thread_list(wire, cursor, max_depth, &mut budget, &mut stack, &mut stats)?;
        }

        Ok(Self {
            wire: wire.clone(),
            threads: Some(threads_start..cursor),
            message_count: stats.message_count,
            list_count: stats.list_count,
            maximum_depth: stats.maximum_depth,
        })
    }

    /// Returns the complete response data beginning with `THREAD`.
    pub const fn as_bytes(&self) -> &Bytes {
        &self.wire
    }

    /// Returns the exact validated thread forest, excluding the response name.
    pub fn threads_bytes(&self) -> Option<&[u8]> {
        self.threads.as_ref().map(|range| &self.wire[range.clone()])
    }

    /// Iterates through lists and messages in depth-first wire order.
    pub fn events(&self) -> ThreadEventIter<'_> {
        ThreadEventIter {
            remaining: self
                .threads
                .as_ref()
                .map_or(b"".as_slice(), |range| &self.wire[range.clone()]),
        }
    }

    /// Returns the number of message-number nodes.
    pub const fn message_count(&self) -> usize {
        self.message_count
    }

    /// Returns the number of parenthesized list nodes.
    pub const fn list_count(&self) -> usize {
        self.list_count
    }

    /// Returns the combined number of list and message nodes.
    pub const fn node_count(&self) -> usize {
        self.message_count + self.list_count
    }

    /// Returns the greatest list nesting depth observed.
    pub const fn maximum_depth(&self) -> usize {
        self.maximum_depth
    }

    /// Appends the validated response data exactly as received.
    pub fn encode(&self, destination: &mut BytesMut) {
        destination.extend_from_slice(&self.wire);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ListMode {
    NeedContent,
    Members,
    Children { completed: usize },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ListFrame {
    mode: ListMode,
}

struct NodeBudget {
    used: usize,
    maximum: usize,
}

#[derive(Default)]
struct ThreadStats {
    message_count: usize,
    list_count: usize,
    maximum_depth: usize,
}

impl NodeBudget {
    const fn new(maximum: usize) -> Self {
        Self { used: 0, maximum }
    }

    fn consume(&mut self, offset: usize) -> Result<(), ProtocolError> {
        if self.used == self.maximum {
            return Err(
                ProtocolError::new(ErrorKind::FrameTooLarge, "IMAP THREAD node budget").at(offset),
            );
        }
        self.used += 1;
        Ok(())
    }
}

fn validate_thread_list(
    input: &[u8],
    start: usize,
    max_depth: usize,
    budget: &mut NodeBudget,
    stack: &mut [ListFrame; DEFAULT_THREAD_MAX_DEPTH],
    stats: &mut ThreadStats,
) -> Result<usize, ProtocolError> {
    let mut depth = 0usize;
    let mut cursor = start;
    open_list(
        input,
        &mut cursor,
        max_depth,
        budget,
        stack,
        &mut depth,
        stats,
    )?;

    while depth != 0 {
        let frame = stack[depth - 1];
        match frame.mode {
            ListMode::NeedContent => match input.get(cursor) {
                Some(b'0'..=b'9') => {
                    cursor = consume_message(input, cursor, budget, stats)?;
                    stack[depth - 1].mode = ListMode::Members;
                }
                Some(b'(') => {
                    stack[depth - 1].mode = ListMode::Children { completed: 0 };
                    open_list(
                        input,
                        &mut cursor,
                        max_depth,
                        budget,
                        stack,
                        &mut depth,
                        stats,
                    )?;
                }
                _ => return Err(invalid("empty IMAP THREAD list").at(cursor)),
            },
            ListMode::Members => match input.get(cursor) {
                Some(b')') => close_list(&mut cursor, stack, &mut depth),
                Some(b' ') => match input.get(cursor + 1) {
                    Some(b'0'..=b'9') => {
                        cursor = consume_message(input, cursor + 1, budget, stats)?;
                    }
                    Some(b'(') => {
                        cursor += 1;
                        stack[depth - 1].mode = ListMode::Children { completed: 0 };
                        open_list(
                            input,
                            &mut cursor,
                            max_depth,
                            budget,
                            stack,
                            &mut depth,
                            stats,
                        )?;
                    }
                    _ => return Err(invalid("IMAP THREAD member separator").at(cursor)),
                },
                _ => return Err(invalid("IMAP THREAD member terminator").at(cursor)),
            },
            ListMode::Children { completed } => match input.get(cursor) {
                Some(b'(') => open_list(
                    input,
                    &mut cursor,
                    max_depth,
                    budget,
                    stack,
                    &mut depth,
                    stats,
                )?,
                Some(b')') if completed >= 2 => {
                    close_list(&mut cursor, stack, &mut depth);
                }
                Some(b')') => {
                    return Err(invalid("IMAP THREAD nested-list cardinality").at(cursor));
                }
                _ => return Err(invalid("IMAP THREAD nested-list separator").at(cursor)),
            },
        }
    }
    Ok(cursor)
}

fn open_list(
    input: &[u8],
    cursor: &mut usize,
    max_depth: usize,
    budget: &mut NodeBudget,
    stack: &mut [ListFrame; DEFAULT_THREAD_MAX_DEPTH],
    depth: &mut usize,
    stats: &mut ThreadStats,
) -> Result<(), ProtocolError> {
    if input.get(*cursor) != Some(&b'(') {
        return Err(invalid("IMAP THREAD list start").at(*cursor));
    }
    let next_depth = *depth + 1;
    if next_depth > max_depth {
        return Err(
            ProtocolError::new(ErrorKind::NestingTooDeep, "IMAP THREAD response nesting")
                .at(*cursor),
        );
    }
    budget.consume(*cursor)?;
    stats.list_count += 1;
    stats.maximum_depth = stats.maximum_depth.max(next_depth);
    stack[*depth] = ListFrame {
        mode: ListMode::NeedContent,
    };
    *depth = next_depth;
    *cursor += 1;
    Ok(())
}

fn close_list(
    cursor: &mut usize,
    stack: &mut [ListFrame; DEFAULT_THREAD_MAX_DEPTH],
    depth: &mut usize,
) {
    *cursor += 1;
    *depth -= 1;
    if *depth == 0 {
        return;
    }
    let ListMode::Children { completed } = &mut stack[*depth - 1].mode else {
        return;
    };
    *completed += 1;
}

fn consume_message(
    input: &[u8],
    start: usize,
    budget: &mut NodeBudget,
    stats: &mut ThreadStats,
) -> Result<usize, ProtocolError> {
    let end = input[start..]
        .iter()
        .position(|byte| !byte.is_ascii_digit())
        .map_or(input.len(), |offset| start + offset);
    parse_nz_number(&input[start..end], start)?;
    if !matches!(input.get(end), Some(b' ' | b')')) {
        return Err(invalid("IMAP THREAD message terminator").at(end));
    }
    budget.consume(start)?;
    stats.message_count += 1;
    Ok(end)
}

fn parse_nz_number(digits: &[u8], offset: usize) -> Result<u32, ProtocolError> {
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) || digits.first() == Some(&b'0')
    {
        return Err(invalid("IMAP THREAD message number").at(offset));
    }
    let value = digits.iter().try_fold(0u64, |value, digit| {
        value
            .checked_mul(10)
            .and_then(|value| value.checked_add(u64::from(*digit - b'0')))
    });
    value
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| invalid("IMAP THREAD message number").at(offset))
}

fn classify_algorithm(name: &[u8]) -> ThreadAlgorithm<'_> {
    if name.eq_ignore_ascii_case(b"ORDEREDSUBJECT") {
        ThreadAlgorithm::OrderedSubject
    } else if name.eq_ignore_ascii_case(b"REFERENCES") {
        ThreadAlgorithm::References
    } else {
        ThreadAlgorithm::Other(name)
    }
}

fn validate_atom(input: &[u8], context: &'static str) -> Result<(), ProtocolError> {
    let parsed = parse_astring_prefix(input).map_err(|_| invalid(context))?;
    if parsed.end != input.len() || parsed.kind != AStringKind::Atom {
        return Err(invalid(context));
    }
    Ok(())
}

fn require_space(
    input: &[u8],
    cursor: usize,
    context: &'static str,
) -> Result<usize, ProtocolError> {
    if input.get(cursor) == Some(&b' ') {
        Ok(cursor + 1)
    } else {
        Err(invalid(context).at(cursor))
    }
}

fn shift_error(error: ProtocolError, start: usize) -> ProtocolError {
    error.at(start.saturating_add(error.offset().unwrap_or(0)))
}

const fn invalid(context: &'static str) -> ProtocolError {
    ProtocolError::new(ErrorKind::InvalidSyntax, context)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(input: &'static [u8]) -> Result<ThreadArguments, ProtocolError> {
        ThreadArguments::parse(&Bytes::from_static(input))
    }

    fn response(input: &'static [u8]) -> Result<ThreadResponse, ProtocolError> {
        ThreadResponse::parse(&Bytes::from_static(input))
    }

    fn nested_thread_list(depth: usize) -> Vec<u8> {
        if depth == 1 {
            return b"(1)".to_vec();
        }
        let mut wire = Vec::from(b"(".as_slice());
        wire.extend_from_slice(&nested_thread_list(depth - 1));
        wire.extend_from_slice(b"(1))");
        wire
    }

    #[test]
    fn parses_thread_arguments_and_extension_algorithm() {
        let value = arguments(b"REFERENCES UTF-8 NOT DELETED").unwrap();
        assert_eq!(value.algorithm(), ThreadAlgorithm::References);
        assert_eq!(value.charset().decoded().as_ref(), b"UTF-8");
        assert_eq!(value.search_program().criteria(), b"NOT DELETED");

        let value = arguments(b"X-GM-THREAD \"UTF-8\" ALL").unwrap();
        assert_eq!(value.algorithm(), ThreadAlgorithm::Other(b"X-GM-THREAD"));
    }

    #[test]
    fn rejects_invalid_thread_arguments() {
        for input in [
            b"REFERENCES".as_slice(),
            b"\"REFERENCES\" UTF-8 ALL".as_slice(),
            b"REFERENCES {5}\r\nUTF-8 ALL".as_slice(),
            b"REFERENCES UTF-8".as_slice(),
            b"REFERENCES UTF-8 CHARSET US-ASCII ALL".as_slice(),
            b"REFERENCES UTF-8 RETURN (COUNT) ALL".as_slice(),
        ] {
            assert!(arguments(input).is_err(), "{input:?}");
        }
    }

    #[test]
    fn validates_official_style_thread_forest_and_yields_events() {
        let value = response(b"THREAD (2)(3 6 (4 23)(44 7 96))").unwrap();
        assert_eq!(value.message_count(), 8);
        assert_eq!(value.list_count(), 4);
        assert_eq!(value.maximum_depth(), 2);
        assert_eq!(
            value.events().collect::<Vec<_>>(),
            [
                ThreadEvent::ListStart,
                ThreadEvent::Message(2),
                ThreadEvent::ListEnd,
                ThreadEvent::ListStart,
                ThreadEvent::Message(3),
                ThreadEvent::Message(6),
                ThreadEvent::ListStart,
                ThreadEvent::Message(4),
                ThreadEvent::Message(23),
                ThreadEvent::ListEnd,
                ThreadEvent::ListStart,
                ThreadEvent::Message(44),
                ThreadEvent::Message(7),
                ThreadEvent::Message(96),
                ThreadEvent::ListEnd,
                ThreadEvent::ListEnd,
            ]
        );
        assert!(response(b"THREAD").unwrap().events().next().is_none());
        assert!(response(b"THREAD ((1)(2))").is_ok());
    }

    #[test]
    fn rejects_ambiguous_or_malformed_thread_trees() {
        for input in [
            b"THREAD ".as_slice(),
            b"THREAD ()".as_slice(),
            b"THREAD (0)".as_slice(),
            b"THREAD (01)".as_slice(),
            b"THREAD (4294967296)".as_slice(),
            b"THREAD ((1))".as_slice(),
            b"THREAD (1 2 (3))".as_slice(),
            b"THREAD (1(2)(3))".as_slice(),
            b"THREAD ((1) (2))".as_slice(),
            b"THREAD (1) (2)".as_slice(),
        ] {
            assert!(response(input).is_err(), "{input:?}");
        }
    }

    #[test]
    fn enforces_independent_depth_and_node_budgets() {
        let wire = Bytes::from_static(b"THREAD (((1)(2))((3)(4)))");
        let error = ThreadResponse::parse_with_limits(&wire, 2, 100).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::NestingTooDeep);
        assert!(ThreadResponse::parse_with_limits(&wire, 3, 11).is_ok());
        let error = ThreadResponse::parse_with_limits(&wire, 3, 10).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::FrameTooLarge);

        let mut at_limit = Vec::from(b"THREAD ".as_slice());
        at_limit.extend_from_slice(&nested_thread_list(DEFAULT_THREAD_MAX_DEPTH));
        assert!(
            ThreadResponse::parse_with_limits(&Bytes::from(at_limit), usize::MAX, usize::MAX)
                .is_ok()
        );
        let mut above_limit = Vec::from(b"THREAD ".as_slice());
        above_limit.extend_from_slice(&nested_thread_list(DEFAULT_THREAD_MAX_DEPTH + 1));
        let error =
            ThreadResponse::parse_with_limits(&Bytes::from(above_limit), usize::MAX, usize::MAX)
                .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::NestingTooDeep);
    }

    #[test]
    fn encoding_is_exact() {
        let value = response(b"THREAD (1)(2 3)").unwrap();
        let mut encoded = BytesMut::new();
        value.encode(&mut encoded);
        assert_eq!(&encoded[..], value.as_bytes().as_ref());
    }
}
