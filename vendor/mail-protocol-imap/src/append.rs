use std::ops::Range;

use bytes::Bytes;
use mail_protocol_core::{ErrorKind, ProtocolError};

use crate::{
    AStringKind, Command, CommandBody,
    astring::{ParsedAString, parse_astring_prefix},
};

pub(crate) const DATE_TIME_LEN: usize = 28;

/// Validated RFC 9051 arguments following an APPEND mailbox.
///
/// The optional flag list, optional internal date, literal marker, and message
/// payload are views into one shared [`Bytes`] allocation.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct AppendArguments {
    wire: Bytes,
    flag_list: Option<Range<usize>>,
    internal_date: Option<Range<usize>>,
    message_literal: Range<usize>,
    message: Range<usize>,
    non_synchronizing: bool,
}

impl AppendArguments {
    /// Parses exactly one base RFC 9051 APPEND argument sequence.
    ///
    /// # Errors
    ///
    /// Returns an error unless the value has an optional valid flag list, an
    /// optional fixed-format `date-time`, and exactly one normal literal.
    pub fn parse(wire: &Bytes) -> Result<Self, ProtocolError> {
        let parsed = parse_append_arguments(wire)?;
        Ok(Self {
            wire: wire.clone(),
            flag_list: parsed.flag_list,
            internal_date: parsed.internal_date,
            message_literal: parsed.message_literal,
            message: parsed.message,
            non_synchronizing: parsed.non_synchronizing,
        })
    }

    /// Returns the complete bytes following the mailbox separator.
    pub const fn as_bytes(&self) -> &Bytes {
        &self.wire
    }

    /// Returns the optional parenthesized flag list in wire form.
    pub fn flag_list(&self) -> Option<&[u8]> {
        self.flag_list
            .as_ref()
            .map(|range| &self.wire[range.clone()])
    }

    /// Iterates over validated flags without allocating a collection.
    pub fn flags(&self) -> AppendFlags<'_> {
        let remaining = self
            .flag_list
            .as_ref()
            .map_or(&[][..], |range| &self.wire[range.start + 1..range.end - 1]);
        AppendFlags { remaining }
    }

    /// Returns the optional quoted fixed-format internal date.
    pub fn internal_date(&self) -> Option<&[u8]> {
        self.internal_date
            .as_ref()
            .map(|range| &self.wire[range.clone()])
    }

    /// Returns the message literal including its marker and marker CRLF.
    pub fn message_literal(&self) -> &[u8] {
        &self.wire[self.message_literal.clone()]
    }

    /// Returns only the APPEND message payload.
    pub fn message(&self) -> &[u8] {
        &self.wire[self.message.clone()]
    }

    /// Returns whether the message uses the `{n+}` form.
    pub const fn is_non_synchronizing(&self) -> bool {
        self.non_synchronizing
    }
}

/// Allocation-free iterator over an APPEND flag list.
#[derive(Clone, Debug)]
pub struct AppendFlags<'a> {
    remaining: &'a [u8],
}

impl<'a> Iterator for AppendFlags<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining.is_empty() {
            return None;
        }
        let end = self
            .remaining
            .iter()
            .position(|byte| *byte == b' ')
            .unwrap_or(self.remaining.len());
        let flag = &self.remaining[..end];
        self.remaining = if end == self.remaining.len() {
            &[]
        } else {
            &self.remaining[end + 1..]
        };
        Some(flag)
    }
}

impl Command {
    /// Parses APPEND flags, internal date, and message literal.
    ///
    /// # Errors
    ///
    /// Returns an error if a manually constructed APPEND contains invalid
    /// base RFC 9051 arguments. Decoded commands have already been validated.
    pub fn parsed_append_arguments(&self) -> Result<Option<AppendArguments>, ProtocolError> {
        match &self.body {
            CommandBody::Append { arguments, .. } => AppendArguments::parse(arguments).map(Some),
            _ => Ok(None),
        }
    }
}

pub(crate) fn validate_append_arguments(input: &[u8]) -> Result<(), ProtocolError> {
    parse_append_arguments(input).map(|_| ())
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ParsedAppend {
    flag_list: Option<Range<usize>>,
    internal_date: Option<Range<usize>>,
    message_literal: Range<usize>,
    message: Range<usize>,
    non_synchronizing: bool,
}

fn parse_append_arguments(input: &[u8]) -> Result<ParsedAppend, ProtocolError> {
    if input.is_empty() {
        return Err(invalid("IMAP APPEND arguments"));
    }

    let mut cursor = 0;
    let flag_list = if input.first() == Some(&b'(') {
        let end = parse_flag_list(
            input,
            "unterminated IMAP APPEND flag list",
            "IMAP APPEND flag",
        )?;
        cursor = required_space(input, end, "IMAP APPEND flag separator")?;
        Some(0..end)
    } else {
        None
    };

    let internal_date = if input.get(cursor) == Some(&b'"') {
        validate_date_time(&input[cursor..])?;
        let range = cursor..cursor + DATE_TIME_LEN;
        cursor = required_space(input, cursor + DATE_TIME_LEN, "IMAP APPEND date separator")?;
        Some(range)
    } else {
        None
    };

    let ParsedAString { end, content, kind } = parse_astring_prefix(&input[cursor..])?;
    let AStringKind::Literal { non_synchronizing } = kind else {
        return Err(invalid("IMAP APPEND message literal"));
    };
    if cursor + end != input.len() {
        return Err(invalid("trailing IMAP APPEND arguments").at(cursor + end));
    }

    Ok(ParsedAppend {
        flag_list,
        internal_date,
        message_literal: cursor..cursor + end,
        message: cursor + content.start..cursor + content.end,
        non_synchronizing,
    })
}

fn parse_flag_list(
    input: &[u8],
    unterminated_context: &'static str,
    flag_context: &'static str,
) -> Result<usize, ProtocolError> {
    let Some(relative_end) = input[1..].iter().position(|byte| *byte == b')') else {
        return Err(invalid(unterminated_context));
    };
    let end = relative_end + 2;
    let flags = &input[1..end - 1];
    if !flags.is_empty() {
        for flag in flags.split(|byte| *byte == b' ') {
            validate_flag(flag, flag_context)?;
        }
    }
    Ok(end)
}

/// Validates the `flag-list / (flag *(SP flag))` argument of RFC 9051 STORE.
pub(crate) fn validate_store_flags(input: &[u8]) -> Result<(), ProtocolError> {
    if input.first() == Some(&b'(') {
        let end = parse_flag_list(
            input,
            "unterminated IMAP STORE flag list",
            "IMAP STORE flag",
        )?;
        if end != input.len() {
            return Err(invalid("trailing IMAP STORE flags").at(end));
        }
        return Ok(());
    }

    if input.is_empty() {
        return Err(invalid("IMAP STORE flags"));
    }
    for flag in input.split(|byte| *byte == b' ') {
        validate_flag(flag, "IMAP STORE flag")?;
    }
    Ok(())
}

fn validate_flag(flag: &[u8], context: &'static str) -> Result<(), ProtocolError> {
    let atom = if flag.first() == Some(&b'\\') {
        &flag[1..]
    } else {
        flag
    };
    if atom.is_empty()
        || atom.iter().any(|byte| {
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

pub(crate) fn validate_date_time(input: &[u8]) -> Result<(), ProtocolError> {
    let Some(value) = input.get(..DATE_TIME_LEN) else {
        return Err(invalid("truncated IMAP APPEND date-time"));
    };
    let day_valid = (value[1] == b' ' && value[2].is_ascii_digit())
        || (value[1].is_ascii_digit() && value[2].is_ascii_digit());
    let month_valid = [
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
    .any(|month| value[4..7].eq_ignore_ascii_case(month));
    let digit_positions = [8, 9, 10, 11, 13, 14, 16, 17, 19, 20, 23, 24, 25, 26];
    if value[0] != b'"'
        || !day_valid
        || value[3] != b'-'
        || !month_valid
        || value[7] != b'-'
        || value[12] != b' '
        || value[15] != b':'
        || value[18] != b':'
        || value[21] != b' '
        || !matches!(value[22], b'+' | b'-')
        || value[27] != b'"'
        || digit_positions
            .iter()
            .any(|position| !value[*position].is_ascii_digit())
    {
        Err(invalid("IMAP APPEND date-time"))
    } else {
        Ok(())
    }
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

const fn invalid(context: &'static str) -> ProtocolError {
    ProtocolError::new(ErrorKind::InvalidSyntax, context)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_base_append_shape_without_copying_message() {
        let cases = [
            (b"{4}\r\ntest".as_slice(), 0, None),
            (b"() {0}\r\n", 0, Some(b"()".as_slice())),
            (
                b"(\\Seen custom) {4+}\r\ntest",
                2,
                Some(b"(\\Seen custom)".as_slice()),
            ),
            (b"\" 7-Feb-2026 01:02:03 +0800\" {1}\r\nx", 0, None),
            (
                b"(\\Draft) \"17-jUl-2026 23:59:59 -1130\" {3}\r\na\r\n",
                1,
                Some(b"(\\Draft)".as_slice()),
            ),
        ];

        for (wire, flag_count, flag_list) in cases {
            let wire = Bytes::copy_from_slice(wire);
            let pointer = wire.as_ptr();
            let parsed = AppendArguments::parse(&wire).unwrap();
            assert_eq!(parsed.as_bytes().as_ptr(), pointer);
            assert_eq!(parsed.flags().count(), flag_count);
            assert_eq!(parsed.flag_list(), flag_list);
            assert_eq!(
                parsed.message().len(),
                parsed.message_literal().len().saturating_sub(
                    parsed
                        .message_literal()
                        .iter()
                        .position(|b| *b == b'\n')
                        .unwrap()
                        + 1
                )
            );
        }
    }

    #[test]
    fn exposes_flags_date_and_literal_payload() {
        let wire = Bytes::from_static(
            b"(\\Seen $Forwarded) \"17-Jul-2026 12:34:56 +0800\" {5+}\r\na\r\nbc",
        );
        let parsed = AppendArguments::parse(&wire).unwrap();
        assert_eq!(
            parsed.flags().collect::<Vec<_>>(),
            vec![b"\\Seen".as_slice(), b"$Forwarded"]
        );
        assert_eq!(
            parsed.internal_date(),
            Some(b"\"17-Jul-2026 12:34:56 +0800\"".as_slice())
        );
        assert_eq!(parsed.message(), b"a\r\nbc");
        assert!(parsed.is_non_synchronizing());
    }

    #[test]
    fn rejects_malformed_append_arguments() {
        for wire in [
            b"atom".as_slice(),
            b"{1}\r\n\0",
            b"{1}\r\nxy",
            b"(\\Seen  custom) {1}\r\nx",
            b"(\\) {1}\r\nx",
            b"(\\Seen)\t{1}\r\nx",
            b"\"17-Not-2026 12:34:56 +0800\" {1}\r\nx",
            b"\"17-Jul-2026 12:34:56 +0800\"  {1}\r\nx",
            b"\"17-Jul-2026 12:34:56 +0800\" \"x\"",
        ] {
            assert!(
                AppendArguments::parse(&Bytes::copy_from_slice(wire)).is_err(),
                "{wire:?}"
            );
        }
    }

    #[test]
    fn store_flags_require_complete_single_sp_flag_grammar() {
        for valid in [
            b"\\Seen".as_slice(),
            b"$Junk custom",
            b"()",
            b"(\\Seen $Forwarded custom)",
        ] {
            assert!(validate_store_flags(valid).is_ok(), "{valid:?}");
        }

        for invalid in [
            b"".as_slice(),
            b"\\",
            b"\\Seen  $Junk",
            b"\\Seen ",
            b" \\Seen",
            b"\\Seen\t$Junk",
            b"(\\Seen  $Junk)",
            b"(\\Seen )",
            b"( \\Seen)",
            b"(\\Seen) trailing",
            b"(\\Seen",
            b"atom(",
            b"\xff",
        ] {
            assert!(validate_store_flags(invalid).is_err(), "{invalid:?}");
        }
    }
}
