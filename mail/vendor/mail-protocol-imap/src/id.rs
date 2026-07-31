use std::borrow::Cow;
use std::iter::FusedIterator;

use bytes::Bytes;
use mail_protocol_core::{ErrorKind, ProtocolError};

use crate::{
    Command, CommandBody,
    astring::{AStringKind, ParsedAString, parse_astring_prefix},
};

/// Maximum number of field/value pairs permitted by RFC 2971.
pub const MAX_ID_PAIRS: usize = 30;

/// Maximum decoded field-name length permitted by RFC 2971, in octets.
pub const MAX_ID_FIELD_OCTETS: usize = 30;

/// Maximum decoded field-value length permitted by RFC 2971, in octets.
pub const MAX_ID_VALUE_OCTETS: usize = 1024;

/// Policy for non-synchronizing literals in an RFC 2971 payload.
///
/// A server response cannot contain a non-synchronizing literal. A command can
/// use one when the relevant IMAP capability has been negotiated; this parser
/// deliberately leaves that session-level check to the caller.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum IdLiteralPolicy {
    /// Accept only quoted strings and synchronizing literals.
    #[default]
    RejectNonSynchronizing,
    /// Also accept `{n+}` non-synchronizing literals.
    AllowNonSynchronizing,
}

/// Top-level representation of an RFC 2971 parameter payload.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum IdParametersKind {
    /// The sender disclosed no identification information using `NIL`.
    Nil,
    /// A parenthesized list, which may contain zero field/value pairs.
    List,
}

/// Wire representation selected for an RFC 2971 string.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum IdStringKind {
    /// A double-quoted UTF-8 string.
    Quoted,
    /// A normal length-prefixed string.
    Literal {
        /// Whether the marker uses the `{n+}` non-synchronizing form.
        non_synchronizing: bool,
    },
}

/// A validated, zero-copy RFC 2971 `string`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct IdString<'a> {
    wire: &'a [u8],
    encoded: Span,
    content: Span,
    kind: IdStringKind,
}

impl<'a> IdString<'a> {
    /// Returns the exact wire value, including quotes or the literal marker.
    pub fn as_wire(self) -> &'a [u8] {
        self.encoded.slice(self.wire)
    }

    /// Returns the escape-encoded quoted content or the literal payload.
    pub fn encoded_content(self) -> &'a [u8] {
        self.content.slice(self.wire)
    }

    /// Returns the selected string representation.
    pub const fn kind(self) -> IdStringKind {
        self.kind
    }

    /// Returns the logical value, allocating only when quoted escapes occur.
    pub fn decoded(self) -> Cow<'a, [u8]> {
        let content = self.encoded_content();
        if self.kind != IdStringKind::Quoted || !content.contains(&b'\\') {
            return Cow::Borrowed(content);
        }

        Cow::Owned(DecodedBytes::new(content, true).collect())
    }

    /// Returns the logical value length in octets.
    pub fn decoded_len(self) -> usize {
        decoded_len(self.wire, StringLayout::from(self))
    }
}

/// A validated RFC 2971 `nstring` field value.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum IdValue<'a> {
    /// The `NIL` sentinel. The slice preserves the original token casing.
    Nil(&'a [u8]),
    /// A quoted or literal string.
    String(IdString<'a>),
}

impl<'a> IdValue<'a> {
    /// Returns the exact wire value.
    pub fn as_wire(self) -> &'a [u8] {
        match self {
            Self::Nil(wire) => wire,
            Self::String(value) => value.encoded.slice(value.wire),
        }
    }

    /// Returns whether this value is `NIL`.
    pub const fn is_nil(self) -> bool {
        matches!(self, Self::Nil(_))
    }

    /// Returns the logical string, or `None` for `NIL`.
    pub fn decoded(self) -> Option<Cow<'a, [u8]>> {
        match self {
            Self::Nil(_) => None,
            Self::String(value) => Some(value.decoded()),
        }
    }
}

/// One validated field/value pair in an RFC 2971 list.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct IdPair<'a> {
    wire: &'a [u8],
    layout: PairLayout,
}

impl<'a> IdPair<'a> {
    /// Returns the exact pair bytes, excluding surrounding list separators.
    pub fn as_wire(self) -> &'a [u8] {
        self.layout.span.slice(self.wire)
    }

    /// Returns the field-name string.
    pub fn field(self) -> IdString<'a> {
        IdString::from_layout(self.wire, self.layout.field)
    }

    /// Returns the field value, preserving `NIL` distinctly from an empty
    /// quoted or literal string.
    pub fn value(self) -> IdValue<'a> {
        IdValue::from_layout(self.wire, self.layout.value)
    }
}

/// Parsed RFC 2971 ID command or response parameters.
///
/// The input is exactly the payload following `ID SP`: either `NIL` or a
/// parenthesized sequence of `string SP nstring` pairs. The value owns one
/// cheap [`Bytes`] clone, while strings, values, pairs, and iteration borrow
/// validated ranges from that backing allocation.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct IdParameters {
    wire: Bytes,
    kind: IdParametersKind,
    pair_count: usize,
}

impl IdParameters {
    /// Parses a complete ID payload using response-safe literal rules.
    ///
    /// This is equivalent to [`Self::parse_response`]. It rejects `{n+}`
    /// literals, leading or trailing whitespace, trailing CRLF, atoms in place
    /// of strings, duplicate field names under ASCII case folding, and all RFC
    /// 2971 size/count violations.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed syntax, forbidden literal forms,
    /// duplicate fields, or exhausted RFC 2971 bounds.
    pub fn parse(wire: &Bytes) -> Result<Self, ProtocolError> {
        Self::parse_response(wire)
    }

    /// Parses a complete ID response payload.
    ///
    /// Non-synchronizing literals are invalid in server responses.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::parse`].
    pub fn parse_response(wire: &Bytes) -> Result<Self, ProtocolError> {
        Self::parse_with_policy(wire, IdLiteralPolicy::RejectNonSynchronizing)
    }

    /// Parses a complete ID command payload.
    ///
    /// This accepts non-synchronizing literals syntactically. The command
    /// framing/session layer remains responsible for enforcing negotiated
    /// `LITERAL+`/`IMAP4rev2` rules before calling or accepting this result.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed syntax, duplicate fields, or exhausted
    /// RFC 2971 bounds.
    pub fn parse_command(wire: &Bytes) -> Result<Self, ProtocolError> {
        Self::parse_with_policy(wire, IdLiteralPolicy::AllowNonSynchronizing)
    }

    /// Parses a complete ID payload with an explicit literal policy.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed syntax, a literal rejected by `policy`,
    /// duplicate field names under ASCII case folding, more than 30 pairs, a
    /// field over 30 decoded octets, or a value over 1024 decoded octets.
    pub fn parse_with_policy(wire: &Bytes, policy: IdLiteralPolicy) -> Result<Self, ProtocolError> {
        let (kind, pair_count) = validate_parameters(wire, policy)?;
        Ok(Self {
            wire: wire.clone(),
            kind,
            pair_count,
        })
    }

    /// Returns the exact validated payload bytes following `ID SP`.
    pub const fn as_bytes(&self) -> &Bytes {
        &self.wire
    }

    /// Returns whether the payload was `NIL` or a parenthesized list.
    pub const fn kind(&self) -> IdParametersKind {
        self.kind
    }

    /// Returns whether the payload is the `NIL` sentinel.
    pub const fn is_nil(&self) -> bool {
        matches!(self.kind, IdParametersKind::Nil)
    }

    /// Returns the number of field/value pairs.
    pub const fn len(&self) -> usize {
        self.pair_count
    }

    /// Returns whether the payload has no field/value pairs.
    ///
    /// Both `NIL` and `()` are empty; use [`Self::kind`] or [`Self::is_nil`] to
    /// distinguish them.
    pub const fn is_empty(&self) -> bool {
        self.pair_count == 0
    }

    /// Iterates over field/value pairs without allocating.
    pub fn iter(&self) -> IdPairIter<'_> {
        IdPairIter {
            wire: &self.wire,
            cursor: usize::from(matches!(self.kind, IdParametersKind::List)),
            remaining: self.pair_count,
        }
    }

    /// Looks up a logical field name using ASCII case-insensitive comparison.
    ///
    /// The outer `Option` distinguishes an absent field from an [`IdValue::Nil`]
    /// value. Non-ASCII octets are compared exactly.
    pub fn get(&self, field: &[u8]) -> Option<IdValue<'_>> {
        self.iter()
            .find(|pair| decoded_eq_slice(pair.field(), field))
            .map(IdPair::value)
    }

    /// Consumes the parsed value and returns its original backing bytes.
    pub fn into_bytes(self) -> Bytes {
        self.wire
    }
}

pub(crate) fn validate_id_command(input: &[u8]) -> Result<(), ProtocolError> {
    validate_parameters(input, IdLiteralPolicy::AllowNonSynchronizing).map(|_| ())
}

impl Command {
    /// Parses typed RFC 2971 parameters when this is an ID command.
    ///
    /// # Errors
    ///
    /// Returns an error if a manually constructed ID command contains invalid
    /// syntax, duplicate fields, or values outside the RFC 2971 bounds.
    pub fn parsed_id_parameters(&self) -> Result<Option<IdParameters>, ProtocolError> {
        match &self.body {
            CommandBody::Id { parameters } => IdParameters::parse_command(parameters).map(Some),
            _ => Ok(None),
        }
    }
}

impl<'a> IntoIterator for &'a IdParameters {
    type Item = IdPair<'a>;
    type IntoIter = IdPairIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Allocation-free iterator over validated RFC 2971 field/value pairs.
#[derive(Clone, Debug)]
pub struct IdPairIter<'a> {
    wire: &'a [u8],
    cursor: usize,
    remaining: usize,
}

impl<'a> Iterator for IdPairIter<'a> {
    type Item = IdPair<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let layout = match parse_pair(
            self.wire,
            self.cursor,
            IdLiteralPolicy::AllowNonSynchronizing,
        ) {
            Ok(layout) => layout,
            Err(error) => {
                debug_assert!(false, "validated IMAP ID pair became invalid: {error}");
                self.remaining = 0;
                return None;
            }
        };

        self.cursor = layout.span.end;
        self.remaining -= 1;
        if self.remaining != 0 {
            debug_assert_eq!(self.wire.get(self.cursor), Some(&b' '));
            self.cursor += 1;
        } else {
            debug_assert_eq!(self.wire.get(self.cursor), Some(&b')'));
        }
        Some(IdPair {
            wire: self.wire,
            layout,
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl ExactSizeIterator for IdPairIter<'_> {}
impl FusedIterator for IdPairIter<'_> {}

impl<'a> IdString<'a> {
    fn from_layout(wire: &'a [u8], layout: StringLayout) -> Self {
        Self {
            wire,
            encoded: layout.encoded,
            content: layout.content,
            kind: layout.kind,
        }
    }
}

impl<'a> IdValue<'a> {
    fn from_layout(wire: &'a [u8], layout: ValueLayout) -> Self {
        match layout {
            ValueLayout::Nil(span) => Self::Nil(span.slice(wire)),
            ValueLayout::String(value) => Self::String(IdString::from_layout(wire, value)),
        }
    }
}

impl From<IdString<'_>> for StringLayout {
    fn from(value: IdString<'_>) -> Self {
        Self {
            encoded: value.encoded,
            content: value.content,
            kind: value.kind,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct Span {
    start: usize,
    end: usize,
}

impl Span {
    const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    fn slice(self, input: &[u8]) -> &[u8] {
        &input[self.start..self.end]
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct StringLayout {
    encoded: Span,
    content: Span,
    kind: IdStringKind,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum ValueLayout {
    Nil(Span),
    String(StringLayout),
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct PairLayout {
    span: Span,
    field: StringLayout,
    value: ValueLayout,
}

fn validate_parameters(
    input: &[u8],
    policy: IdLiteralPolicy,
) -> Result<(IdParametersKind, usize), ProtocolError> {
    if input.eq_ignore_ascii_case(b"NIL") {
        return Ok((IdParametersKind::Nil, 0));
    }
    if input.first() != Some(&b'(') {
        return Err(invalid("IMAP ID parameter payload"));
    }

    let mut cursor = 1usize;
    let mut pair_count = 0usize;
    let mut fields = [None; MAX_ID_PAIRS];
    if input.get(cursor) == Some(&b')') {
        cursor += 1;
    } else {
        loop {
            if pair_count == MAX_ID_PAIRS {
                return Err(ProtocolError::new(
                    ErrorKind::FrameTooLarge,
                    "IMAP ID field/value pair count",
                )
                .at(cursor));
            }

            let pair = parse_pair(input, cursor, policy)?;
            enforce_length(
                input,
                pair.field,
                MAX_ID_FIELD_OCTETS,
                "IMAP ID field length",
            )?;
            if let ValueLayout::String(value) = pair.value {
                enforce_length(input, value, MAX_ID_VALUE_OCTETS, "IMAP ID value length")?;
            }
            if fields[..pair_count]
                .iter()
                .flatten()
                .any(|previous| decoded_eq_ascii(input, *previous, pair.field))
            {
                return Err(invalid("duplicate IMAP ID field name").at(pair.field.encoded.start));
            }

            fields[pair_count] = Some(pair.field);
            pair_count += 1;
            cursor = pair.span.end;
            match input.get(cursor) {
                Some(b')') => {
                    cursor += 1;
                    break;
                }
                Some(b' ')
                    if input
                        .get(cursor + 1)
                        .is_some_and(|byte| !matches!(byte, b' ' | b')')) =>
                {
                    cursor += 1;
                }
                None => return Err(invalid("unterminated IMAP ID parameter list").at(cursor)),
                _ => return Err(invalid("IMAP ID pair separator").at(cursor)),
            }
        }
    }
    if cursor != input.len() {
        return Err(invalid("trailing IMAP ID parameter data").at(cursor));
    }
    Ok((IdParametersKind::List, pair_count))
}

fn parse_pair(
    input: &[u8],
    start: usize,
    policy: IdLiteralPolicy,
) -> Result<PairLayout, ProtocolError> {
    let field = parse_string(input, start, policy)?;
    let value_start = required_space(input, field.encoded.end, "IMAP ID field/value separator")?;
    let (value, end) = parse_nstring(input, value_start, policy)?;
    Ok(PairLayout {
        span: Span::new(start, end),
        field,
        value,
    })
}

fn parse_string(
    input: &[u8],
    start: usize,
    policy: IdLiteralPolicy,
) -> Result<StringLayout, ProtocolError> {
    if !matches!(input.get(start), Some(b'"' | b'{')) {
        return Err(invalid("IMAP ID string").at(start));
    }
    let ParsedAString { end, content, kind } =
        parse_astring_prefix(&input[start..]).map_err(|error| shift_error(error, start))?;
    let kind = match kind {
        AStringKind::Quoted => IdStringKind::Quoted,
        AStringKind::Literal {
            non_synchronizing: false,
        } => IdStringKind::Literal {
            non_synchronizing: false,
        },
        AStringKind::Literal {
            non_synchronizing: true,
        } if policy == IdLiteralPolicy::AllowNonSynchronizing => IdStringKind::Literal {
            non_synchronizing: true,
        },
        AStringKind::Literal {
            non_synchronizing: true,
        } => return Err(invalid("non-synchronizing IMAP server literal").at(start)),
        AStringKind::Atom => return Err(invalid("IMAP ID string").at(start)),
    };
    Ok(StringLayout {
        encoded: Span::new(start, start + end),
        content: Span::new(start + content.start, start + content.end),
        kind,
    })
}

fn parse_nstring(
    input: &[u8],
    start: usize,
    policy: IdLiteralPolicy,
) -> Result<(ValueLayout, usize), ProtocolError> {
    if let Some(end) = nil_end(input, start) {
        return Ok((ValueLayout::Nil(Span::new(start, end)), end));
    }
    let value = parse_string(input, start, policy)?;
    Ok((ValueLayout::String(value), value.encoded.end))
}

fn nil_end(input: &[u8], start: usize) -> Option<usize> {
    let end = start.checked_add(3)?;
    if !input.get(start..end)?.eq_ignore_ascii_case(b"NIL") {
        return None;
    }
    match input.get(end) {
        None | Some(b' ' | b')') => Some(end),
        _ => None,
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

fn enforce_length(
    input: &[u8],
    value: StringLayout,
    maximum: usize,
    context: &'static str,
) -> Result<(), ProtocolError> {
    if decoded_len(input, value) > maximum {
        Err(ProtocolError::new(ErrorKind::FrameTooLarge, context).at(value.content.start))
    } else {
        Ok(())
    }
}

fn decoded_len(input: &[u8], value: StringLayout) -> usize {
    decoded_bytes(input, value).count()
}

fn decoded_eq_ascii(input: &[u8], left: StringLayout, right: StringLayout) -> bool {
    let mut left = decoded_bytes(input, left);
    let mut right = decoded_bytes(input, right);
    loop {
        match (left.next(), right.next()) {
            (Some(left), Some(right)) if left.eq_ignore_ascii_case(&right) => {}
            (None, None) => return true,
            _ => return false,
        }
    }
}

fn decoded_eq_slice(value: IdString<'_>, expected: &[u8]) -> bool {
    let mut value = DecodedBytes::new(value.encoded_content(), value.kind == IdStringKind::Quoted);
    let mut expected = expected.iter().copied();
    loop {
        match (value.next(), expected.next()) {
            (Some(value), Some(expected)) if value.eq_ignore_ascii_case(&expected) => {}
            (None, None) => return true,
            _ => return false,
        }
    }
}

fn decoded_bytes(input: &[u8], value: StringLayout) -> DecodedBytes<'_> {
    DecodedBytes::new(
        value.content.slice(input),
        value.kind == IdStringKind::Quoted,
    )
}

struct DecodedBytes<'a> {
    content: &'a [u8],
    quoted: bool,
    cursor: usize,
}

impl<'a> DecodedBytes<'a> {
    const fn new(content: &'a [u8], quoted: bool) -> Self {
        Self {
            content,
            quoted,
            cursor: 0,
        }
    }
}

impl Iterator for DecodedBytes<'_> {
    type Item = u8;

    fn next(&mut self) -> Option<Self::Item> {
        let mut byte = *self.content.get(self.cursor)?;
        self.cursor += 1;
        if self.quoted && byte == b'\\' {
            byte = *self.content.get(self.cursor)?;
            self.cursor += 1;
        }
        Some(byte)
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

    fn bytes(value: &'static [u8]) -> Bytes {
        Bytes::from_static(value)
    }

    #[test]
    fn parses_rfc_examples_and_provides_case_insensitive_access() {
        let wire = bytes(
            b"(\"name\" \"sodr\" \"version\" \"19.34\" \"vendor\" \"Pink Floyd Music Limited\")",
        );
        let parsed = IdParameters::parse_command(&wire).unwrap();

        assert_eq!(parsed.as_bytes().as_ptr(), wire.as_ptr());
        assert_eq!(parsed.kind(), IdParametersKind::List);
        assert_eq!(parsed.len(), 3);
        assert!(!parsed.is_empty());
        assert_eq!(
            parsed.get(b"NAME").unwrap().decoded().unwrap().as_ref(),
            b"sodr"
        );
        assert_eq!(
            parsed.get(b"VeRsIoN").unwrap().decoded().unwrap().as_ref(),
            b"19.34"
        );
        assert!(parsed.get(b"missing").is_none());

        let pairs = parsed.iter().collect::<Vec<_>>();
        assert_eq!(pairs[0].as_wire(), b"\"name\" \"sodr\"");
        assert_eq!(pairs[2].field().decoded().as_ref(), b"vendor");
        assert_eq!(pairs[2].value().as_wire(), b"\"Pink Floyd Music Limited\"");
    }

    #[test]
    fn keeps_nil_distinct_from_an_empty_list_and_nil_value() {
        let nil = IdParameters::parse(&bytes(b"nIl")).unwrap();
        let empty = IdParameters::parse(&bytes(b"()")).unwrap();
        assert_eq!(nil.kind(), IdParametersKind::Nil);
        assert!(nil.is_nil());
        assert!(nil.is_empty());
        assert_eq!(empty.kind(), IdParametersKind::List);
        assert!(!empty.is_nil());
        assert!(empty.is_empty());

        let fields = IdParameters::parse(&bytes(b"(\"known\" nil \"empty\" \"\")")).unwrap();
        assert!(matches!(fields.get(b"known"), Some(IdValue::Nil(b"nil"))));
        assert_eq!(
            fields.get(b"empty").unwrap().decoded().unwrap().as_ref(),
            b""
        );
        assert!(fields.get(b"unknown").is_none());
    }

    #[test]
    fn parses_quoted_and_synchronizing_literal_strings_at_exact_boundaries() {
        let wire = bytes(b"({4}\r\nna)m {4}\r\na\r\nb \"escaped\" \"a\\\\b\\\"c\" \"none\" NIL)");
        let parsed = IdParameters::parse_response(&wire).unwrap();
        let mut pairs = parsed.iter();
        assert_eq!(pairs.len(), 3);

        let first = pairs.next().unwrap();
        assert_eq!(
            first.field().kind(),
            IdStringKind::Literal {
                non_synchronizing: false
            }
        );
        assert_eq!(first.field().decoded().as_ref(), b"na)m");
        assert_eq!(first.value().decoded().unwrap().as_ref(), b"a\r\nb");

        let second = pairs.next().unwrap();
        let IdValue::String(second_value) = second.value() else {
            panic!("quoted value expected");
        };
        assert_eq!(second_value.decoded().as_ref(), b"a\\b\"c");
        assert_eq!(second_value.decoded_len(), 5);
        assert!(pairs.next().unwrap().value().is_nil());
        assert!(pairs.next().is_none());
    }

    #[test]
    fn command_policy_is_the_only_mode_allowing_non_synchronizing_literals() {
        let wire = bytes(b"({4+}\r\nname {5+}\r\nvalue)");
        assert!(IdParameters::parse(&wire).is_err());
        assert!(IdParameters::parse_response(&wire).is_err());
        assert!(
            IdParameters::parse_with_policy(&wire, IdLiteralPolicy::RejectNonSynchronizing)
                .is_err()
        );

        let command = IdParameters::parse_command(&wire).unwrap();
        let pair = command.iter().next().unwrap();
        assert_eq!(
            pair.field().kind(),
            IdStringKind::Literal {
                non_synchronizing: true
            }
        );
        assert_eq!(pair.value().decoded().unwrap().as_ref(), b"value");
    }

    #[test]
    fn enforces_decoded_octet_limits_not_wire_escape_lengths() {
        let mut exact = b"(\"".to_vec();
        exact.extend(std::iter::repeat_n(b'\\', MAX_ID_FIELD_OCTETS * 2));
        exact.extend_from_slice(b"\" \"");
        exact.extend(std::iter::repeat_n(b'v', MAX_ID_VALUE_OCTETS));
        exact.extend_from_slice(b"\")");
        let exact = IdParameters::parse(&Bytes::from(exact)).unwrap();
        let pair = exact.iter().next().unwrap();
        assert_eq!(pair.field().decoded_len(), MAX_ID_FIELD_OCTETS);
        assert_eq!(pair.value().decoded().unwrap().len(), MAX_ID_VALUE_OCTETS);

        let mut long_field = b"(\"".to_vec();
        long_field.extend(std::iter::repeat_n(b'f', MAX_ID_FIELD_OCTETS + 1));
        long_field.extend_from_slice(b"\" NIL)");
        let error = IdParameters::parse(&Bytes::from(long_field)).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::FrameTooLarge);
        assert_eq!(error.context(), "IMAP ID field length");

        let mut long_value = b"(\"field\" {1025}\r\n".to_vec();
        long_value.extend(std::iter::repeat_n(b'v', MAX_ID_VALUE_OCTETS + 1));
        long_value.push(b')');
        let error = IdParameters::parse(&Bytes::from(long_value)).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::FrameTooLarge);
        assert_eq!(error.context(), "IMAP ID value length");
    }

    #[test]
    fn enforces_pair_limit_and_keeps_iteration_exact_sized_and_fused() {
        let mut wire = vec![b'('];
        for index in 0..MAX_ID_PAIRS {
            if index != 0 {
                wire.push(b' ');
            }
            wire.extend_from_slice(format!("\"f{index}\" NIL").as_bytes());
        }
        wire.push(b')');
        let parsed = IdParameters::parse(&Bytes::from(wire.clone())).unwrap();
        let mut pairs = parsed.iter();
        assert_eq!(pairs.len(), MAX_ID_PAIRS);
        assert_eq!(pairs.by_ref().count(), MAX_ID_PAIRS);
        assert_eq!(pairs.len(), 0);
        assert!(pairs.next().is_none());
        assert!(pairs.next().is_none());

        wire.pop();
        wire.extend_from_slice(b" \"overflow\" NIL)");
        let error = IdParameters::parse(&Bytes::from(wire)).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::FrameTooLarge);
        assert_eq!(error.context(), "IMAP ID field/value pair count");
    }

    #[test]
    fn rejects_duplicate_logical_field_names_with_ascii_case_folding() {
        for wire in [
            b"(\"Name\" \"first\" {4}\r\nnAmE \"second\")".as_slice(),
            b"(\"a\\\\b\" NIL {3}\r\nA\\B NIL)",
            b"(\"\" NIL {0}\r\n NIL)",
        ] {
            let error = IdParameters::parse(&Bytes::copy_from_slice(wire)).unwrap_err();
            assert_eq!(error.context(), "duplicate IMAP ID field name", "{wire:?}");
        }
    }

    #[test]
    fn rejects_atoms_bad_spacing_trailing_data_and_stray_line_breaks() {
        for wire in [
            b"".as_slice(),
            b" NIL",
            b"NIL ",
            b"NIL\r\n",
            b"NILx",
            b"(",
            b"( )",
            b"() ",
            b"()\r\n",
            b"(\"field\")",
            b"(\"field\"  NIL)",
            b"(\"field\" NIL )",
            b"(\"field\" NIL  \"next\" NIL)",
            b"(field \"value\")",
            b"(\"field\" value)",
            b"(\"field\"\tNIL)",
            b"(\"field\" NIL\r\n)",
            b"(\"field\" NIL\n)",
            b"(\"unterminated NIL)",
            b"(\"field\" {2}\r\nx)",
            b"(\"field\" {1}\n\nx)",
            b"(\"field\" {1}\r\n\0)",
            b"(~{1}\r\nx NIL)",
        ] {
            assert!(
                IdParameters::parse(&Bytes::copy_from_slice(wire)).is_err(),
                "accepted {wire:?}"
            );
        }
    }

    #[test]
    fn rejects_invalid_utf8_quoted_strings_but_accepts_opaque_literal_octets() {
        assert!(IdParameters::parse(&Bytes::from_static(b"(\"\xff\" NIL)")).is_err());
        let literal = IdParameters::parse(&Bytes::from_static(b"({1}\r\n\xff NIL)")).unwrap();
        assert_eq!(
            literal.iter().next().unwrap().field().decoded().as_ref(),
            b"\xff"
        );
    }
}
