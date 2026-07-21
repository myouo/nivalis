use std::ops::Range;

use bytes::{Bytes, BytesMut};
use mail_protocol_core::{ErrorKind, ProtocolError};

use crate::astring::parse_astring_prefix;
use crate::tagged_ext::{LiteralPolicy, parse_value, validate_label};
use crate::{AString, Command, CommandBody, Sequence, SequenceSetRef};

/// Default maximum nesting accepted in an unknown SELECT parameter value.
pub const DEFAULT_SELECT_MAX_DEPTH: usize = 64;

/// Maximum number of SELECT parameters accepted in one command.
///
/// The bound keeps duplicate detection deterministic without allocating a
/// collection proportional to attacker-controlled input.
pub const MAX_SELECT_PARAMETERS: usize = 64;

/// Validated, zero-copy SELECT command arguments.
///
/// The value owns one [`Bytes`] backing store. The mailbox and every parameter
/// exposed by its iterators are views into that store.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectArguments {
    wire: Bytes,
    mailbox: AString,
    parameters: Option<Range<usize>>,
    extension_depth: usize,
}

impl SelectArguments {
    /// Parses SELECT arguments using the default extension nesting limit.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid mailbox, malformed parameter list,
    /// duplicate parameter, invalid CONDSTORE/QRESYNC value, or excessive
    /// extension nesting.
    pub fn parse(wire: &Bytes) -> Result<Self, ProtocolError> {
        Self::parse_with_max_depth(wire, DEFAULT_SELECT_MAX_DEPTH)
    }

    /// Parses SELECT arguments with an explicit extension nesting limit.
    ///
    /// The limit applies to values of unknown RFC 4466 SELECT parameters.
    /// QRESYNC has a fixed, non-recursive grammar.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::parse`] and `NestingTooDeep` when an
    /// unknown extension value exceeds `max_depth`.
    pub fn parse_with_max_depth(wire: &Bytes, max_depth: usize) -> Result<Self, ProtocolError> {
        let parsed = validate_select_arguments(wire, max_depth)?;
        let mailbox = AString::parse(&wire.slice(..parsed.mailbox_end))?;
        Ok(Self {
            wire: wire.clone(),
            mailbox,
            parameters: parsed.parameters,
            extension_depth: parsed.extension_depth,
        })
    }

    /// Returns the exact bytes following the SELECT command name.
    pub const fn as_bytes(&self) -> &Bytes {
        &self.wire
    }

    /// Returns the validated mailbox argument.
    pub const fn mailbox(&self) -> &AString {
        &self.mailbox
    }

    /// Returns the complete optional parenthesized parameter list.
    pub fn parameters_bytes(&self) -> Option<&[u8]> {
        self.parameters
            .as_ref()
            .map(|range| &self.wire[range.clone()])
    }

    /// Iterates over typed SELECT parameters without allocating.
    pub fn parameters(&self) -> SelectParameterIter<'_> {
        let remaining = self.parameters.as_ref().map_or(b"".as_slice(), |range| {
            &self.wire[range.start + 1..range.end - 1]
        });
        SelectParameterIter { remaining }
    }

    /// Returns the greatest nesting depth in an unknown extension value.
    pub const fn extension_depth(&self) -> usize {
        self.extension_depth
    }

    /// Appends the validated arguments without re-encoding or copying fields.
    pub fn encode(&self, destination: &mut BytesMut) {
        destination.extend_from_slice(&self.wire);
    }
}

impl Command {
    /// Returns typed arguments for SELECT/EXAMINE, including extension
    /// parameters, without copying decoded frame data.
    ///
    /// # Errors
    ///
    /// Returns an error when a manually constructed base or raw command has
    /// invalid SELECT argument syntax.
    pub fn parsed_select_arguments(&self) -> Result<Option<SelectArguments>, ProtocolError> {
        match &self.body {
            CommandBody::Select { mailbox } | CommandBody::Examine { mailbox } => {
                SelectArguments::parse(mailbox).map(Some)
            }
            CommandBody::SelectExtended { arguments }
            | CommandBody::ExamineExtended { arguments } => Ok(Some(arguments.clone())),
            CommandBody::Raw { name, arguments }
                if name.eq_ignore_ascii_case(b"SELECT")
                    || name.eq_ignore_ascii_case(b"EXAMINE") =>
            {
                SelectArguments::parse(arguments).map(Some)
            }
            _ => Ok(None),
        }
    }
}

/// Internal layout returned by the shared borrowed SELECT validator.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ParsedSelectArguments {
    /// End of the mailbox `astring`, which always begins at byte zero.
    pub(crate) mailbox_end: usize,
    /// Complete optional parenthesized parameter list.
    pub(crate) parameters: Option<Range<usize>>,
    /// Greatest nesting depth in an unknown extension parameter.
    pub(crate) extension_depth: usize,
}

/// Validates SELECT arguments directly from a borrowed command frame.
///
/// This is the allocation-free entry point used by `CommandRef`; it shares
/// the exact parser used to construct [`SelectArguments`] and returns the
/// small layout needed to preserve legacy/extended command variants.
pub(crate) fn validate_select_arguments(
    input: &[u8],
    max_depth: usize,
) -> Result<ParsedSelectArguments, ProtocolError> {
    let mailbox_end = parse_astring_prefix(input)
        .map_err(|error| shift_error(error, 0))?
        .end;
    if mailbox_end == input.len() {
        return Ok(ParsedSelectArguments {
            mailbox_end,
            parameters: None,
            extension_depth: 0,
        });
    }
    if input.get(mailbox_end..mailbox_end + 2) != Some(b" (") {
        return Err(invalid("IMAP SELECT parameter separator").at(mailbox_end));
    }

    let list_start = mailbox_end + 1;
    let mut cursor = list_start + 1;
    if input.get(cursor) == Some(&b')') {
        return Err(invalid("empty IMAP SELECT parameter list").at(cursor));
    }

    let mut labels = [(0usize, 0usize); MAX_SELECT_PARAMETERS];
    let mut label_count = 0usize;
    let mut extension_depth = 0usize;
    loop {
        if label_count == MAX_SELECT_PARAMETERS {
            return Err(invalid("too many IMAP SELECT parameters").at(cursor));
        }
        let parsed = parse_parameter(input, cursor, max_depth)?;
        if labels[..label_count]
            .iter()
            .any(|&(start, end)| input[start..end].eq_ignore_ascii_case(parsed.item.name()))
        {
            return Err(invalid("duplicate IMAP SELECT parameter").at(cursor));
        }
        labels[label_count] = parsed.label;
        label_count += 1;
        extension_depth = extension_depth.max(parsed.extension_depth);
        cursor = parsed.end;

        match input.get(cursor) {
            Some(b')') => {
                cursor += 1;
                break;
            }
            Some(b' ') => cursor += 1,
            _ => return Err(invalid("IMAP SELECT parameter separator").at(cursor)),
        }
        if input.get(cursor) == Some(&b')') {
            return Err(invalid("trailing IMAP SELECT parameter separator").at(cursor));
        }
    }
    if cursor != input.len() {
        return Err(invalid("trailing IMAP SELECT argument data").at(cursor));
    }

    Ok(ParsedSelectArguments {
        mailbox_end,
        parameters: Some(list_start..cursor),
        extension_depth,
    })
}

/// One validated RFC 4466 SELECT parameter.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum SelectParameter<'a> {
    /// RFC 7162 CONDSTORE parameter.
    CondStore,
    /// RFC 7162 QRESYNC parameter.
    QResync(QResyncParameter<'a>),
    /// An extension parameter preserved as exact borrowed wire slices.
    Other {
        /// Extension parameter name.
        name: &'a [u8],
        /// Optional RFC 4466 extension value.
        value: Option<&'a [u8]>,
    },
}

impl<'a> SelectParameter<'a> {
    /// Returns the parameter name.
    pub const fn name(self) -> &'a [u8] {
        match self {
            Self::CondStore => b"CONDSTORE",
            Self::QResync(_) => b"QRESYNC",
            Self::Other { name, .. } => name,
        }
    }
}

/// Allocation-free iterator over validated SELECT parameters.
#[derive(Clone, Debug)]
pub struct SelectParameterIter<'a> {
    remaining: &'a [u8],
}

impl<'a> Iterator for SelectParameterIter<'a> {
    type Item = SelectParameter<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining.is_empty() {
            return None;
        }
        let parsed = match parse_parameter(self.remaining, 0, usize::MAX) {
            Ok(parsed) => parsed,
            Err(error) => {
                debug_assert!(false, "validated SELECT parameter became invalid: {error}");
                self.remaining = b"";
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

/// Typed RFC 7162 QRESYNC SELECT parameter value.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct QResyncParameter<'a> {
    wire: &'a [u8],
    uid_validity: u32,
    mod_sequence: u64,
    known_uids: Option<SequenceSetRef<'a>>,
    sequence_match: Option<SequenceMatchData<'a>>,
}

impl<'a> QResyncParameter<'a> {
    /// Returns the complete parenthesized QRESYNC value.
    pub const fn as_bytes(self) -> &'a [u8] {
        self.wire
    }

    /// Returns the previous UIDVALIDITY value.
    pub const fn uid_validity(self) -> u32 {
        self.uid_validity
    }

    /// Returns the client's last known modification sequence.
    pub const fn mod_sequence(self) -> u64 {
        self.mod_sequence
    }

    /// Returns the optional set of known UIDs.
    pub const fn known_uids(self) -> Option<SequenceSetRef<'a>> {
        self.known_uids
    }

    /// Returns the optional sequence-number to UID correspondence data.
    pub const fn sequence_match(self) -> Option<SequenceMatchData<'a>> {
        self.sequence_match
    }
}

/// RFC 7162 sequence-number to UID correspondence data.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SequenceMatchData<'a> {
    sequence_set: SequenceSetRef<'a>,
    uid_set: SequenceSetRef<'a>,
}

impl<'a> SequenceMatchData<'a> {
    /// Returns the strictly ascending set of known sequence numbers.
    pub const fn sequence_set(self) -> SequenceSetRef<'a> {
        self.sequence_set
    }

    /// Returns the strictly ascending, positionally corresponding UID set.
    pub const fn uid_set(self) -> SequenceSetRef<'a> {
        self.uid_set
    }
}

struct ParsedParameter<'a> {
    item: SelectParameter<'a>,
    label: (usize, usize),
    end: usize,
    extension_depth: usize,
}

fn parse_parameter(
    input: &[u8],
    start: usize,
    max_depth: usize,
) -> Result<ParsedParameter<'_>, ProtocolError> {
    let label_end = input[start..]
        .iter()
        .position(|byte| matches!(byte, b' ' | b')'))
        .map_or(input.len(), |offset| start + offset);
    let label = &input[start..label_end];
    validate_label(label).map_err(|error| shift_error(error, start))?;

    if label.eq_ignore_ascii_case(b"CONDSTORE") {
        return Ok(ParsedParameter {
            item: SelectParameter::CondStore,
            label: (start, label_end),
            end: label_end,
            extension_depth: 0,
        });
    }
    if label.eq_ignore_ascii_case(b"QRESYNC") {
        if input.get(label_end) != Some(&b' ') {
            return Err(invalid("missing IMAP QRESYNC value").at(label_end));
        }
        let (value, end) = parse_qresync(input, label_end + 1)?;
        return Ok(ParsedParameter {
            item: SelectParameter::QResync(value),
            label: (start, label_end),
            end,
            extension_depth: 0,
        });
    }

    let value_start = label_end.saturating_add(1);
    let value = match input.get(label_end..value_start + 1) {
        Some([b' ', b'(' | b'0'..=b'9' | b'*' | b'$']) => {
            let parsed = parse_value(
                input,
                value_start,
                max_depth,
                LiteralPolicy::AllowNonSynchronizing,
            )?;
            Some((value_start, parsed.end, parsed.nesting_depth))
        }
        _ => None,
    };
    let (end, extension_depth) = value.map_or((label_end, 0), |(_, end, depth)| (end, depth));
    Ok(ParsedParameter {
        item: SelectParameter::Other {
            name: label,
            value: value.map(|(start, end, _)| &input[start..end]),
        },
        label: (start, label_end),
        end,
        extension_depth,
    })
}

fn parse_qresync(
    input: &[u8],
    start: usize,
) -> Result<(QResyncParameter<'_>, usize), ProtocolError> {
    if input.get(start) != Some(&b'(') {
        return Err(invalid("IMAP QRESYNC value").at(start));
    }
    let mut cursor = start + 1;
    if input.get(cursor) == Some(&b'0') {
        return Err(invalid("IMAP QRESYNC UIDVALIDITY").at(cursor));
    }
    let (uid_validity, end) = parse_decimal(input, cursor, u64::from(u32::MAX), true)?;
    cursor = require_space(input, end, "IMAP QRESYNC UIDVALIDITY separator")?;
    let (mod_sequence, end) = parse_decimal(input, cursor, i64::MAX as u64, false)?;
    cursor = end;

    let mut known_uids = None;
    let mut sequence_match = None;
    if input.get(cursor) == Some(&b' ') {
        cursor += 1;
        if input.get(cursor) == Some(&b'(') {
            let (value, end) = parse_sequence_match(input, cursor)?;
            sequence_match = Some(value);
            cursor = end;
        } else {
            let end = token_end(input, cursor);
            let value = parse_numeric_set(&input[cursor..end], cursor, false)?;
            known_uids = Some(value);
            cursor = end;
            if input.get(cursor) == Some(&b' ') {
                cursor += 1;
                let (value, end) = parse_sequence_match(input, cursor)?;
                sequence_match = Some(value);
                cursor = end;
            }
        }
    }
    if input.get(cursor) != Some(&b')') {
        return Err(invalid("IMAP QRESYNC value terminator").at(cursor));
    }
    let end = cursor + 1;
    let uid_validity = u32::try_from(uid_validity)
        .map_err(|_| invalid("IMAP QRESYNC UIDVALIDITY boundary").at(start + 1))?;
    Ok((
        QResyncParameter {
            wire: &input[start..end],
            uid_validity,
            mod_sequence,
            known_uids,
            sequence_match,
        },
        end,
    ))
}

fn parse_sequence_match(
    input: &[u8],
    start: usize,
) -> Result<(SequenceMatchData<'_>, usize), ProtocolError> {
    if input.get(start) != Some(&b'(') {
        return Err(invalid("IMAP QRESYNC sequence match data").at(start));
    }
    let first_start = start + 1;
    let first_end = token_end(input, first_start);
    let sequence_set = parse_numeric_set(&input[first_start..first_end], first_start, true)?;
    let second_start = require_space(input, first_end, "IMAP QRESYNC sequence match separator")?;
    let second_end = token_end(input, second_start);
    let uid_set = parse_numeric_set(&input[second_start..second_end], second_start, true)?;
    if input.get(second_end) != Some(&b')') {
        return Err(invalid("IMAP QRESYNC sequence match terminator").at(second_end));
    }
    if expanded_cardinality(sequence_set)? != expanded_cardinality(uid_set)? {
        return Err(invalid("IMAP QRESYNC sequence match cardinality").at(start));
    }
    Ok((
        SequenceMatchData {
            sequence_set,
            uid_set,
        },
        second_end + 1,
    ))
}

fn parse_numeric_set(
    input: &[u8],
    offset: usize,
    require_ascending: bool,
) -> Result<SequenceSetRef<'_>, ProtocolError> {
    let set = SequenceSetRef::parse(input).map_err(|error| shift_error(error, offset))?;
    if set.is_saved_search() || input.contains(&b'*') {
        return Err(invalid("dynamic IMAP QRESYNC sequence set").at(offset));
    }
    if require_ascending {
        let mut previous = None;
        for range in set.ranges() {
            let Sequence::Number(start) = range.start else {
                return Err(invalid("dynamic IMAP QRESYNC sequence set").at(offset));
            };
            let end = match range.end.unwrap_or(range.start) {
                Sequence::Number(end) => end,
                Sequence::Asterisk => {
                    return Err(invalid("dynamic IMAP QRESYNC sequence set").at(offset));
                }
            };
            if start > end || previous.is_some_and(|previous| start <= previous) {
                return Err(invalid("unordered IMAP QRESYNC sequence set").at(offset));
            }
            previous = Some(end);
        }
    }
    Ok(set)
}

fn expanded_cardinality(set: SequenceSetRef<'_>) -> Result<u64, ProtocolError> {
    set.ranges().try_fold(0u64, |total, range| {
        let Sequence::Number(start) = range.start else {
            return Err(invalid("dynamic IMAP QRESYNC sequence set"));
        };
        let end = match range.end.unwrap_or(range.start) {
            Sequence::Number(end) => end,
            Sequence::Asterisk => return Err(invalid("dynamic IMAP QRESYNC sequence set")),
        };
        total
            .checked_add(end - start + 1)
            .ok_or_else(|| invalid("IMAP QRESYNC sequence match cardinality"))
    })
}

fn parse_decimal(
    input: &[u8],
    start: usize,
    maximum: u64,
    non_zero: bool,
) -> Result<(u64, usize), ProtocolError> {
    let end = token_end(input, start);
    let digits = &input[start..end];
    if digits.is_empty()
        || !digits.iter().all(u8::is_ascii_digit)
        || digits.len() > 1 && digits.first() == Some(&b'0')
    {
        return Err(invalid("IMAP QRESYNC numeric value").at(start));
    }
    let value = digits.iter().try_fold(0u64, |value, digit| {
        value
            .checked_mul(10)
            .and_then(|value| value.checked_add(u64::from(*digit - b'0')))
    });
    let Some(value) = value.filter(|value| *value <= maximum && (!non_zero || *value != 0)) else {
        return Err(invalid("IMAP QRESYNC numeric boundary").at(start));
    };
    Ok((value, end))
}

fn token_end(input: &[u8], start: usize) -> usize {
    input[start..]
        .iter()
        .position(|byte| matches!(byte, b' ' | b')'))
        .map_or(input.len(), |offset| start + offset)
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

    fn parse(input: &'static [u8]) -> Result<SelectArguments, ProtocolError> {
        SelectArguments::parse(&Bytes::from_static(input))
    }

    #[test]
    fn parses_condstore_and_complete_qresync_data() {
        let value = parse(
            b"INBOX (CONDSTORE QRESYNC (3857529045 20010715194045000 1:198 (1:5,7 101:105,107)))",
        )
        .unwrap();
        assert_eq!(value.mailbox().decoded().as_ref(), b"INBOX");
        let parameters: Vec<_> = value.parameters().collect();
        assert_eq!(parameters.len(), 2);
        let SelectParameter::QResync(qresync) = parameters[1] else {
            panic!("expected QRESYNC")
        };
        assert_eq!(qresync.uid_validity(), 3_857_529_045);
        assert_eq!(qresync.mod_sequence(), 20_010_715_194_045_000);
        assert_eq!(qresync.known_uids().unwrap().as_bytes(), b"1:198");
        let match_data = qresync.sequence_match().unwrap();
        assert_eq!(match_data.sequence_set().as_bytes(), b"1:5,7");
        assert_eq!(match_data.uid_set().as_bytes(), b"101:105,107");
    }

    #[test]
    fn permits_match_data_without_known_uids_and_bounded_unknown_values() {
        let value = parse(b"Archive (QRESYNC (1 2 (3:4 30:31)) X-OPT (A (B 1)))").unwrap();
        assert_eq!(value.extension_depth(), 2);
        let parameters: Vec<_> = value.parameters().collect();
        let SelectParameter::QResync(qresync) = parameters[0] else {
            panic!("expected QRESYNC")
        };
        assert!(qresync.known_uids().is_none());
        assert!(qresync.sequence_match().is_some());
        assert!(matches!(
            parameters[1],
            SelectParameter::Other {
                name: b"X-OPT",
                value: Some(b"(A (B 1))")
            }
        ));

        let error = SelectArguments::parse_with_max_depth(value.as_bytes(), 1).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::NestingTooDeep);
    }

    #[test]
    fn rejects_duplicates_dynamic_sets_and_invalid_correspondence() {
        for input in [
            b"INBOX (CONDSTORE condstore)".as_slice(),
            b"INBOX (X-A X-a)".as_slice(),
            b"INBOX (QRESYNC (1 2 $))".as_slice(),
            b"INBOX (QRESYNC (1 2 1:*))".as_slice(),
            b"INBOX (QRESYNC (1 2 (2:1 20:21)))".as_slice(),
            b"INBOX (QRESYNC (1 2 (1:2 20)))".as_slice(),
            b"INBOX (QRESYNC (1 2 (1,1 20,21)))".as_slice(),
        ] {
            assert!(parse(input).is_err(), "{input:?}");
        }
    }

    #[test]
    fn enforces_uidvalidity_and_modseq_boundaries() {
        assert!(parse(b"INBOX (QRESYNC (4294967295 9223372036854775807))").is_ok());
        assert!(parse(b"INBOX (QRESYNC (1 0))").is_ok());
        for input in [
            b"INBOX (QRESYNC (0 1))".as_slice(),
            b"INBOX (QRESYNC (01 1))".as_slice(),
            b"INBOX (QRESYNC (4294967296 1))".as_slice(),
            b"INBOX (QRESYNC (1 00))".as_slice(),
            b"INBOX (QRESYNC (1 01))".as_slice(),
            b"INBOX (QRESYNC (1 9223372036854775808))".as_slice(),
        ] {
            assert!(parse(input).is_err(), "{input:?}");
        }
    }

    #[test]
    fn preserves_one_backing_store_and_encodes_exactly() {
        let value = parse(b"{5}\r\nINBOX (X-PARAM 42)").unwrap();
        let mailbox = value.mailbox().as_wire();
        let wire = value.as_bytes();
        assert!(mailbox.as_ptr() >= wire.as_ptr());
        assert!(mailbox.as_ptr() < end_pointer(wire));

        let mut encoded = BytesMut::new();
        value.encode(&mut encoded);
        assert_eq!(&encoded[..], &wire[..]);
    }

    fn end_pointer(bytes: &[u8]) -> *const u8 {
        bytes.as_ptr().wrapping_add(bytes.len())
    }
}
