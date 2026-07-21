use mail_protocol_core::{ErrorKind, ProtocolError};

use crate::{
    AStringKind, SequenceSetRef,
    astring::{parse_astring_prefix, validate_astring},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LiteralPolicy {
    AllowNonSynchronizing,
    RejectNonSynchronizing,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ParsedTaggedExtValue {
    pub(crate) end: usize,
    pub(crate) nesting_depth: usize,
}

pub(crate) fn validate_label(label: &[u8]) -> Result<(), ProtocolError> {
    let Some(first) = label.first() else {
        return Err(invalid("empty IMAP tagged extension label"));
    };
    if !(first.is_ascii_alphabetic() || matches!(first, b'-' | b'_' | b'.'))
        || label.iter().skip(1).any(|byte| {
            !(byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
        })
    {
        return Err(invalid("IMAP tagged extension label"));
    }
    Ok(())
}

pub(crate) fn parse_value(
    input: &[u8],
    start: usize,
    max_depth: usize,
    literal_policy: LiteralPolicy,
) -> Result<ParsedTaggedExtValue, ProtocolError> {
    let Some(first) = input.get(start) else {
        return Err(invalid("missing IMAP tagged extension value").at(start));
    };
    if *first == b'(' {
        parse_list(input, start, max_depth, literal_policy)
    } else {
        parse_simple(input, start)
    }
}

fn parse_list(
    input: &[u8],
    start: usize,
    max_depth: usize,
    literal_policy: LiteralPolicy,
) -> Result<ParsedTaggedExtValue, ProtocolError> {
    let mut cursor = start;
    let mut depth = 0usize;
    let mut greatest_depth = 0usize;
    let mut expecting_component = true;
    let mut allow_empty = false;
    loop {
        let Some(byte) = input.get(cursor) else {
            return Err(invalid("IMAP tagged extension list terminator").at(cursor));
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
                    allow_empty = depth == 1;
                }
                b')' if allow_empty => {
                    cursor += 1;
                    depth -= 1;
                    if depth == 0 {
                        return Ok(ParsedTaggedExtValue {
                            end: cursor,
                            nesting_depth: greatest_depth,
                        });
                    }
                    expecting_component = false;
                    allow_empty = false;
                }
                b' ' | b')' => {
                    return Err(invalid("IMAP tagged extension list separator").at(cursor));
                }
                _ => {
                    cursor = parse_component(input, cursor, literal_policy)?;
                    expecting_component = false;
                    allow_empty = false;
                }
            }
        } else {
            match byte {
                b')' => {
                    cursor += 1;
                    depth -= 1;
                    if depth == 0 {
                        return Ok(ParsedTaggedExtValue {
                            end: cursor,
                            nesting_depth: greatest_depth,
                        });
                    }
                }
                b' ' => {
                    cursor += 1;
                    expecting_component = true;
                    allow_empty = false;
                }
                _ => {
                    return Err(invalid("IMAP tagged extension list terminator").at(cursor));
                }
            }
        }
    }
}

fn parse_simple(input: &[u8], start: usize) -> Result<ParsedTaggedExtValue, ProtocolError> {
    let end = input[start..]
        .iter()
        .position(|byte| matches!(byte, b' ' | b')'))
        .map_or(input.len(), |offset| start + offset);
    let value = &input[start..end];
    if value.is_empty()
        || SequenceSetRef::parse(value).is_err() && validate_number64(value).is_err()
    {
        return Err(invalid("IMAP tagged extension simple value").at(start));
    }
    Ok(ParsedTaggedExtValue {
        end,
        nesting_depth: 0,
    })
}

fn parse_component(
    input: &[u8],
    start: usize,
    literal_policy: LiteralPolicy,
) -> Result<usize, ProtocolError> {
    match input.get(start) {
        Some(b'\"' | b'{') => {
            let parsed =
                parse_astring_prefix(&input[start..]).map_err(|error| shift_error(error, start))?;
            if literal_policy == LiteralPolicy::RejectNonSynchronizing
                && matches!(
                    parsed.kind,
                    AStringKind::Literal {
                        non_synchronizing: true
                    }
                )
            {
                return Err(invalid("non-synchronizing IMAP server literal").at(start));
            }
            Ok(start + parsed.end)
        }
        None | Some(b' ' | b'(' | b')') => {
            Err(invalid("empty IMAP tagged extension component").at(start))
        }
        Some(_) => {
            let end = input[start..]
                .iter()
                .position(|byte| matches!(byte, b' ' | b')'))
                .map_or(input.len(), |offset| start + offset);
            validate_astring(&input[start..end]).map_err(|error| shift_error(error, start))?;
            Ok(end)
        }
    }
}

fn validate_number64(value: &[u8]) -> Result<(), ProtocolError> {
    if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
        return Err(invalid("IMAP tagged extension number64"));
    }
    let number = value.iter().try_fold(0i64, |number, digit| {
        number
            .checked_mul(10)
            .and_then(|number| number.checked_add(i64::from(*digit - b'0')))
    });
    if number.is_some() {
        Ok(())
    } else {
        Err(invalid("IMAP tagged extension number64"))
    }
}

fn shift_error(error: ProtocolError, start: usize) -> ProtocolError {
    error.at(start.saturating_add(error.offset().unwrap_or(0)))
}

const fn invalid(context: &'static str) -> ProtocolError {
    ProtocolError::new(ErrorKind::InvalidSyntax, context)
}

fn nesting_too_deep(offset: usize) -> ProtocolError {
    ProtocolError::new(ErrorKind::NestingTooDeep, "IMAP tagged extension nesting").at(offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_labels_and_simple_values() {
        for label in [b"X".as_slice(), b"-VENDOR", b"_x.1:y"] {
            validate_label(label).unwrap();
        }
        for label in [b"".as_slice(), b"1BAD", b"X+BAD", b"X\xff"] {
            assert!(validate_label(label).is_err(), "{label:?}");
        }
        for value in [b"0".as_slice(), b"01", b"1:4,*", b"$"] {
            assert_eq!(
                parse_value(value, 0, 0, LiteralPolicy::AllowNonSynchronizing)
                    .unwrap()
                    .end,
                value.len()
            );
        }
        for value in [b"".as_slice(), b"atom", b"9223372036854775808"] {
            assert!(
                parse_value(value, 0, 0, LiteralPolicy::AllowNonSynchronizing).is_err(),
                "{value:?}"
            );
        }
    }

    #[test]
    fn list_parser_is_iterative_bounded_and_literal_aware() {
        let value = b"(one \"two\" (three {4}\r\nfour) ())";
        assert!(parse_value(value, 0, 4, LiteralPolicy::AllowNonSynchronizing).is_err());

        let value = b"(one \"two\" (three {4}\r\nfour)) trailing";
        let parsed = parse_value(value, 0, 2, LiteralPolicy::AllowNonSynchronizing).unwrap();
        assert_eq!(&value[..parsed.end], b"(one \"two\" (three {4}\r\nfour))");
        assert_eq!(parsed.nesting_depth, 2);
        assert_eq!(
            parse_value(value, 0, 1, LiteralPolicy::AllowNonSynchronizing)
                .unwrap_err()
                .kind(),
            ErrorKind::NestingTooDeep
        );

        assert!(parse_value(b"({1+}\r\nx)", 0, 1, LiteralPolicy::RejectNonSynchronizing,).is_err());
        assert!(parse_value(b"({1+}\r\nx)", 0, 1, LiteralPolicy::AllowNonSynchronizing,).is_ok());
    }

    #[test]
    fn explicit_large_budget_does_not_use_the_call_stack() {
        const DEPTH: usize = 4_096;
        let mut value = Vec::with_capacity(DEPTH * 2 + 1);
        value.extend(std::iter::repeat_n(b'(', DEPTH));
        value.push(b'x');
        value.extend(std::iter::repeat_n(b')', DEPTH));
        let parsed = parse_value(&value, 0, DEPTH, LiteralPolicy::AllowNonSynchronizing).unwrap();
        assert_eq!(parsed.end, value.len());
        assert_eq!(parsed.nesting_depth, DEPTH);
    }
}
