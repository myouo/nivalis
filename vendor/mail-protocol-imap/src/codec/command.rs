use super::{
    BytesMut, Command, DecodeStatus, Decoder, FrameMode, FrameProgress, FrameScanner, Limits,
    ProtocolError, parse_command, restore_frame,
};

/// Incremental framing decoder for client commands.
///
/// This variant accepts a complete command already present in the buffer and
/// does not enforce the synchronization exchange for plain `{n}` literals.
/// Servers receiving untrusted client input should use
/// [`ServerCommandDecoder`] instead.
#[derive(Clone, Debug, Default)]
pub struct CommandDecoder {
    limits: Limits,
    scanner: FrameScanner,
    literal_plus: bool,
}

impl CommandDecoder {
    /// Creates a decoder with explicit resource limits.
    pub const fn new(limits: Limits) -> Self {
        Self {
            limits,
            scanner: FrameScanner::new(),
            literal_plus: false,
        }
    }

    /// Enables RFC 7888 LITERAL+ semantics, allowing non-synchronizing
    /// command literals larger than the `IMAP4rev2` 4,096-octet baseline.
    #[must_use]
    pub const fn with_literal_plus(mut self, enabled: bool) -> Self {
        self.literal_plus = enabled;
        self
    }

    /// Discards incremental scan progress without consuming caller-owned input.
    ///
    /// Call this before reusing the decoder with an unrelated buffer after an
    /// [`DecodeStatus::Incomplete`] result. Appending bytes to the same logical
    /// buffer does not require a reset.
    pub const fn reset(&mut self) {
        self.scanner.reset();
    }
}

impl Decoder for CommandDecoder {
    type Item = Command;

    fn decode(&mut self, src: &mut BytesMut) -> Result<DecodeStatus<Command>, ProtocolError> {
        let Some(end) = self.scanner.frame_end(
            src,
            self.limits,
            FrameMode::Command {
                literal_plus: self.literal_plus,
                require_continuation: false,
            },
        )?
        else {
            return Ok(DecodeStatus::Incomplete);
        };
        let frame = src.split_to(end).freeze();
        match parse_command(&frame, self.limits) {
            Ok(command) => Ok(DecodeStatus::Complete(command)),
            Err(error) => {
                restore_frame(src, frame);
                Err(error)
            }
        }
    }
}

/// A synchronizing literal for which an IMAP server must send a continuation
/// request before accepting the declared octets.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct LiteralRequest {
    /// Number of literal octets the client will send after acknowledgement.
    pub length: usize,
}

/// Progress returned by [`ServerCommandDecoder`].
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
// Boxing `Command` would add a heap allocation to the successful decode path
// and break the established public variant shape.
#[allow(clippy::large_enum_variant)]
pub enum ServerCommandStatus {
    /// A complete command was removed from the input buffer.
    Complete(Command),
    /// More bytes are required; the input buffer was not consumed.
    Incomplete,
    /// The server must send `+` and then call
    /// [`ServerCommandDecoder::acknowledge_literal`] before accepting bytes.
    ContinuationRequired(LiteralRequest),
}

/// Strict incremental decoder for the server side of an IMAP command stream.
///
/// This decoder models the synchronization boundary that a plain `{n}` literal
/// introduces. It rejects literal bytes sent before acknowledgement, including
/// the command remainder following a zero-length synchronizing literal.
#[derive(Clone, Debug, Default)]
pub struct ServerCommandDecoder {
    limits: Limits,
    scanner: FrameScanner,
    literal_plus: bool,
}

impl ServerCommandDecoder {
    /// Creates a strict server command decoder with explicit resource limits.
    pub const fn new(limits: Limits) -> Self {
        Self {
            limits,
            scanner: FrameScanner::new(),
            literal_plus: false,
        }
    }

    /// Enables RFC 7888 LITERAL+ after that capability has been advertised.
    #[must_use]
    pub const fn with_literal_plus(mut self, enabled: bool) -> Self {
        self.literal_plus = enabled;
        self
    }

    /// Discards command and continuation progress without consuming input.
    pub const fn reset(&mut self) {
        self.scanner.reset();
    }

    /// Confirms that the server has accepted the currently requested literal.
    ///
    /// # Errors
    ///
    /// Returns [`mail_protocol_core::ErrorKind::InvalidState`] when no synchronizing literal is
    /// waiting for acknowledgement.
    pub fn acknowledge_literal(&mut self) -> Result<(), ProtocolError> {
        self.scanner.acknowledge_literal()
    }

    /// Attempts to receive one complete command or a synchronization event.
    ///
    /// # Errors
    ///
    /// Returns a categorized protocol error for invalid syntax, premature
    /// synchronizing-literal bytes, or configured resource-limit violations.
    pub fn decode(&mut self, src: &mut BytesMut) -> Result<ServerCommandStatus, ProtocolError> {
        let progress = self.scanner.scan(
            src,
            self.limits,
            FrameMode::Command {
                literal_plus: self.literal_plus,
                require_continuation: true,
            },
        )?;
        let FrameProgress::Complete(end) = progress else {
            return Ok(match progress {
                FrameProgress::Incomplete => ServerCommandStatus::Incomplete,
                FrameProgress::ContinuationRequired(request) => {
                    ServerCommandStatus::ContinuationRequired(request)
                }
                FrameProgress::Complete(_) => unreachable!(),
            });
        };
        let frame = src.split_to(end).freeze();
        match parse_command(&frame, self.limits) {
            Ok(command) => Ok(ServerCommandStatus::Complete(command)),
            Err(error) => {
                restore_frame(src, frame);
                Err(error)
            }
        }
    }
}
