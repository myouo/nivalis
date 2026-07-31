use super::{
    AuthenticateContinuation, BufMut, BytesMut, DecodeStatus, Decoder, Encoder, ErrorKind,
    IdleDone, Limits, ProtocolError, append_transactionally, eq_ascii, find_crlf, restore_frame,
    validate_complete_line, validate_incomplete_line,
};

/// Incremental decoder for client response lines during AUTHENTICATE.
#[derive(Clone, Debug)]
pub struct AuthenticateContinuationDecoder {
    scanner: ClientLineScanner,
}

impl AuthenticateContinuationDecoder {
    /// Creates a decoder with explicit line and frame limits.
    pub const fn new(limits: Limits) -> Self {
        Self {
            scanner: ClientLineScanner::new(limits),
        }
    }

    /// Discards incremental progress without consuming caller-owned input.
    pub const fn reset(&mut self) {
        self.scanner.reset();
    }
}

impl Default for AuthenticateContinuationDecoder {
    fn default() -> Self {
        Self::new(Limits::default())
    }
}

impl Decoder for AuthenticateContinuationDecoder {
    type Item = AuthenticateContinuation;

    fn decode(&mut self, src: &mut BytesMut) -> Result<DecodeStatus<Self::Item>, ProtocolError> {
        let Some(line_end) = self.scanner.line_end(src, "IMAP AUTHENTICATE response")? else {
            return Ok(DecodeStatus::Incomplete);
        };
        let frame = src.split_to(line_end + 2).freeze();
        let content = frame.slice(..line_end);
        let result = if content.as_ref() == b"*" {
            Ok(AuthenticateContinuation::Cancel)
        } else {
            validate_base64(&content).map(|()| AuthenticateContinuation::Response(content))
        };
        match result {
            Ok(item) => Ok(DecodeStatus::Complete(item)),
            Err(error) => {
                restore_frame(src, frame);
                Err(error)
            }
        }
    }
}

/// Encoder for client response lines during AUTHENTICATE.
#[derive(Clone, Copy, Debug, Default)]
pub struct AuthenticateContinuationEncoder;

impl Encoder<AuthenticateContinuation> for AuthenticateContinuationEncoder {
    fn encode(
        &mut self,
        item: &AuthenticateContinuation,
        dst: &mut BytesMut,
    ) -> Result<(), ProtocolError> {
        append_transactionally(dst, |dst| {
            match item {
                AuthenticateContinuation::Response(data) => {
                    validate_base64(data)?;
                    dst.put_slice(data);
                }
                AuthenticateContinuation::Cancel => dst.put_u8(b'*'),
            }
            dst.put_slice(b"\r\n");
            Ok(())
        })
    }
}

/// Incremental decoder for the client `DONE` continuation that ends IDLE.
#[derive(Clone, Debug)]
pub struct IdleDoneDecoder {
    scanner: ClientLineScanner,
}

impl IdleDoneDecoder {
    /// Creates a decoder with explicit line and frame limits.
    pub const fn new(limits: Limits) -> Self {
        Self {
            scanner: ClientLineScanner::new(limits),
        }
    }

    /// Discards incremental progress without consuming caller-owned input.
    pub const fn reset(&mut self) {
        self.scanner.reset();
    }
}

impl Default for IdleDoneDecoder {
    fn default() -> Self {
        Self::new(Limits::default())
    }
}

impl Decoder for IdleDoneDecoder {
    type Item = IdleDone;

    fn decode(&mut self, src: &mut BytesMut) -> Result<DecodeStatus<Self::Item>, ProtocolError> {
        let Some(line_end) = self.scanner.line_end(src, "IMAP IDLE continuation")? else {
            return Ok(DecodeStatus::Incomplete);
        };
        let frame = src.split_to(line_end + 2).freeze();
        if eq_ascii(&frame[..line_end], b"DONE") {
            Ok(DecodeStatus::Complete(IdleDone))
        } else {
            restore_frame(src, frame);
            Err(ProtocolError::new(
                ErrorKind::InvalidSyntax,
                "IMAP IDLE continuation",
            ))
        }
    }
}

/// Encoder for the canonical client `DONE` continuation.
#[derive(Clone, Copy, Debug, Default)]
pub struct IdleDoneEncoder;

impl Encoder<IdleDone> for IdleDoneEncoder {
    fn encode(&mut self, _: &IdleDone, dst: &mut BytesMut) -> Result<(), ProtocolError> {
        dst.put_slice(b"DONE\r\n");
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct ClientLineScanner {
    limits: Limits,
    search_from: usize,
    observed_len: usize,
}

impl ClientLineScanner {
    const fn new(limits: Limits) -> Self {
        Self {
            limits,
            search_from: 0,
            observed_len: 0,
        }
    }

    const fn reset(&mut self) {
        self.search_from = 0;
        self.observed_len = 0;
    }

    fn line_end(
        &mut self,
        src: &[u8],
        context: &'static str,
    ) -> Result<Option<usize>, ProtocolError> {
        if src.len() < self.observed_len {
            self.reset();
        }
        let result = self.line_end_inner(src, context);
        if result.is_err() || matches!(result, Ok(Some(_))) {
            self.reset();
        }
        result
    }

    fn line_end_inner(
        &mut self,
        src: &[u8],
        context: &'static str,
    ) -> Result<Option<usize>, ProtocolError> {
        if let Some(end) = find_crlf(src, self.search_from) {
            validate_complete_line(&src[..end], context, 0)?;
            if end > self.limits.max_line_len() {
                Err(ProtocolError::new(ErrorKind::LineTooLong, context))
            } else if end + 2 > self.limits.max_frame_len() {
                Err(ProtocolError::new(ErrorKind::FrameTooLarge, context))
            } else {
                Ok(Some(end))
            }
        } else {
            validate_incomplete_line(&src[self.search_from..], context, self.search_from)?;
            if src.len() > self.limits.max_line_len() {
                Err(ProtocolError::new(ErrorKind::LineTooLong, context))
            } else if src.len() > self.limits.max_frame_len() {
                Err(ProtocolError::new(ErrorKind::FrameTooLarge, context))
            } else {
                self.search_from = src.len().saturating_sub(1);
                self.observed_len = src.len();
                Ok(None)
            }
        }
    }
}

pub(crate) fn validate_base64(value: &[u8]) -> Result<(), ProtocolError> {
    if value.len() % 4 != 0 {
        return Err(ProtocolError::new(
            ErrorKind::InvalidSyntax,
            "IMAP AUTHENTICATE base64",
        ));
    }
    let padding = value.iter().rev().take_while(|byte| **byte == b'=').count();
    if padding > 2
        || value[..value.len() - padding]
            .iter()
            .any(|byte| !matches!(byte, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'+' | b'/'))
        || value[value.len() - padding..]
            .iter()
            .any(|byte| *byte != b'=')
    {
        return Err(ProtocolError::new(
            ErrorKind::InvalidSyntax,
            "IMAP AUTHENTICATE base64",
        ));
    }
    Ok(())
}

pub(super) fn validate_initial_response(value: &[u8]) -> Result<(), ProtocolError> {
    if value == b"=" {
        Ok(())
    } else {
        validate_base64(value)
    }
}
