use std::borrow::Cow;
use std::ops::Range;

use bytes::Bytes;
use mail_protocol_core::{ErrorKind, ProtocolError};
use memchr::memchr;

/// Wire representation selected for an IMAP `astring`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum AStringKind {
    /// One or more `ASTRING-CHAR` octets.
    Atom,
    /// A double-quoted UTF-8 string.
    Quoted,
    /// A length-prefixed string.
    Literal {
        /// Whether the marker uses the `{n+}` non-synchronizing form.
        non_synchronizing: bool,
    },
}

/// A validated, zero-copy IMAP `astring`.
///
/// The value owns a cheap [`Bytes`] view of its wire representation. Literal
/// payloads and quoted contents are exposed as borrowed slices, so parsing a
/// large mailbox, user name, or password does not copy it.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct AString {
    wire: Bytes,
    content: Range<usize>,
    kind: AStringKind,
}

impl AString {
    /// Parses exactly one RFC 9051 `astring`.
    ///
    /// This validates the base grammar. Negotiated limits for `{n+}` are
    /// enforced by the command framing decoder because they depend on the
    /// active IMAP capabilities.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid atoms, malformed UTF-8 or escaping in a
    /// quoted string, malformed/truncated literal markers, NUL in a normal
    /// literal, or bytes following the value.
    pub fn parse(wire: &Bytes) -> Result<Self, ProtocolError> {
        let parsed = parse_astring_prefix(wire)?;
        if parsed.end != wire.len() {
            return Err(
                ProtocolError::new(ErrorKind::InvalidSyntax, "trailing IMAP astring data")
                    .at(parsed.end),
            );
        }
        Ok(Self {
            wire: wire.clone(),
            content: parsed.content,
            kind: parsed.kind,
        })
    }

    /// Returns the selected wire representation.
    pub const fn kind(&self) -> AStringKind {
        self.kind
    }

    /// Returns the complete encoded value, including quotes or literal marker.
    pub fn as_wire(&self) -> &[u8] {
        &self.wire
    }

    /// Returns the atom, escape-encoded quoted content, or literal payload.
    pub fn encoded_content(&self) -> &[u8] {
        &self.wire[self.content.clone()]
    }

    /// Returns the logical value, allocating only when quoted escapes must be
    /// removed.
    pub fn decoded(&self) -> Cow<'_, [u8]> {
        let content = self.encoded_content();
        if self.kind != AStringKind::Quoted || !content.contains(&b'\\') {
            return Cow::Borrowed(content);
        }

        let mut decoded = Vec::with_capacity(content.len());
        let mut cursor = 0;
        while cursor < content.len() {
            if content[cursor] == b'\\' {
                cursor += 1;
            }
            decoded.push(content[cursor]);
            cursor += 1;
        }
        Cow::Owned(decoded)
    }

    /// Returns the owned wire view.
    pub fn into_wire(self) -> Bytes {
        self.wire
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ParsedAString {
    pub(crate) end: usize,
    pub(crate) content: Range<usize>,
    pub(crate) kind: AStringKind,
}

pub(crate) fn parse_astring_prefix(input: &[u8]) -> Result<ParsedAString, ProtocolError> {
    match input.first() {
        None | Some(b' ') => Err(ProtocolError::new(
            ErrorKind::InvalidSyntax,
            "empty IMAP astring",
        )),
        Some(b'{') => parse_literal(input),
        Some(b'\"') => parse_quoted(input),
        Some(_) => parse_atom(input),
    }
}

pub(crate) fn validate_astring(input: &[u8]) -> Result<(), ProtocolError> {
    let parsed = parse_astring_prefix(input)?;
    if parsed.end == input.len() {
        Ok(())
    } else {
        Err(
            ProtocolError::new(ErrorKind::InvalidSyntax, "trailing IMAP astring data")
                .at(parsed.end),
        )
    }
}

fn parse_atom(input: &[u8]) -> Result<ParsedAString, ProtocolError> {
    let end = input
        .iter()
        .position(|byte| *byte == b' ')
        .unwrap_or(input.len());
    let atom = &input[..end];
    if atom.is_empty() {
        return Err(ProtocolError::new(
            ErrorKind::InvalidSyntax,
            "empty IMAP astring atom",
        ));
    }
    if let Some(offset) = atom.iter().position(|byte| {
        !byte.is_ascii()
            || byte.is_ascii_control()
            || matches!(byte, b'(' | b')' | b'{' | b'%' | b'*' | b'\"' | b'\\')
    }) {
        return Err(ProtocolError::new(ErrorKind::InvalidSyntax, "IMAP astring atom").at(offset));
    }
    Ok(ParsedAString {
        end,
        content: 0..end,
        kind: AStringKind::Atom,
    })
}

fn parse_quoted(input: &[u8]) -> Result<ParsedAString, ProtocolError> {
    let mut cursor = 1;
    while cursor < input.len() {
        match input[cursor] {
            b'\"' => {
                if std::str::from_utf8(&input[1..cursor]).is_err() {
                    return Err(ProtocolError::new(
                        ErrorKind::InvalidSyntax,
                        "UTF-8 IMAP quoted string",
                    ));
                }
                return Ok(ParsedAString {
                    end: cursor + 1,
                    content: 1..cursor,
                    kind: AStringKind::Quoted,
                });
            }
            b'\\' => {
                let Some(escaped) = input.get(cursor + 1) else {
                    break;
                };
                if !matches!(escaped, b'\"' | b'\\') {
                    return Err(
                        ProtocolError::new(ErrorKind::InvalidSyntax, "IMAP quoted escape")
                            .at(cursor),
                    );
                }
                cursor += 2;
            }
            b'\0' | b'\r' | b'\n' => {
                return Err(ProtocolError::new(
                    ErrorKind::InvalidSyntax,
                    "IMAP quoted string character",
                )
                .at(cursor));
            }
            _ => cursor += 1,
        }
    }
    Err(ProtocolError::new(
        ErrorKind::InvalidSyntax,
        "unterminated IMAP quoted string",
    ))
}

fn parse_literal(input: &[u8]) -> Result<ParsedAString, ProtocolError> {
    let mut cursor = 1;
    let digits_start = cursor;
    let mut length = 0u64;
    while let Some(digit) = input.get(cursor).filter(|byte| byte.is_ascii_digit()) {
        length = length
            .checked_mul(10)
            .and_then(|value| value.checked_add(u64::from(*digit - b'0')))
            .ok_or_else(|| {
                ProtocolError::new(ErrorKind::LiteralTooLarge, "IMAP astring literal length")
            })?;
        cursor += 1;
    }
    if cursor == digits_start {
        return Err(
            ProtocolError::new(ErrorKind::InvalidSyntax, "IMAP astring literal length").at(cursor),
        );
    }
    if length > i64::MAX as u64 {
        return Err(ProtocolError::new(
            ErrorKind::LiteralTooLarge,
            "IMAP astring literal length",
        ));
    }

    let non_synchronizing = input.get(cursor) == Some(&b'+');
    cursor += usize::from(non_synchronizing);
    if input.get(cursor) != Some(&b'}') {
        return Err(
            ProtocolError::new(ErrorKind::InvalidSyntax, "IMAP astring literal marker").at(cursor),
        );
    }
    cursor += 1;
    if input.get(cursor..cursor + 2) != Some(b"\r\n") {
        return Err(
            ProtocolError::new(ErrorKind::InvalidSyntax, "IMAP astring literal marker").at(cursor),
        );
    }
    let content_start = cursor + 2;
    let length = usize::try_from(length).map_err(|_| {
        ProtocolError::new(ErrorKind::LiteralTooLarge, "IMAP astring literal length")
    })?;
    let end = content_start.checked_add(length).ok_or_else(|| {
        ProtocolError::new(ErrorKind::LiteralTooLarge, "IMAP astring literal length")
    })?;
    if end > input.len() {
        return Err(
            ProtocolError::new(ErrorKind::InvalidSyntax, "truncated IMAP astring literal")
                .at(input.len()),
        );
    }
    if let Some(relative) = memchr(0, &input[content_start..end]) {
        return Err(
            ProtocolError::new(ErrorKind::InvalidSyntax, "NUL in IMAP astring literal")
                .at(content_start + relative),
        );
    }
    Ok(ParsedAString {
        end,
        content: content_start..end,
        kind: AStringKind::Literal { non_synchronizing },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_wire_forms_without_copying_content() {
        let atom = AString::parse(&Bytes::from_static(b"IN]BOX")).unwrap();
        assert_eq!(atom.kind(), AStringKind::Atom);
        assert_eq!(atom.encoded_content(), b"IN]BOX");
        assert_eq!(atom.decoded(), &b"IN]BOX"[..]);

        let quoted = AString::parse(&Bytes::from_static(b"\"box \\\"name\\\"\"")).unwrap();
        assert_eq!(quoted.kind(), AStringKind::Quoted);
        assert_eq!(quoted.encoded_content(), b"box \\\"name\\\"");
        assert_eq!(quoted.decoded(), &b"box \"name\""[..]);

        let literal = AString::parse(&Bytes::from_static(b"{5+}\r\na\r\nbc")).unwrap();
        assert_eq!(
            literal.kind(),
            AStringKind::Literal {
                non_synchronizing: true
            }
        );
        assert_eq!(literal.encoded_content(), b"a\r\nbc");
    }

    #[test]
    fn rejects_invalid_quoted_and_literal_values() {
        for wire in [
            b"\"bad\\escape\"".as_slice(),
            b"\"bad\0value\"",
            b"\"\xff\"",
            b"{1}\r\n\0",
            b"{2}\r\nx",
            b"{1}\r\nxy",
        ] {
            assert!(
                AString::parse(&Bytes::copy_from_slice(wire)).is_err(),
                "{wire:?}"
            );
        }
    }
}
