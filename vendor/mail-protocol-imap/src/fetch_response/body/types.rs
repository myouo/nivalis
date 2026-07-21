use mail_protocol_core::ProtocolError;

use super::super::{FetchEnvelope, FetchNString, FetchString, parse_string, required_space};
use super::validation::{parse_body, parse_body_extension};
use super::view::{analyze_body, parse_body_extensions_view};
use crate::fetch_response::DEFAULT_FETCH_RESPONSE_MAX_DEPTH;

/// Validated MIME body structure returned by BODY or BODYSTRUCTURE.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct BodyStructure<'a> {
    pub(super) wire: &'a [u8],
    pub(super) extensible: bool,
    pub(super) nesting_depth: usize,
}

impl<'a> BodyStructure<'a> {
    /// Parses one standalone extensible BODYSTRUCTURE value without copying.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed grammar, trailing input, numeric bounds,
    /// or excessive nesting.
    pub fn parse(wire: &'a [u8]) -> Result<Self, ProtocolError> {
        Self::parse_with_max_depth(wire, DEFAULT_FETCH_RESPONSE_MAX_DEPTH)
    }

    /// Parses a standalone BODYSTRUCTURE with an explicit nesting budget.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::parse`] and `NestingTooDeep` when
    /// the supplied budget is exceeded.
    pub fn parse_with_max_depth(wire: &'a [u8], max_depth: usize) -> Result<Self, ProtocolError> {
        let parsed = parse_body(wire, 0, max_depth, true)?;
        if parsed.end != wire.len() {
            return Err(super::super::invalid("trailing IMAP body structure data").at(parsed.end));
        }
        Ok(parsed.value)
    }

    pub const fn as_bytes(self) -> &'a [u8] {
        self.wire
    }

    pub const fn is_extensible(self) -> bool {
        self.extensible
    }

    pub const fn nesting_depth(self) -> usize {
        self.nesting_depth
    }

    /// Returns the semantic body form.
    pub fn kind(self) -> BodyStructureKind {
        self.view().kind()
    }

    /// Returns the media type for a non-multipart body.
    pub fn media_type(self) -> Option<FetchString<'a>> {
        self.view().media_type()
    }

    /// Returns the media subtype.
    pub fn subtype(self) -> FetchString<'a> {
        self.view().subtype()
    }

    /// Returns common one-part MIME fields.
    pub fn fields(self) -> Option<BodyFields<'a>> {
        self.view().fields()
    }

    /// Returns the encoded text-line count for TEXT and MESSAGE bodies.
    pub fn lines(self) -> Option<u64> {
        self.view().lines()
    }

    /// Returns the embedded envelope for MESSAGE/RFC822 or MESSAGE/GLOBAL.
    pub fn envelope(self) -> Option<FetchEnvelope<'a>> {
        self.view().envelope()
    }

    /// Returns the embedded body for MESSAGE/RFC822 or MESSAGE/GLOBAL.
    pub fn embedded_body(self) -> Option<BodyStructure<'a>> {
        self.view().embedded_body()
    }

    /// Iterates over direct children of a multipart body.
    pub fn parts(self) -> BodyPartIter<'a> {
        self.view().parts()
    }

    /// Returns exact extension fields after the required body fields.
    pub fn extension_data(self) -> Option<&'a [u8]> {
        self.view().extension_data()
    }

    /// Returns typed standard extension fields, if present.
    pub fn extensions(self) -> Option<BodyExtensions<'a>> {
        self.view().extensions()
    }

    /// Parses the top-level layout once for efficient multi-field access.
    ///
    /// # Panics
    ///
    /// Panics only if an internal crate invariant is violated after this value
    /// was successfully validated.
    pub fn view(self) -> BodyStructureView<'a> {
        analyze_body(self).expect("BodyStructure stores validated grammar")
    }
}

/// One analyzed top-level BODY/BODYSTRUCTURE view.
#[derive(Clone, Debug)]
pub struct BodyStructureView<'a> {
    pub(super) kind: BodyStructureKind,
    pub(super) media_type: Option<FetchString<'a>>,
    pub(super) subtype: FetchString<'a>,
    pub(super) fields: Option<BodyFields<'a>>,
    pub(super) lines: Option<u64>,
    pub(super) envelope: Option<FetchEnvelope<'a>>,
    pub(super) embedded_body: Option<BodyStructure<'a>>,
    pub(super) parts: &'a [u8],
    pub(super) extensions: Option<&'a [u8]>,
    pub(super) extensible: bool,
    pub(super) max_depth: usize,
}

impl<'a> BodyStructureView<'a> {
    pub const fn kind(&self) -> BodyStructureKind {
        self.kind
    }

    pub const fn media_type(&self) -> Option<FetchString<'a>> {
        self.media_type
    }

    pub const fn subtype(&self) -> FetchString<'a> {
        self.subtype
    }

    pub const fn fields(&self) -> Option<BodyFields<'a>> {
        self.fields
    }

    pub const fn lines(&self) -> Option<u64> {
        self.lines
    }

    pub const fn envelope(&self) -> Option<FetchEnvelope<'a>> {
        self.envelope
    }

    pub const fn embedded_body(&self) -> Option<BodyStructure<'a>> {
        self.embedded_body
    }

    pub const fn parts(&self) -> BodyPartIter<'a> {
        BodyPartIter {
            remaining: self.parts,
            extensible: self.extensible,
            max_depth: self.max_depth,
        }
    }

    pub const fn extension_data(&self) -> Option<&'a [u8]> {
        self.extensions
    }

    /// Returns typed standard extension fields, if present.
    ///
    /// # Panics
    ///
    /// Panics only if an internal crate invariant is violated after validation.
    pub fn extensions(&self) -> Option<BodyExtensions<'a>> {
        self.extensions.map(|wire| {
            parse_body_extensions_view(wire, self.kind)
                .expect("BodyStructureView stores validated extensions")
        })
    }
}

/// Standard BODYSTRUCTURE extension fields plus future extension values.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct BodyExtensions<'a> {
    pub(super) md5: Option<FetchNString<'a>>,
    pub(super) parameters: Option<BodyParameters<'a>>,
    pub(super) disposition: Option<BodyDisposition<'a>>,
    pub(super) language: Option<BodyLanguage<'a>>,
    pub(super) location: Option<FetchNString<'a>>,
    pub(super) future: &'a [u8],
}

impl<'a> BodyExtensions<'a> {
    pub const fn md5(self) -> Option<FetchNString<'a>> {
        self.md5
    }

    pub const fn parameters(self) -> Option<BodyParameters<'a>> {
        self.parameters
    }

    pub const fn disposition(self) -> Option<BodyDisposition<'a>> {
        self.disposition
    }

    pub const fn language(self) -> Option<BodyLanguage<'a>> {
        self.language
    }

    pub const fn location(self) -> Option<FetchNString<'a>> {
        self.location
    }

    pub const fn future(self) -> BodyExtensionIter<'a> {
        BodyExtensionIter {
            remaining: self.future,
        }
    }
}

/// NIL or a typed content disposition and its parameters.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum BodyDisposition<'a> {
    Nil,
    Value {
        kind: FetchString<'a>,
        parameters: BodyParameters<'a>,
    },
}

/// NIL, one language string, or a non-empty language string list.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum BodyLanguage<'a> {
    Nil,
    String(FetchString<'a>),
    List(&'a [u8]),
}

impl<'a> BodyLanguage<'a> {
    pub const fn iter(self) -> BodyLanguageIter<'a> {
        match self {
            Self::Nil => BodyLanguageIter { remaining: b"" },
            Self::String(value) => BodyLanguageIter {
                remaining: value.as_bytes(),
            },
            Self::List(value) => BodyLanguageIter { remaining: value },
        }
    }
}

#[derive(Clone, Debug)]
pub struct BodyLanguageIter<'a> {
    remaining: &'a [u8],
}

impl<'a> Iterator for BodyLanguageIter<'a> {
    type Item = FetchString<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining.is_empty() {
            return None;
        }
        let value =
            parse_string(self.remaining, 0).expect("BodyLanguage stores validated string values");
        self.remaining = if value.end == self.remaining.len() {
            b""
        } else {
            &self.remaining[value.end + 1..]
        };
        Some(value.value)
    }
}

#[derive(Clone, Debug)]
pub struct BodyExtensionIter<'a> {
    remaining: &'a [u8],
}

impl<'a> Iterator for BodyExtensionIter<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining.is_empty() {
            return None;
        }
        let parsed = parse_body_extension(self.remaining, 0, 0, usize::MAX)
            .expect("BodyExtensions stores validated future values");
        let value = &self.remaining[..parsed.end];
        self.remaining = if parsed.end == self.remaining.len() {
            b""
        } else {
            &self.remaining[parsed.end + 1..]
        };
        Some(value)
    }
}

/// Semantic form of a MIME body structure.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum BodyStructureKind {
    Basic,
    Text,
    Message,
    Multipart,
}

/// Common fields of a non-multipart MIME body.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct BodyFields<'a> {
    pub parameters: BodyParameters<'a>,
    pub id: FetchNString<'a>,
    pub description: FetchNString<'a>,
    pub encoding: FetchString<'a>,
    pub octets: u32,
}

/// NIL or a list of MIME parameter name/value string pairs.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum BodyParameters<'a> {
    Nil,
    List(&'a [u8]),
}

impl<'a> BodyParameters<'a> {
    pub const fn iter(self) -> BodyParameterIter<'a> {
        BodyParameterIter {
            remaining: match self {
                Self::Nil => b"",
                Self::List(value) => value,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct BodyParameter<'a> {
    pub name: FetchString<'a>,
    pub value: FetchString<'a>,
}

#[derive(Clone, Debug)]
pub struct BodyParameterIter<'a> {
    remaining: &'a [u8],
}

impl<'a> Iterator for BodyParameterIter<'a> {
    type Item = BodyParameter<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining.is_empty() {
            return None;
        }
        let name = parse_string(self.remaining, 0).expect("BodyParameters stores validated names");
        let value_start = required_space(
            self.remaining,
            name.end,
            "validated body parameter separator",
        )
        .expect("BodyParameters stores validated separators");
        let value = parse_string(self.remaining, value_start)
            .expect("BodyParameters stores validated values");
        self.remaining = if value.end == self.remaining.len() {
            b""
        } else {
            &self.remaining[value.end + 1..]
        };
        Some(BodyParameter {
            name: name.value,
            value: value.value,
        })
    }
}

#[derive(Clone, Debug)]
pub struct BodyPartIter<'a> {
    remaining: &'a [u8],
    extensible: bool,
    max_depth: usize,
}

impl<'a> Iterator for BodyPartIter<'a> {
    type Item = BodyStructure<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining.is_empty() {
            return None;
        }
        let body = parse_body(self.remaining, 0, self.max_depth, self.extensible)
            .expect("multipart iterator stores validated child bodies");
        self.remaining = &self.remaining[body.end..];
        Some(body.value)
    }
}
