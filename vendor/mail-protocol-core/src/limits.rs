/// Resource limits applied before a codec allocates or consumes input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use]
pub struct Limits {
    line_len: usize,
    literal_len: usize,
    frame_len: usize,
    reply_lines: usize,
    nesting_depth: usize,
}

impl Limits {
    /// Creates a set of limits.
    pub const fn new(
        max_line_len: usize,
        max_literal_len: usize,
        max_frame_len: usize,
        max_reply_lines: usize,
    ) -> Self {
        Self {
            line_len: max_line_len,
            literal_len: max_literal_len,
            frame_len: max_frame_len,
            reply_lines: max_reply_lines,
            nesting_depth: 64,
        }
    }

    /// Maximum bytes before a CRLF delimiter.
    pub const fn max_line_len(self) -> usize {
        self.line_len
    }

    /// Maximum declared IMAP literal or similar counted block.
    pub const fn max_literal_len(self) -> usize {
        self.literal_len
    }

    /// Maximum total bytes in a decoded item.
    pub const fn max_frame_len(self) -> usize {
        self.frame_len
    }

    /// Maximum lines accepted in a multi-line reply.
    pub const fn max_reply_lines(self) -> usize {
        self.reply_lines
    }

    /// Maximum nesting accepted in recursive protocol grammar.
    pub const fn max_nesting_depth(self) -> usize {
        self.nesting_depth
    }

    /// Returns a copy with a new maximum line length.
    pub const fn with_max_line_len(mut self, value: usize) -> Self {
        self.line_len = value;
        self
    }

    /// Returns a copy with a new maximum literal length.
    pub const fn with_max_literal_len(mut self, value: usize) -> Self {
        self.literal_len = value;
        self
    }

    /// Returns a copy with a new maximum frame length.
    pub const fn with_max_frame_len(mut self, value: usize) -> Self {
        self.frame_len = value;
        self
    }

    /// Returns a copy with a new maximum number of reply lines.
    pub const fn with_max_reply_lines(mut self, value: usize) -> Self {
        self.reply_lines = value;
        self
    }

    /// Returns a copy with a new maximum recursive grammar depth.
    pub const fn with_max_nesting_depth(mut self, value: usize) -> Self {
        self.nesting_depth = value;
        self
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self::new(64 * 1024, 64 * 1024 * 1024, 128 * 1024 * 1024, 128)
    }
}

#[cfg(test)]
mod tests {
    use super::Limits;

    #[test]
    fn recursive_grammar_limit_defaults_to_64_and_is_configurable() {
        assert_eq!(Limits::default().max_nesting_depth(), 64);
        assert_eq!(
            Limits::default()
                .with_max_nesting_depth(17)
                .max_nesting_depth(),
            17
        );
    }
}
