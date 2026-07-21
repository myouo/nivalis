use std::borrow::Cow;
use std::iter::FusedIterator;

use bytes::Bytes;
use mail_protocol_core::wire::eq_ascii;
use mail_protocol_core::{ErrorKind, ProtocolError};

use crate::astring::{AStringKind, ParsedAString, parse_astring_prefix};

/// Default maximum structural depth accepted by the NAMESPACE parser.
///
/// RFC 9051 has three parenthesized levels here: a namespace group, a
/// descriptor, and an extension value list.
pub const DEFAULT_NAMESPACE_MAX_DEPTH: usize = 3;

/// Default combined limit for descriptors, response extensions, and extension
/// values in one NAMESPACE payload.
pub const DEFAULT_NAMESPACE_MAX_ITEMS: usize = 256;

/// Wire representation used by an RFC 9051 namespace string.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum NamespaceStringKind {
    /// A double-quoted UTF-8 string.
    Quoted,
    /// A synchronizing length-prefixed string.
    Literal,
}

/// A validated, zero-copy string within a NAMESPACE response.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct NamespaceString<'a> {
    wire: &'a [u8],
    encoded: Span,
    content: Span,
    kind: NamespaceStringKind,
}

impl<'a> NamespaceString<'a> {
    /// Returns the complete encoded string, including quotes or literal marker.
    pub fn as_wire(self) -> &'a [u8] {
        self.encoded.slice(self.wire)
    }

    /// Returns the selected wire representation.
    pub const fn kind(self) -> NamespaceStringKind {
        self.kind
    }

    /// Returns quoted content with escapes intact, or the literal payload.
    pub fn encoded_content(self) -> &'a [u8] {
        self.content.slice(self.wire)
    }

    /// Returns the logical value, allocating only when quoted escapes occur.
    pub fn decoded(self) -> Cow<'a, [u8]> {
        let content = self.encoded_content();
        if self.kind != NamespaceStringKind::Quoted || !content.contains(&b'\\') {
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
}

/// One validated hierarchy delimiter from a namespace descriptor.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct NamespaceDelimiter<'a> {
    wire: &'a [u8],
    encoded: Span,
    value: char,
}

impl<'a> NamespaceDelimiter<'a> {
    /// Returns the complete quoted delimiter.
    pub fn as_wire(self) -> &'a [u8] {
        self.encoded.slice(self.wire)
    }

    /// Returns the single logical delimiter character.
    pub const fn value(self) -> char {
        self.value
    }
}

/// Parsed RFC 2342 / RFC 9051 NAMESPACE response data.
///
/// The input is the payload after the `NAMESPACE` response name. The value owns
/// one cheap [`Bytes`] clone; all groups, descriptors, strings, and extensions
/// are borrowed views reconstructed from validated byte ranges.
/// Quoted strings use the RFC 9051 UTF-8 rules; an IMAP4rev1-only caller remains
/// responsible for interpreting legacy modified UTF-7 mailbox prefixes.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct NamespaceResponse {
    wire: Bytes,
    personal: GroupLayout,
    other_users: GroupLayout,
    shared: GroupLayout,
    item_count: usize,
    nesting_depth: usize,
}

impl NamespaceResponse {
    /// Parses exactly three namespace groups using the default depth and item
    /// limits.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed strings or delimiters, non-synchronizing
    /// server literals, invalid spacing, CRLF outside a declared literal,
    /// missing or extra groups, trailing data, or exhausted resource budgets.
    pub fn parse(wire: &Bytes) -> Result<Self, ProtocolError> {
        Self::parse_with_limits(
            wire,
            DEFAULT_NAMESPACE_MAX_DEPTH,
            DEFAULT_NAMESPACE_MAX_ITEMS,
        )
    }

    /// Parses exactly three namespace groups with explicit structural budgets.
    ///
    /// `max_items` is shared by descriptors, response extensions, and extension
    /// values. A valid response with extensions requires a depth of three; one
    /// with descriptors but no extensions requires a depth of two.
    ///
    /// # Errors
    ///
    /// Returns the same syntax errors as [`Self::parse`], `NestingTooDeep` when
    /// `max_depth` is exceeded, and `FrameTooLarge` when `max_items` is exceeded.
    pub fn parse_with_limits(
        wire: &Bytes,
        max_depth: usize,
        max_items: usize,
    ) -> Result<Self, ProtocolError> {
        let mut parser = Parser::new(wire, max_depth, max_items);
        let personal = parser.parse_group(0)?;
        let other_start = parser.required_space(personal.span.end)?;
        let other_users = parser.parse_group(other_start)?;
        let shared_start = parser.required_space(other_users.span.end)?;
        let shared = parser.parse_group(shared_start)?;
        if shared.span.end != wire.len() {
            return Err(invalid("trailing IMAP NAMESPACE response data").at(shared.span.end));
        }

        Ok(Self {
            wire: wire.clone(),
            personal,
            other_users,
            shared,
            item_count: parser.item_count,
            nesting_depth: parser.greatest_depth,
        })
    }

    /// Returns the exact bytes following the `NAMESPACE` response name.
    pub const fn as_bytes(&self) -> &Bytes {
        &self.wire
    }

    /// Returns the personal namespace group.
    pub fn personal(&self) -> NamespaceGroup<'_> {
        NamespaceGroup::new(&self.wire, self.personal)
    }

    /// Returns the other-users namespace group.
    pub fn other_users(&self) -> NamespaceGroup<'_> {
        NamespaceGroup::new(&self.wire, self.other_users)
    }

    /// Returns the shared namespace group.
    pub fn shared(&self) -> NamespaceGroup<'_> {
        NamespaceGroup::new(&self.wire, self.shared)
    }

    /// Returns the combined number of bounded descriptors, extensions, and
    /// extension values.
    pub const fn item_count(&self) -> usize {
        self.item_count
    }

    /// Returns the greatest parenthesized depth used by the response.
    pub const fn nesting_depth(&self) -> usize {
        self.nesting_depth
    }

    /// Consumes the parsed value and returns its original backing bytes.
    pub fn into_bytes(self) -> Bytes {
        self.wire
    }
}

/// A personal, other-users, or shared namespace group.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct NamespaceGroup<'a> {
    wire: &'a [u8],
    layout: GroupLayout,
}

impl<'a> NamespaceGroup<'a> {
    fn new(wire: &'a [u8], layout: GroupLayout) -> Self {
        Self { wire, layout }
    }

    /// Returns the exact `NIL` or parenthesized group bytes.
    pub fn as_wire(self) -> &'a [u8] {
        self.layout.span.slice(self.wire)
    }

    /// Returns whether this namespace class is unavailable (`NIL`).
    pub const fn is_nil(self) -> bool {
        self.layout.nil
    }

    /// Returns the number of descriptors in this group.
    pub const fn len(self) -> usize {
        self.layout.descriptor_count
    }

    /// Returns whether this group contains no descriptors.
    pub const fn is_empty(self) -> bool {
        self.layout.descriptor_count == 0
    }

    /// Iterates over namespace descriptors without allocating.
    pub fn iter(self) -> NamespaceDescriptorIter<'a> {
        let (cursor, end) = if self.layout.nil {
            (self.layout.span.end, self.layout.span.end)
        } else {
            (self.layout.span.start + 1, self.layout.span.end - 1)
        };
        NamespaceDescriptorIter {
            wire: self.wire,
            cursor,
            end,
            remaining: self.layout.descriptor_count,
        }
    }
}

impl<'a> IntoIterator for NamespaceGroup<'a> {
    type Item = NamespaceDescriptor<'a>;
    type IntoIter = NamespaceDescriptorIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Allocation-free iterator over namespace descriptors.
#[derive(Clone, Debug)]
pub struct NamespaceDescriptorIter<'a> {
    wire: &'a [u8],
    cursor: usize,
    end: usize,
    remaining: usize,
}

impl<'a> Iterator for NamespaceDescriptorIter<'a> {
    type Item = NamespaceDescriptor<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            debug_assert_eq!(self.cursor, self.end);
            return None;
        }
        let mut parser = Parser::unbounded(self.wire);
        let layout = match parser.parse_descriptor(self.cursor) {
            Ok(layout) => layout,
            Err(error) => {
                debug_assert!(
                    false,
                    "validated NAMESPACE descriptor became invalid: {error}"
                );
                self.remaining = 0;
                self.cursor = self.end;
                return None;
            }
        };
        self.cursor = layout.span.end;
        self.remaining -= 1;
        Some(NamespaceDescriptor {
            wire: self.wire,
            layout,
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl ExactSizeIterator for NamespaceDescriptorIter<'_> {}
impl FusedIterator for NamespaceDescriptorIter<'_> {}

/// One namespace descriptor: prefix, optional hierarchy delimiter, and response
/// extensions.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct NamespaceDescriptor<'a> {
    wire: &'a [u8],
    layout: DescriptorLayout,
}

impl<'a> NamespaceDescriptor<'a> {
    /// Returns the complete parenthesized descriptor bytes.
    pub fn as_wire(self) -> &'a [u8] {
        self.layout.span.slice(self.wire)
    }

    /// Returns the namespace prefix string.
    pub fn prefix(self) -> NamespaceString<'a> {
        NamespaceString::from_layout(self.wire, self.layout.prefix)
    }

    /// Returns the hierarchy delimiter, or `None` for `NIL`.
    pub fn delimiter(self) -> Option<NamespaceDelimiter<'a>> {
        match self.layout.delimiter {
            DelimiterLayout::Nil => None,
            DelimiterLayout::Quoted { encoded, value } => Some(NamespaceDelimiter {
                wire: self.wire,
                encoded,
                value,
            }),
        }
    }

    /// Returns the number of response extensions on this descriptor.
    pub const fn extension_count(self) -> usize {
        self.layout.extension_count
    }

    /// Iterates over response extensions without allocating.
    pub fn extensions(self) -> NamespaceExtensionIter<'a> {
        NamespaceExtensionIter {
            wire: self.wire,
            cursor: self.layout.extensions.start,
            end: self.layout.extensions.end,
            remaining: self.layout.extension_count,
        }
    }
}

/// Allocation-free iterator over namespace response extensions.
#[derive(Clone, Debug)]
pub struct NamespaceExtensionIter<'a> {
    wire: &'a [u8],
    cursor: usize,
    end: usize,
    remaining: usize,
}

impl<'a> Iterator for NamespaceExtensionIter<'a> {
    type Item = NamespaceExtension<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            debug_assert_eq!(self.cursor, self.end);
            return None;
        }
        debug_assert_eq!(self.wire.get(self.cursor), Some(&b' '));
        let mut parser = Parser::unbounded(self.wire);
        let layout = match parser.parse_extension(self.cursor + 1) {
            Ok(layout) => layout,
            Err(error) => {
                debug_assert!(
                    false,
                    "validated NAMESPACE extension became invalid: {error}"
                );
                self.remaining = 0;
                self.cursor = self.end;
                return None;
            }
        };
        self.cursor = layout.span.end;
        self.remaining -= 1;
        Some(NamespaceExtension {
            wire: self.wire,
            layout,
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl ExactSizeIterator for NamespaceExtensionIter<'_> {}
impl FusedIterator for NamespaceExtensionIter<'_> {}

/// One RFC 2342 namespace response extension.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct NamespaceExtension<'a> {
    wire: &'a [u8],
    layout: ExtensionLayout,
}

impl<'a> NamespaceExtension<'a> {
    /// Returns the complete extension bytes, excluding the leading descriptor
    /// separator.
    pub fn as_wire(self) -> &'a [u8] {
        self.layout.span.slice(self.wire)
    }

    /// Returns the extension name string.
    pub fn name(self) -> NamespaceString<'a> {
        NamespaceString::from_layout(self.wire, self.layout.name)
    }

    /// Returns the number of strings in the extension value list.
    pub const fn value_count(self) -> usize {
        self.layout.value_count
    }

    /// Iterates over extension value strings without allocating.
    pub fn values(self) -> NamespaceExtensionValueIter<'a> {
        NamespaceExtensionValueIter {
            wire: self.wire,
            cursor: self.layout.values.start,
            end: self.layout.values.end,
            remaining: self.layout.value_count,
        }
    }
}

/// Allocation-free iterator over strings in an extension value list.
#[derive(Clone, Debug)]
pub struct NamespaceExtensionValueIter<'a> {
    wire: &'a [u8],
    cursor: usize,
    end: usize,
    remaining: usize,
}

impl<'a> Iterator for NamespaceExtensionValueIter<'a> {
    type Item = NamespaceString<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            debug_assert_eq!(self.cursor, self.end);
            return None;
        }
        let parser = Parser::unbounded(self.wire);
        let parsed = match parser.parse_string(self.cursor) {
            Ok(parsed) => parsed,
            Err(error) => {
                debug_assert!(
                    false,
                    "validated NAMESPACE extension value became invalid: {error}"
                );
                self.remaining = 0;
                self.cursor = self.end;
                return None;
            }
        };
        self.cursor = parsed.encoded.end;
        self.remaining -= 1;
        if self.remaining != 0 {
            debug_assert_eq!(self.wire.get(self.cursor), Some(&b' '));
            self.cursor += 1;
        }
        Some(NamespaceString::from_layout(self.wire, parsed))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl ExactSizeIterator for NamespaceExtensionValueIter<'_> {}
impl FusedIterator for NamespaceExtensionValueIter<'_> {}

impl<'a> NamespaceString<'a> {
    fn from_layout(wire: &'a [u8], layout: StringLayout) -> Self {
        Self {
            wire,
            encoded: layout.encoded,
            content: layout.content,
            kind: layout.kind,
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
struct GroupLayout {
    span: Span,
    descriptor_count: usize,
    nil: bool,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct StringLayout {
    encoded: Span,
    content: Span,
    kind: NamespaceStringKind,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum DelimiterLayout {
    Nil,
    Quoted { encoded: Span, value: char },
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct DescriptorLayout {
    span: Span,
    prefix: StringLayout,
    delimiter: DelimiterLayout,
    extensions: Span,
    extension_count: usize,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct ExtensionLayout {
    span: Span,
    name: StringLayout,
    values: Span,
    value_count: usize,
}

struct Parser<'a> {
    input: &'a [u8],
    max_depth: usize,
    max_items: usize,
    item_count: usize,
    greatest_depth: usize,
}

impl<'a> Parser<'a> {
    fn new(input: &'a [u8], max_depth: usize, max_items: usize) -> Self {
        Self {
            input,
            max_depth,
            max_items,
            item_count: 0,
            greatest_depth: 0,
        }
    }

    fn unbounded(input: &'a [u8]) -> Self {
        Self::new(input, usize::MAX, usize::MAX)
    }

    fn parse_group(&mut self, start: usize) -> Result<GroupLayout, ProtocolError> {
        if let Some(end) = self.nil_end(start) {
            return Ok(GroupLayout {
                span: Span::new(start, end),
                descriptor_count: 0,
                nil: true,
            });
        }
        if self.input.get(start) != Some(&b'(') {
            return Err(invalid("IMAP NAMESPACE group").at(start));
        }
        self.enter_depth(1, start)?;
        let mut cursor = start + 1;
        if self.input.get(cursor) != Some(&b'(') {
            return Err(invalid("empty IMAP NAMESPACE group").at(cursor));
        }

        let mut descriptor_count = 0usize;
        loop {
            let descriptor = self.parse_descriptor(cursor)?;
            descriptor_count = descriptor_count.checked_add(1).ok_or_else(|| {
                ProtocolError::new(ErrorKind::FrameTooLarge, "IMAP NAMESPACE descriptor count")
                    .at(cursor)
            })?;
            cursor = descriptor.span.end;
            match self.input.get(cursor) {
                Some(b'(') => {}
                Some(b')') => {
                    cursor += 1;
                    break;
                }
                _ => return Err(invalid("IMAP NAMESPACE group terminator").at(cursor)),
            }
        }

        Ok(GroupLayout {
            span: Span::new(start, cursor),
            descriptor_count,
            nil: false,
        })
    }

    fn parse_descriptor(&mut self, start: usize) -> Result<DescriptorLayout, ProtocolError> {
        if self.input.get(start) != Some(&b'(') {
            return Err(invalid("IMAP NAMESPACE descriptor").at(start));
        }
        self.enter_depth(2, start)?;
        self.bump_item(start)?;

        let prefix = self.parse_string(start + 1)?;
        let delimiter_start = self.required_space(prefix.encoded.end)?;
        let (delimiter, mut cursor) = if let Some(end) = self.nil_end(delimiter_start) {
            (DelimiterLayout::Nil, end)
        } else {
            let parsed = self.parse_string(delimiter_start)?;
            if parsed.kind != NamespaceStringKind::Quoted {
                return Err(invalid("IMAP NAMESPACE hierarchy delimiter").at(delimiter_start));
            }
            let value = self.parse_delimiter_value(parsed)?;
            (
                DelimiterLayout::Quoted {
                    encoded: parsed.encoded,
                    value,
                },
                parsed.encoded.end,
            )
        };

        let extensions_start = cursor;
        let mut extension_count = 0usize;
        while self.input.get(cursor) == Some(&b' ') {
            let next_extension = cursor + 1;
            if self.input.get(next_extension).is_none()
                || self.input.get(next_extension) == Some(&b' ')
            {
                return Err(invalid("IMAP NAMESPACE extension separator").at(cursor));
            }
            let extension = self.parse_extension(next_extension)?;
            extension_count = extension_count.checked_add(1).ok_or_else(|| {
                ProtocolError::new(ErrorKind::FrameTooLarge, "IMAP NAMESPACE extension count")
                    .at(next_extension)
            })?;
            cursor = extension.span.end;
        }
        if self.input.get(cursor) != Some(&b')') {
            return Err(invalid("IMAP NAMESPACE descriptor terminator").at(cursor));
        }

        Ok(DescriptorLayout {
            span: Span::new(start, cursor + 1),
            prefix,
            delimiter,
            extensions: Span::new(extensions_start, cursor),
            extension_count,
        })
    }

    fn parse_extension(&mut self, start: usize) -> Result<ExtensionLayout, ProtocolError> {
        self.bump_item(start)?;
        let name = self.parse_string(start)?;
        let list_start = self.required_space(name.encoded.end)?;
        if self.input.get(list_start) != Some(&b'(') {
            return Err(invalid("IMAP NAMESPACE extension value list").at(list_start));
        }
        self.enter_depth(3, list_start)?;
        let values_start = list_start + 1;
        if self.input.get(values_start) == Some(&b')') {
            return Err(invalid("empty IMAP NAMESPACE extension value list").at(values_start));
        }

        let mut cursor = values_start;
        let mut value_count = 0usize;
        loop {
            self.bump_item(cursor)?;
            let value = self.parse_string(cursor)?;
            value_count = value_count.checked_add(1).ok_or_else(|| {
                ProtocolError::new(
                    ErrorKind::FrameTooLarge,
                    "IMAP NAMESPACE extension value count",
                )
                .at(cursor)
            })?;
            cursor = value.encoded.end;
            match self.input.get(cursor) {
                Some(b' ') => {
                    cursor += 1;
                    if self.input.get(cursor).is_none() || self.input.get(cursor) == Some(&b' ') {
                        return Err(
                            invalid("IMAP NAMESPACE extension value separator").at(cursor - 1)
                        );
                    }
                }
                Some(b')') => break,
                _ => {
                    return Err(
                        invalid("IMAP NAMESPACE extension value list terminator").at(cursor)
                    );
                }
            }
        }

        Ok(ExtensionLayout {
            span: Span::new(start, cursor + 1),
            name,
            values: Span::new(values_start, cursor),
            value_count,
        })
    }

    fn parse_string(&self, start: usize) -> Result<StringLayout, ProtocolError> {
        if !matches!(self.input.get(start), Some(b'\"' | b'{')) {
            return Err(invalid("IMAP NAMESPACE string").at(start));
        }
        let ParsedAString { end, content, kind } = parse_astring_prefix(&self.input[start..])
            .map_err(|error| shift_error(error, start))?;
        let kind = match kind {
            AStringKind::Quoted => NamespaceStringKind::Quoted,
            AStringKind::Literal {
                non_synchronizing: false,
            } => NamespaceStringKind::Literal,
            AStringKind::Literal {
                non_synchronizing: true,
            } => {
                return Err(invalid("non-synchronizing IMAP server literal").at(start));
            }
            AStringKind::Atom => {
                return Err(invalid("IMAP NAMESPACE string").at(start));
            }
        };
        Ok(StringLayout {
            encoded: Span::new(start, start + end),
            content: Span::new(start + content.start, start + content.end),
            kind,
        })
    }

    fn parse_delimiter_value(&self, parsed: StringLayout) -> Result<char, ProtocolError> {
        let content = parsed.content.slice(self.input);
        if content.first() == Some(&b'\\') {
            if content.len() == 2 && matches!(content[1], b'\"' | b'\\') {
                return Ok(char::from(content[1]));
            }
            return Err(invalid("IMAP NAMESPACE hierarchy delimiter").at(parsed.content.start));
        }
        let value = core::str::from_utf8(content).map_err(|_| {
            invalid("invalid UTF-8 IMAP NAMESPACE hierarchy delimiter").at(parsed.content.start)
        })?;
        let mut characters = value.chars();
        let delimiter = characters.next().ok_or_else(|| {
            invalid("empty IMAP NAMESPACE hierarchy delimiter").at(parsed.content.start)
        })?;
        if characters.next().is_some() {
            return Err(
                invalid("multi-character IMAP NAMESPACE hierarchy delimiter")
                    .at(parsed.content.start),
            );
        }
        Ok(delimiter)
    }

    fn nil_end(&self, start: usize) -> Option<usize> {
        let end = start.checked_add(3)?;
        let token = self.input.get(start..end)?;
        if !eq_ascii(token, b"NIL") {
            return None;
        }
        match self.input.get(end) {
            None | Some(b' ' | b')') => Some(end),
            _ => None,
        }
    }

    fn required_space(&self, cursor: usize) -> Result<usize, ProtocolError> {
        if self.input.get(cursor) != Some(&b' ')
            || self.input.get(cursor + 1).is_none()
            || self.input.get(cursor + 1) == Some(&b' ')
        {
            return Err(invalid("IMAP NAMESPACE separator").at(cursor));
        }
        Ok(cursor + 1)
    }

    fn enter_depth(&mut self, depth: usize, offset: usize) -> Result<(), ProtocolError> {
        if depth > self.max_depth {
            return Err(
                ProtocolError::new(ErrorKind::NestingTooDeep, "IMAP NAMESPACE nesting").at(offset),
            );
        }
        self.greatest_depth = self.greatest_depth.max(depth);
        Ok(())
    }

    fn bump_item(&mut self, offset: usize) -> Result<(), ProtocolError> {
        let next = self.item_count.checked_add(1).ok_or_else(|| {
            ProtocolError::new(ErrorKind::FrameTooLarge, "IMAP NAMESPACE item count").at(offset)
        })?;
        if next > self.max_items {
            return Err(
                ProtocolError::new(ErrorKind::FrameTooLarge, "IMAP NAMESPACE item count")
                    .at(offset),
            );
        }
        self.item_count = next;
        Ok(())
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
    fn parses_rfc_example_and_exposes_exact_groups() {
        let wire = bytes(b"((\"\" \"/\")) ((\"~\" \"/\")) NIL");
        let parsed = NamespaceResponse::parse(&wire).unwrap();

        assert_eq!(parsed.as_bytes(), &wire);
        assert_eq!(parsed.personal().as_wire(), b"((\"\" \"/\"))");
        assert_eq!(parsed.personal().len(), 1);
        assert_eq!(parsed.other_users().len(), 1);
        assert!(parsed.shared().is_nil());
        assert!(parsed.shared().is_empty());
        assert_eq!(parsed.item_count(), 2);
        assert_eq!(parsed.nesting_depth(), 2);

        let personal = parsed.personal().iter().next().unwrap();
        assert_eq!(personal.prefix().decoded().as_ref(), b"");
        assert_eq!(personal.delimiter().unwrap().value(), '/');
        assert_eq!(personal.extension_count(), 0);

        let other = parsed.other_users().iter().next().unwrap();
        assert_eq!(other.prefix().decoded().as_ref(), b"~");
        assert_eq!(other.delimiter().unwrap().as_wire(), b"\"/\"");
    }

    #[test]
    fn supports_multiple_descriptors_and_nil_groups() {
        let wire = bytes(b"((\"INBOX.\" \".\")(\"Archive\" NIL)) nil ((\"Shared\" \"/\"))");
        let parsed = NamespaceResponse::parse(&wire).unwrap();
        let mut personal = parsed.personal().iter();
        assert_eq!(personal.len(), 2);
        assert_eq!(personal.next().unwrap().delimiter().unwrap().value(), '.');
        assert!(personal.next().unwrap().delimiter().is_none());
        assert!(personal.next().is_none());
        assert!(parsed.other_users().is_nil());
        assert_eq!(
            parsed
                .shared()
                .iter()
                .next()
                .unwrap()
                .delimiter()
                .unwrap()
                .value(),
            '/'
        );
    }

    #[test]
    fn parses_extensions_and_synchronizing_literals_without_copying() {
        let wire =
            bytes(b"((\"\" \"/\" \"X-PARAM\" (\"one\" {3}\r\ntwo) {1}\r\nY (\"v\"))) NIL NIL");
        let parsed = NamespaceResponse::parse(&wire).unwrap();
        assert_eq!(parsed.item_count(), 6);
        assert_eq!(parsed.nesting_depth(), 3);
        let descriptor = parsed.personal().iter().next().unwrap();
        let mut extensions = descriptor.extensions();
        assert_eq!(extensions.len(), 2);

        let first = extensions.next().unwrap();
        assert_eq!(first.name().decoded().as_ref(), b"X-PARAM");
        assert_eq!(first.value_count(), 2);
        let values = first
            .values()
            .map(|value| value.decoded().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(values, [b"one".to_vec(), b"two".to_vec()]);
        assert_eq!(
            first.values().nth(1).unwrap().kind(),
            NamespaceStringKind::Literal
        );

        let second = extensions.next().unwrap();
        assert_eq!(second.name().decoded().as_ref(), b"Y");
        assert_eq!(second.values().next().unwrap().decoded().as_ref(), b"v");
        assert!(extensions.next().is_none());

        let cloned = parsed.clone();
        assert_eq!(parsed.as_bytes().as_ptr(), cloned.as_bytes().as_ptr());
        assert_eq!(wire.as_ptr(), parsed.as_bytes().as_ptr());
    }

    #[test]
    fn allows_crlf_only_inside_declared_synchronizing_literal_data() {
        let wire = bytes(b"(({4}\r\nA\r\nB \"/\")) NIL NIL");
        let parsed = NamespaceResponse::parse(&wire).unwrap();
        let prefix = parsed.personal().iter().next().unwrap().prefix();
        assert_eq!(prefix.kind(), NamespaceStringKind::Literal);
        assert_eq!(prefix.decoded().as_ref(), b"A\r\nB");

        for invalid_wire in [
            b"((\"A\r\nB\" \"/\")) NIL NIL".as_slice(),
            b"((\"A\" \"/\"))\r\n NIL NIL",
            b"((\"A\" \"/\")) NIL\r\n NIL",
        ] {
            assert!(NamespaceResponse::parse(&Bytes::copy_from_slice(invalid_wire)).is_err());
        }
    }

    #[test]
    fn rejects_non_synchronizing_server_literals_everywhere() {
        for wire in [
            b"(({1+}\r\nx \"/\")) NIL NIL".as_slice(),
            b"((\"x\" \"/\" {1+}\r\nX (\"v\"))) NIL NIL",
            b"((\"x\" \"/\" \"X\" ({1+}\r\nv))) NIL NIL",
        ] {
            let error = NamespaceResponse::parse(&Bytes::copy_from_slice(wire)).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidSyntax);
            assert_eq!(error.context(), "non-synchronizing IMAP server literal");
        }
    }

    #[test]
    fn delimiter_is_exactly_one_quoted_character_or_nil() {
        let escaped_quote = NamespaceResponse::parse(&bytes(b"((\"\" \"\\\"\")) NIL NIL")).unwrap();
        assert_eq!(
            escaped_quote
                .personal()
                .iter()
                .next()
                .unwrap()
                .delimiter()
                .unwrap()
                .value(),
            '"'
        );
        let escaped_backslash =
            NamespaceResponse::parse(&bytes(b"((\"\" \"\\\\\")) NIL NIL")).unwrap();
        assert_eq!(
            escaped_backslash
                .personal()
                .iter()
                .next()
                .unwrap()
                .delimiter()
                .unwrap()
                .value(),
            '\\'
        );

        let unicode =
            NamespaceResponse::parse(&bytes("((\"x\" \"☃\")) NIL NIL".as_bytes())).unwrap();
        assert_eq!(
            unicode
                .personal()
                .iter()
                .next()
                .unwrap()
                .delimiter()
                .unwrap()
                .value(),
            '☃'
        );

        for wire in [
            b"((\"x\" \"\")) NIL NIL".as_slice(),
            b"((\"x\" \"ab\")) NIL NIL",
            b"((\"x\" {1}\r\n/)) NIL NIL",
            b"((\"x\" atom)) NIL NIL",
        ] {
            assert!(NamespaceResponse::parse(&Bytes::copy_from_slice(wire)).is_err());
        }
    }

    #[test]
    fn enforces_exact_three_groups_spacing_and_end_of_input() {
        for wire in [
            b"NIL NIL".as_slice(),
            b"NIL NIL NIL NIL",
            b"NIL  NIL NIL",
            b"NIL\tNIL NIL",
            b"NAMESPACE NIL NIL NIL",
            b"NIL NIL NIL trailing",
            b"() NIL NIL",
            b"( ) NIL NIL",
            b"((\"x\" \"/\")) NIL",
            b"((\"x\" \"/\"))NIL NIL",
        ] {
            assert!(
                NamespaceResponse::parse(&Bytes::copy_from_slice(wire)).is_err(),
                "accepted {wire:?}"
            );
        }
        assert!(NamespaceResponse::parse(&bytes(b"nil NIL NiL")).is_ok());
    }

    #[test]
    fn rejects_malformed_descriptors_and_extensions() {
        for wire in [
            b"((atom \"/\")) NIL NIL".as_slice(),
            b"((\"x\")) NIL NIL",
            b"((\"x\"  \"/\")) NIL NIL",
            b"((\"x\" \"/\" )) NIL NIL",
            b"((\"x\" \"/\" \"X\" ())) NIL NIL",
            b"((\"x\" \"/\" \"X\" (atom))) NIL NIL",
            b"((\"x\" \"/\" \"X\" (\"a\"  \"b\"))) NIL NIL",
            b"((\"x\" \"/\" \"X\" (\"a\") garbage)) NIL NIL",
            b"((\"x\" \"/\") (\"y\" \"/\")) NIL NIL",
        ] {
            assert!(
                NamespaceResponse::parse(&Bytes::copy_from_slice(wire)).is_err(),
                "accepted {wire:?}"
            );
        }
    }

    #[test]
    fn depth_and_combined_item_limits_are_explicit() {
        let no_extensions = bytes(b"((\"x\" \"/\")) NIL NIL");
        assert!(NamespaceResponse::parse_with_limits(&no_extensions, 2, 1).is_ok());
        assert_eq!(
            NamespaceResponse::parse_with_limits(&no_extensions, 1, 1)
                .unwrap_err()
                .kind(),
            ErrorKind::NestingTooDeep
        );

        let extensions = bytes(b"((\"x\" \"/\" \"X\" (\"a\" \"b\"))) NIL NIL");
        let parsed = NamespaceResponse::parse_with_limits(&extensions, 3, 4).unwrap();
        assert_eq!(parsed.item_count(), 4);
        assert_eq!(
            NamespaceResponse::parse_with_limits(&extensions, 2, 4)
                .unwrap_err()
                .kind(),
            ErrorKind::NestingTooDeep
        );
        assert_eq!(
            NamespaceResponse::parse_with_limits(&extensions, 3, 3)
                .unwrap_err()
                .kind(),
            ErrorKind::FrameTooLarge
        );
        assert!(NamespaceResponse::parse_with_limits(&bytes(b"NIL NIL NIL"), 0, 0).is_ok());
    }

    #[test]
    fn default_item_limit_rejects_large_flat_groups() {
        let mut wire = Vec::new();
        wire.push(b'(');
        for _ in 0..=DEFAULT_NAMESPACE_MAX_ITEMS {
            wire.extend_from_slice(b"(\"x\" \"/\")");
        }
        wire.extend_from_slice(b") NIL NIL");
        let error = NamespaceResponse::parse(&Bytes::from(wire)).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::FrameTooLarge);
        assert_eq!(error.context(), "IMAP NAMESPACE item count");
    }

    #[test]
    fn iterators_are_exact_sized_fused_and_reusable() {
        let parsed = NamespaceResponse::parse(&bytes(
            b"((\"a\" \"/\")(\"b\" NIL)) NIL ((\"c\" \".\" \"X\" (\"1\" \"2\")))",
        ))
        .unwrap();
        let mut descriptors = parsed.personal().iter();
        assert_eq!(descriptors.size_hint(), (2, Some(2)));
        assert_eq!(
            descriptors.next().unwrap().prefix().decoded().as_ref(),
            b"a"
        );
        assert_eq!(descriptors.len(), 1);
        assert!(descriptors.next().is_some());
        assert!(descriptors.next().is_none());
        assert!(descriptors.next().is_none());

        let extension = parsed
            .shared()
            .iter()
            .next()
            .unwrap()
            .extensions()
            .next()
            .unwrap();
        let mut values = extension.values();
        assert_eq!(values.len(), 2);
        assert_eq!(values.next().unwrap().decoded().as_ref(), b"1");
        assert_eq!(values.next().unwrap().decoded().as_ref(), b"2");
        assert!(values.next().is_none());
        assert!(values.next().is_none());
    }

    #[test]
    fn literal_lengths_and_trailing_garbage_are_strict() {
        for wire in [
            b"(({2}\r\nx \"/\")) NIL NIL".as_slice(),
            b"(({4}\r\nabc \"/\")) NIL NIL",
            b"(({1}\nx \"/\")) NIL NIL",
            b"(({1}\r\nx \"/\")) NIL NIL\r\n",
        ] {
            assert!(NamespaceResponse::parse(&Bytes::copy_from_slice(wire)).is_err());
        }
    }
}
