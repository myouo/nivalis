use core::fmt;

/// Stable category for a protocol error.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum ErrorKind {
    /// A caller-supplied protocol configuration violates a mandatory bound.
    InvalidConfiguration,
    /// The input is not valid protocol syntax.
    InvalidSyntax,
    /// A line exceeded the configured limit.
    LineTooLong,
    /// A complete frame exceeded the configured limit.
    FrameTooLarge,
    /// A declared literal exceeded the configured limit.
    LiteralTooLarge,
    /// A recursive protocol value exceeded the configured nesting limit.
    NestingTooDeep,
    /// A numeric reply or status code is invalid.
    InvalidCode,
    /// The codec was used in an invalid protocol state.
    InvalidState,
}

/// Allocation-free error returned by all core codecs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use]
pub struct ProtocolError {
    kind: ErrorKind,
    context: &'static str,
    offset: Option<usize>,
}

impl ProtocolError {
    /// Creates an error without a byte offset.
    pub const fn new(kind: ErrorKind, context: &'static str) -> Self {
        Self {
            kind,
            context,
            offset: None,
        }
    }

    /// Adds the byte offset at which the error was detected.
    pub const fn at(mut self, offset: usize) -> Self {
        self.offset = Some(offset);
        self
    }

    /// Returns the stable error category.
    pub const fn kind(self) -> ErrorKind {
        self.kind
    }

    /// Returns a non-sensitive static description of the parsing context.
    pub const fn context(self) -> &'static str {
        self.context
    }

    /// Returns the byte offset, when one is available.
    pub const fn offset(self) -> Option<usize> {
        self.offset
    }
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(offset) = self.offset {
            write!(
                formatter,
                "{:?} while parsing {} at byte {}",
                self.kind, self.context, offset
            )
        } else {
            write!(formatter, "{:?} while parsing {}", self.kind, self.context)
        }
    }
}

impl std::error::Error for ProtocolError {}
