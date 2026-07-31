use super::{
    Bytes, BytesMut, Command, CommandEncoder, Encoder, ErrorKind, FrameMode, LiteralKind,
    LiteralRequest, ProtocolError, find_crlf, literal_spec, validate_literal_mode,
};

/// One action in a client-side IMAP command transmission.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum CommandSendStep {
    /// Bytes that may now be written by the caller.
    Bytes(Bytes),
    /// The caller must wait for a server `+` response, then acknowledge it.
    ContinuationRequired(LiteralRequest),
    /// Every byte of the command has been released for writing.
    Complete,
}

#[derive(Clone, Debug)]
enum CommandSendPart {
    Bytes(core::ops::Range<usize>),
    Continuation(LiteralRequest),
}

/// Pure protocol state for safely transmitting a client command with literals.
///
/// The command is encoded once. Returned [`Bytes`] values are zero-copy slices
/// of that allocation. Synchronizing literal data is not released until the
/// corresponding continuation has been acknowledged.
#[derive(Clone, Debug)]
pub struct ClientCommandTransmission {
    frame: Bytes,
    parts: Vec<CommandSendPart>,
    next_part: usize,
    waiting: bool,
}

impl ClientCommandTransmission {
    /// Encodes and plans one command transmission.
    ///
    /// Set `literal_plus` only after RFC 7888 LITERAL+ has been advertised.
    ///
    /// # Errors
    ///
    /// Returns an error for an unencodable command, client-side literal8, or a
    /// non-synchronizing literal that exceeds the negotiated baseline.
    pub fn new(command: &Command, literal_plus: bool) -> Result<Self, ProtocolError> {
        let mut encoded = BytesMut::new();
        CommandEncoder.encode(command, &mut encoded)?;
        let frame = encoded.freeze();
        let parts = command_send_parts(&frame, literal_plus)?;
        Ok(Self {
            frame,
            parts,
            next_part: 0,
            waiting: false,
        })
    }

    /// Returns the next bytes or synchronization action.
    pub fn next_step(&mut self) -> CommandSendStep {
        let Some(part) = self.parts.get(self.next_part) else {
            return CommandSendStep::Complete;
        };
        match part {
            CommandSendPart::Bytes(range) => {
                self.next_part += 1;
                CommandSendStep::Bytes(self.frame.slice(range.clone()))
            }
            CommandSendPart::Continuation(request) => {
                self.waiting = true;
                CommandSendStep::ContinuationRequired(*request)
            }
        }
    }

    /// Confirms receipt of the server continuation currently being awaited.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::InvalidState`] if the next action is not an emitted
    /// continuation wait.
    pub fn acknowledge_continuation(&mut self) -> Result<(), ProtocolError> {
        if !self.waiting
            || !matches!(
                self.parts.get(self.next_part),
                Some(CommandSendPart::Continuation(_))
            )
        {
            return Err(ProtocolError::new(
                ErrorKind::InvalidState,
                "IMAP command continuation",
            ));
        }
        self.waiting = false;
        self.next_part += 1;
        Ok(())
    }

    /// Returns whether all command bytes have been released.
    pub fn is_complete(&self) -> bool {
        self.next_part == self.parts.len()
    }
}

fn command_send_parts(
    frame: &[u8],
    literal_plus: bool,
) -> Result<Vec<CommandSendPart>, ProtocolError> {
    let mut parts = Vec::new();
    let mut cursor = 0;
    let mut bytes_from = 0;
    loop {
        let line_end = find_crlf(frame, cursor).ok_or_else(|| {
            ProtocolError::new(ErrorKind::InvalidSyntax, "IMAP encoded command boundary")
        })?;
        let after_line = line_end + 2;
        let Some(spec) = literal_spec(&frame[cursor..line_end])? else {
            if after_line != frame.len() {
                return Err(ProtocolError::new(
                    ErrorKind::InvalidSyntax,
                    "IMAP encoded command boundary",
                )
                .at(after_line));
            }
            if bytes_from < after_line {
                parts.push(CommandSendPart::Bytes(bytes_from..after_line));
            }
            return Ok(parts);
        };
        validate_literal_mode(
            spec,
            FrameMode::Command {
                literal_plus,
                require_continuation: false,
            },
            line_end,
        )?;
        let data_end = after_line
            .checked_add(spec.length)
            .ok_or_else(|| ProtocolError::new(ErrorKind::FrameTooLarge, "IMAP literal length"))?;
        if data_end > frame.len() {
            return Err(ProtocolError::new(
                ErrorKind::InvalidSyntax,
                "truncated IMAP command literal",
            )
            .at(line_end));
        }
        if spec.kind == LiteralKind::Synchronizing {
            if bytes_from < after_line {
                parts.push(CommandSendPart::Bytes(bytes_from..after_line));
            }
            parts.push(CommandSendPart::Continuation(LiteralRequest {
                length: spec.length,
            }));
            bytes_from = after_line;
        }
        cursor = data_end;
    }
}
