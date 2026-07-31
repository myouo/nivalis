use mail_protocol_core::ProtocolError;

use super::super::{
    FetchNString, MAX_NUMBER, MAX_NUMBER64, invalid, parse_envelope, parse_nstring,
    parse_number_token, parse_string, required_space,
};
use super::types::{
    BodyDisposition, BodyExtensions, BodyFields, BodyLanguage, BodyParameters, BodyStructure,
    BodyStructureKind, BodyStructureView,
};
use super::validation::{
    BodyClass, classify_body, parse_body, parse_body_language, parse_body_parameter,
};

pub(super) fn analyze_body(
    body: BodyStructure<'_>,
) -> Result<BodyStructureView<'_>, ProtocolError> {
    let input = body.wire;
    let mut cursor = 1usize;
    if input.get(cursor) == Some(&b'(') {
        let parts_start = cursor;
        while input.get(cursor) == Some(&b'(') {
            cursor = parse_body(input, cursor, body.nesting_depth, body.extensible)?.end;
        }
        let parts_end = cursor;
        cursor = required_space(input, cursor, "IMAP multipart subtype separator")?;
        let subtype = parse_string(input, cursor)?;
        let extensions = body_extension_slice(input, subtype.end)?;
        return Ok(BodyStructureView {
            kind: BodyStructureKind::Multipart,
            media_type: None,
            parts: &input[parts_start..parts_end],
            subtype: subtype.value,
            fields: None,
            lines: None,
            envelope: None,
            embedded_body: None,
            extensions,
            extensible: body.extensible,
            max_depth: body.nesting_depth,
        });
    }

    let media_type = parse_string(input, cursor)?;
    cursor = required_space(input, media_type.end, "IMAP body type separator")?;
    let subtype = parse_string(input, cursor)?;
    let class = classify_body(media_type.value, subtype.value)?;
    cursor = required_space(input, subtype.end, "IMAP body subtype separator")?;

    let parameter_start = cursor;
    cursor = parse_body_parameter(input, cursor)?;
    let parameters = if input[parameter_start..cursor].eq_ignore_ascii_case(b"NIL") {
        BodyParameters::Nil
    } else {
        BodyParameters::List(&input[parameter_start + 1..cursor - 1])
    };
    cursor = required_space(input, cursor, "IMAP body parameter separator")?;
    let id = parse_nstring(input, cursor)?;
    cursor = required_space(input, id.end, "IMAP body id separator")?;
    let description = parse_nstring(input, cursor)?;
    cursor = required_space(input, description.end, "IMAP body description separator")?;
    let encoding = parse_string(input, cursor)?;
    cursor = required_space(input, encoding.end, "IMAP body encoding separator")?;
    let (octets, octets_end) = parse_number_token(input, cursor, MAX_NUMBER, false, false)?;
    cursor = octets_end;
    let fields = BodyFields {
        parameters,
        id: id.value,
        description: description.value,
        encoding: encoding.value,
        octets: u32::try_from(octets).expect("body octets fit u32"),
    };

    let (kind, lines, envelope, embedded_body) = match class {
        BodyClass::Basic => (BodyStructureKind::Basic, None, None, None),
        BodyClass::Text => {
            cursor = required_space(input, cursor, "IMAP text body line separator")?;
            let (lines, end) = parse_number_token(input, cursor, MAX_NUMBER64, false, false)?;
            cursor = end;
            (BodyStructureKind::Text, Some(lines), None, None)
        }
        BodyClass::Message => {
            cursor = required_space(input, cursor, "IMAP message envelope separator")?;
            let parsed_envelope = parse_envelope(input, cursor)?;
            cursor = required_space(input, parsed_envelope.end, "IMAP embedded body separator")?;
            let parsed_body = parse_body(input, cursor, body.nesting_depth, body.extensible)?;
            cursor = required_space(input, parsed_body.end, "IMAP message body line separator")?;
            let (lines, end) = parse_number_token(input, cursor, MAX_NUMBER64, false, false)?;
            cursor = end;
            (
                BodyStructureKind::Message,
                Some(lines),
                Some(parsed_envelope.value),
                Some(parsed_body.value),
            )
        }
    };
    let extensions = body_extension_slice(input, cursor)?;
    Ok(BodyStructureView {
        kind,
        media_type: Some(media_type.value),
        subtype: subtype.value,
        fields: Some(fields),
        lines,
        envelope,
        embedded_body,
        parts: b"",
        extensions,
        extensible: body.extensible,
        max_depth: body.nesting_depth,
    })
}

fn body_extension_slice(input: &[u8], cursor: usize) -> Result<Option<&[u8]>, ProtocolError> {
    let terminator = input.len() - 1;
    if cursor == terminator {
        Ok(None)
    } else if input.get(cursor) == Some(&b' ') {
        Ok(Some(&input[cursor + 1..terminator]))
    } else {
        Err(invalid("IMAP body extension boundary").at(cursor))
    }
}

pub(super) fn parse_body_extensions_view(
    input: &[u8],
    kind: BodyStructureKind,
) -> Result<BodyExtensions<'_>, ProtocolError> {
    let multipart = kind == BodyStructureKind::Multipart;
    let mut cursor;
    let (md5, parameters) = if multipart {
        cursor = parse_body_parameter(input, 0)?;
        (None, Some(body_parameters_view(&input[..cursor])?))
    } else {
        let value = parse_nstring(input, 0)?;
        cursor = value.end;
        (Some(value.value), None)
    };
    let mut result = BodyExtensions {
        md5,
        parameters,
        disposition: None,
        language: None,
        location: None,
        future: b"",
    };
    if cursor == input.len() {
        return Ok(result);
    }

    cursor = required_space(input, cursor, "validated body disposition separator")?;
    let disposition = parse_body_disposition_view(input, cursor)?;
    result.disposition = Some(disposition.value);
    cursor = disposition.end;
    if cursor == input.len() {
        return Ok(result);
    }

    cursor = required_space(input, cursor, "validated body language separator")?;
    let language = parse_body_language_view(input, cursor)?;
    result.language = Some(language.value);
    cursor = language.end;
    if cursor == input.len() {
        return Ok(result);
    }

    cursor = required_space(input, cursor, "validated body location separator")?;
    let location = parse_nstring(input, cursor)?;
    result.location = Some(location.value);
    cursor = location.end;
    if cursor == input.len() {
        return Ok(result);
    }

    cursor = required_space(input, cursor, "validated future body extension separator")?;
    result.future = &input[cursor..];
    Ok(result)
}

fn body_parameters_view(input: &[u8]) -> Result<BodyParameters<'_>, ProtocolError> {
    if input.eq_ignore_ascii_case(b"NIL") {
        Ok(BodyParameters::Nil)
    } else if input.first() == Some(&b'(') && input.last() == Some(&b')') {
        Ok(BodyParameters::List(&input[1..input.len() - 1]))
    } else {
        Err(invalid("validated IMAP body parameters"))
    }
}

struct ParsedDisposition<'a> {
    value: BodyDisposition<'a>,
    end: usize,
}

fn parse_body_disposition_view(
    input: &[u8],
    start: usize,
) -> Result<ParsedDisposition<'_>, ProtocolError> {
    if input
        .get(start..start + 3)
        .is_some_and(|value| value.eq_ignore_ascii_case(b"NIL"))
    {
        return Ok(ParsedDisposition {
            value: BodyDisposition::Nil,
            end: start + 3,
        });
    }
    if input.get(start) != Some(&b'(') {
        return Err(invalid("IMAP body disposition view").at(start));
    }
    let kind = parse_string(input, start + 1)?;
    let parameter_start = required_space(
        input,
        kind.end,
        "validated body disposition parameter separator",
    )?;
    let parameter_end = parse_body_parameter(input, parameter_start)?;
    if input.get(parameter_end) != Some(&b')') {
        return Err(invalid("IMAP body disposition view terminator").at(parameter_end));
    }
    Ok(ParsedDisposition {
        value: BodyDisposition::Value {
            kind: kind.value,
            parameters: body_parameters_view(&input[parameter_start..parameter_end])?,
        },
        end: parameter_end + 1,
    })
}

struct ParsedLanguage<'a> {
    value: BodyLanguage<'a>,
    end: usize,
}

fn parse_body_language_view(
    input: &[u8],
    start: usize,
) -> Result<ParsedLanguage<'_>, ProtocolError> {
    if input.get(start) == Some(&b'(') {
        let end = parse_body_language(input, start)?;
        return Ok(ParsedLanguage {
            value: BodyLanguage::List(&input[start + 1..end - 1]),
            end,
        });
    }
    let value = parse_nstring(input, start)?;
    Ok(ParsedLanguage {
        value: match value.value {
            FetchNString::Nil => BodyLanguage::Nil,
            FetchNString::String(value) => BodyLanguage::String(value),
        },
        end: value.end,
    })
}
