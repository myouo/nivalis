use mail_protocol_core::ProtocolError;

use super::super::{
    FetchString, FetchStringKind, MAX_NUMBER, MAX_NUMBER64, invalid, nesting_too_deep,
    parse_envelope, parse_nstring, parse_number_token, parse_string, required_space,
};
use super::types::BodyStructure;

#[derive(Clone, Copy, Debug)]
enum BodyState {
    Enter,
    MultipartAfterChild,
    MessageAfterChild,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum BodyClass {
    Basic,
    Text,
    Message,
}

pub(in crate::fetch_response) struct ParsedBody<'a> {
    pub(in crate::fetch_response) value: BodyStructure<'a>,
    pub(in crate::fetch_response) end: usize,
    pub(in crate::fetch_response) nesting_depth: usize,
}

#[allow(clippy::too_many_lines)]
pub(in crate::fetch_response) fn parse_body(
    input: &[u8],
    start: usize,
    max_depth: usize,
    extensible: bool,
) -> Result<ParsedBody<'_>, ProtocolError> {
    let mut stack = Vec::with_capacity(max_depth.min(16));
    stack.push(BodyState::Enter);
    let mut cursor = start;
    let mut greatest_depth = 0usize;

    loop {
        let state = *stack
            .last()
            .expect("BODY parser keeps one state until completion");
        match state {
            BodyState::Enter => {
                let depth = stack.len();
                if depth > max_depth {
                    return Err(nesting_too_deep(cursor));
                }
                greatest_depth = greatest_depth.max(depth);
                if input.get(cursor) != Some(&b'(') {
                    return Err(invalid("IMAP body structure opener").at(cursor));
                }
                cursor += 1;
                if input.get(cursor) == Some(&b'(') {
                    *stack.last_mut().expect("BODY state exists") = BodyState::MultipartAfterChild;
                    stack.push(BodyState::Enter);
                    continue;
                }

                let media_type = parse_string(input, cursor)?;
                cursor = required_space(input, media_type.end, "IMAP body type separator")?;
                let subtype = parse_string(input, cursor)?;
                let class = classify_body(media_type.value, subtype.value)?;
                cursor = required_space(input, subtype.end, "IMAP body subtype separator")?;
                cursor = parse_body_parameter(input, cursor)?;
                cursor = required_space(input, cursor, "IMAP body parameter separator")?;
                cursor = parse_nstring(input, cursor)?.end;
                cursor = required_space(input, cursor, "IMAP body id separator")?;
                cursor = parse_nstring(input, cursor)?.end;
                cursor = required_space(input, cursor, "IMAP body description separator")?;
                cursor = parse_string(input, cursor)?.end;
                cursor = required_space(input, cursor, "IMAP body encoding separator")?;
                let (_, octets_end) = parse_number_token(input, cursor, MAX_NUMBER, false, false)?;
                cursor = octets_end;

                match class {
                    BodyClass::Basic => {
                        let finished =
                            finish_body(input, cursor, extensible, false, depth, max_depth)?;
                        cursor = finished.end;
                        greatest_depth = greatest_depth.max(finished.nesting_depth);
                        stack.pop();
                    }
                    BodyClass::Text => {
                        cursor = required_space(input, cursor, "IMAP text body line separator")?;
                        let (_, lines_end) =
                            parse_number_token(input, cursor, MAX_NUMBER64, false, false)?;
                        let finished =
                            finish_body(input, lines_end, extensible, false, depth, max_depth)?;
                        cursor = finished.end;
                        greatest_depth = greatest_depth.max(finished.nesting_depth);
                        stack.pop();
                    }
                    BodyClass::Message => {
                        cursor = required_space(input, cursor, "IMAP message envelope separator")?;
                        cursor = parse_envelope(input, cursor)?.end;
                        cursor = required_space(input, cursor, "IMAP embedded body separator")?;
                        *stack.last_mut().expect("BODY state exists") =
                            BodyState::MessageAfterChild;
                        stack.push(BodyState::Enter);
                    }
                }
            }
            BodyState::MultipartAfterChild => {
                if input.get(cursor) == Some(&b'(') {
                    stack.push(BodyState::Enter);
                    continue;
                }
                let depth = stack.len();
                cursor = required_space(input, cursor, "IMAP multipart subtype separator")?;
                cursor = parse_string(input, cursor)?.end;
                let finished = finish_body(input, cursor, extensible, true, depth, max_depth)?;
                cursor = finished.end;
                greatest_depth = greatest_depth.max(finished.nesting_depth);
                stack.pop();
            }
            BodyState::MessageAfterChild => {
                let depth = stack.len();
                cursor = required_space(input, cursor, "IMAP message body line separator")?;
                let (_, lines_end) = parse_number_token(input, cursor, MAX_NUMBER64, false, false)?;
                let finished = finish_body(input, lines_end, extensible, false, depth, max_depth)?;
                cursor = finished.end;
                greatest_depth = greatest_depth.max(finished.nesting_depth);
                stack.pop();
            }
        }

        if stack.is_empty() {
            return Ok(ParsedBody {
                value: BodyStructure {
                    wire: &input[start..cursor],
                    extensible,
                    nesting_depth: greatest_depth,
                },
                end: cursor,
                nesting_depth: greatest_depth,
            });
        }
    }
}

pub(super) fn classify_body(
    media_type: FetchString<'_>,
    subtype: FetchString<'_>,
) -> Result<BodyClass, ProtocolError> {
    let media = media_type.decoded();
    let subtype_value = subtype.decoded();
    if media_type.kind() == FetchStringKind::Quoted && media.eq_ignore_ascii_case(b"TEXT") {
        return Ok(BodyClass::Text);
    }
    let message = media.eq_ignore_ascii_case(b"MESSAGE");
    let embedded = subtype_value.eq_ignore_ascii_case(b"RFC822")
        || subtype_value.eq_ignore_ascii_case(b"GLOBAL");
    if message && embedded {
        if media_type.kind() == FetchStringKind::Quoted && subtype.kind() == FetchStringKind::Quoted
        {
            Ok(BodyClass::Message)
        } else {
            Err(invalid("IMAP MESSAGE body media form"))
        }
    } else {
        Ok(BodyClass::Basic)
    }
}

pub(super) fn parse_body_parameter(input: &[u8], start: usize) -> Result<usize, ProtocolError> {
    if input
        .get(start..start + 3)
        .is_some_and(|value| value.eq_ignore_ascii_case(b"NIL"))
    {
        return Ok(start + 3);
    }
    if input.get(start) != Some(&b'(') {
        return Err(invalid("IMAP body parameter list").at(start));
    }
    let mut cursor = start + 1;
    loop {
        cursor = parse_string(input, cursor)?.end;
        cursor = required_space(input, cursor, "IMAP body parameter pair separator")?;
        cursor = parse_string(input, cursor)?.end;
        match input.get(cursor) {
            Some(b')') => return Ok(cursor + 1),
            Some(b' ')
                if input
                    .get(cursor + 1)
                    .is_some_and(|byte| !matches!(byte, b' ' | b')')) =>
            {
                cursor += 1;
            }
            None => return Err(invalid("unterminated IMAP body parameter list").at(cursor)),
            _ => return Err(invalid("IMAP body parameter separator").at(cursor)),
        }
    }
}

fn parse_body_disposition(input: &[u8], start: usize) -> Result<usize, ProtocolError> {
    if input
        .get(start..start + 3)
        .is_some_and(|value| value.eq_ignore_ascii_case(b"NIL"))
    {
        return Ok(start + 3);
    }
    if input.get(start) != Some(&b'(') {
        return Err(invalid("IMAP body disposition").at(start));
    }
    let mut cursor = parse_string(input, start + 1)?.end;
    cursor = required_space(input, cursor, "IMAP body disposition separator")?;
    cursor = parse_body_parameter(input, cursor)?;
    if input.get(cursor) != Some(&b')') {
        return Err(invalid("IMAP body disposition terminator").at(cursor));
    }
    Ok(cursor + 1)
}

pub(super) fn parse_body_language(input: &[u8], start: usize) -> Result<usize, ProtocolError> {
    if input.get(start) != Some(&b'(') {
        return Ok(parse_nstring(input, start)?.end);
    }
    let mut cursor = start + 1;
    loop {
        cursor = parse_string(input, cursor)?.end;
        match input.get(cursor) {
            Some(b')') => return Ok(cursor + 1),
            Some(b' ')
                if input
                    .get(cursor + 1)
                    .is_some_and(|byte| !matches!(byte, b' ' | b')')) =>
            {
                cursor += 1;
            }
            None => return Err(invalid("unterminated IMAP body language list").at(cursor)),
            _ => return Err(invalid("IMAP body language separator").at(cursor)),
        }
    }
}

struct FinishedBody {
    end: usize,
    nesting_depth: usize,
}

fn finish_body(
    input: &[u8],
    mut cursor: usize,
    extensible: bool,
    multipart: bool,
    body_depth: usize,
    max_depth: usize,
) -> Result<FinishedBody, ProtocolError> {
    if input.get(cursor) == Some(&b')') {
        return Ok(FinishedBody {
            end: cursor + 1,
            nesting_depth: body_depth,
        });
    }
    if !extensible {
        return Err(invalid("extension data in non-extensible IMAP BODY").at(cursor));
    }
    cursor = required_space(input, cursor, "IMAP body extension separator")?;
    cursor = if multipart {
        parse_body_parameter(input, cursor)?
    } else {
        parse_nstring(input, cursor)?.end
    };
    if input.get(cursor) == Some(&b')') {
        return Ok(FinishedBody {
            end: cursor + 1,
            nesting_depth: body_depth,
        });
    }

    cursor = required_space(input, cursor, "IMAP body disposition extension separator")?;
    cursor = parse_body_disposition(input, cursor)?;
    if input.get(cursor) == Some(&b')') {
        return Ok(FinishedBody {
            end: cursor + 1,
            nesting_depth: body_depth,
        });
    }

    cursor = required_space(input, cursor, "IMAP body language extension separator")?;
    cursor = parse_body_language(input, cursor)?;
    if input.get(cursor) == Some(&b')') {
        return Ok(FinishedBody {
            end: cursor + 1,
            nesting_depth: body_depth,
        });
    }

    cursor = required_space(input, cursor, "IMAP body location extension separator")?;
    cursor = parse_nstring(input, cursor)?.end;
    let mut greatest_depth = body_depth;
    loop {
        match input.get(cursor) {
            Some(b')') => {
                return Ok(FinishedBody {
                    end: cursor + 1,
                    nesting_depth: greatest_depth,
                });
            }
            Some(b' ') => {
                let value_start =
                    required_space(input, cursor, "IMAP future body extension separator")?;
                let parsed = parse_body_extension(input, value_start, body_depth, max_depth)?;
                cursor = parsed.end;
                greatest_depth = greatest_depth.max(parsed.nesting_depth);
            }
            None => return Err(invalid("unterminated IMAP body structure").at(cursor)),
            _ => return Err(invalid("IMAP body structure terminator").at(cursor)),
        }
    }
}

pub(super) struct ParsedBodyExtension {
    pub(super) end: usize,
    pub(super) nesting_depth: usize,
}

pub(super) fn parse_body_extension(
    input: &[u8],
    start: usize,
    body_depth: usize,
    max_depth: usize,
) -> Result<ParsedBodyExtension, ProtocolError> {
    if input.get(start) != Some(&b'(') {
        return Ok(ParsedBodyExtension {
            end: parse_body_extension_scalar(input, start)?,
            nesting_depth: body_depth,
        });
    }
    let mut cursor = start;
    let mut depth = 0usize;
    let mut greatest_depth = body_depth;
    let mut expecting_value = true;
    loop {
        let Some(byte) = input.get(cursor) else {
            return Err(invalid("unterminated IMAP body extension").at(cursor));
        };
        if expecting_value {
            match byte {
                b'(' => {
                    depth = depth
                        .checked_add(1)
                        .ok_or_else(|| nesting_too_deep(cursor))?;
                    let total_depth = body_depth
                        .checked_add(depth)
                        .ok_or_else(|| nesting_too_deep(cursor))?;
                    if total_depth > max_depth {
                        return Err(nesting_too_deep(cursor));
                    }
                    greatest_depth = greatest_depth.max(total_depth);
                    cursor += 1;
                }
                b' ' | b')' => {
                    return Err(invalid("empty IMAP body extension list").at(cursor));
                }
                _ => {
                    cursor = parse_body_extension_scalar(input, cursor)?;
                    expecting_value = false;
                }
            }
        } else {
            match byte {
                b')' => {
                    cursor += 1;
                    depth -= 1;
                    if depth == 0 {
                        return Ok(ParsedBodyExtension {
                            end: cursor,
                            nesting_depth: greatest_depth,
                        });
                    }
                }
                b' ' => {
                    if input
                        .get(cursor + 1)
                        .is_none_or(|byte| matches!(byte, b' ' | b')'))
                    {
                        return Err(invalid("IMAP body extension separator").at(cursor));
                    }
                    cursor += 1;
                    expecting_value = true;
                }
                _ => return Err(invalid("IMAP body extension terminator").at(cursor)),
            }
        }
    }
}

fn parse_body_extension_scalar(input: &[u8], start: usize) -> Result<usize, ProtocolError> {
    match input.get(start) {
        Some(b'"' | b'{') => parse_string(input, start).map(|value| value.end),
        Some(byte) if byte.is_ascii_digit() => {
            parse_number_token(input, start, MAX_NUMBER64, false, false).map(|(_, end)| end)
        }
        Some(_)
            if input
                .get(start..start + 3)
                .is_some_and(|value| value.eq_ignore_ascii_case(b"NIL")) =>
        {
            Ok(start + 3)
        }
        _ => Err(invalid("IMAP body extension scalar").at(start)),
    }
}
