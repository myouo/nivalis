use super::{
    BufMut, Bytes, BytesMut, DecodeStatus, Decoder, Encoder, ErrorKind, FrameMode, FrameScanner,
    Limits, ProtocolError, Response, Status, UntaggedData, append_transactionally, eq_ascii,
    parse_untagged, parse_untagged_with_max_depth, restore_frame, slice_for,
    validate_complete_line, validate_raw, validate_tag,
};

/// Incremental decoder for server responses.
#[derive(Clone, Debug, Default)]
pub struct ResponseDecoder {
    limits: Limits,
    scanner: FrameScanner,
}

impl ResponseDecoder {
    /// Creates a decoder with explicit resource limits.
    pub const fn new(limits: Limits) -> Self {
        Self {
            limits,
            scanner: FrameScanner::new(),
        }
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

impl Decoder for ResponseDecoder {
    type Item = Response;

    fn decode(&mut self, src: &mut BytesMut) -> Result<DecodeStatus<Response>, ProtocolError> {
        let Some(end) = self
            .scanner
            .frame_end(src, self.limits, FrameMode::Response)?
        else {
            return Ok(DecodeStatus::Incomplete);
        };
        let frame = src.split_to(end).freeze();
        let parsed = parse_response(&frame).and_then(|response| {
            match &response {
                Response::Untagged { .. } => {
                    parse_untagged_with_max_depth(&response, self.limits.max_nesting_depth())?;
                }
                Response::Tagged { .. } => {
                    response.parsed_response_code()?;
                }
                Response::Continuation { .. } => {}
            }
            Ok(response)
        });
        match parsed {
            Ok(response) => Ok(DecodeStatus::Complete(response)),
            Err(error) => {
                restore_frame(src, frame);
                Err(error)
            }
        }
    }
}

/// Encoder for IMAP response frames.
#[derive(Clone, Copy, Debug, Default)]
pub struct ResponseEncoder;

impl Encoder<Response> for ResponseEncoder {
    fn encode(&mut self, item: &Response, dst: &mut BytesMut) -> Result<(), ProtocolError> {
        append_transactionally(dst, |dst| encode_response(item, dst))
    }
}

fn encode_response(item: &Response, dst: &mut BytesMut) -> Result<(), ProtocolError> {
    match item {
        Response::Continuation { data } => {
            validate_complete_line(data, "IMAP continuation response", 0)?;
            dst.put_slice(b"+ ");
            dst.put_slice(data);
        }
        Response::Untagged { data } => {
            let parsed = parse_untagged(item)?;
            if matches!(parsed, Some(UntaggedData::Status(_))) {
                validate_complete_line(data, "IMAP untagged status response", 0)?;
            } else {
                validate_raw(data)?;
            }
            dst.put_u8(b'*');
            if !data.is_empty() {
                dst.put_u8(b' ');
                dst.put_slice(data);
            }
        }
        Response::Tagged {
            tag,
            status,
            information,
        } => {
            validate_tag(tag)?;
            validate_complete_line(information, "IMAP tagged response text", 0)?;
            item.parsed_response_code()?;
            dst.put_slice(tag);
            dst.put_u8(b' ');
            dst.put_slice(match status {
                Status::Ok => b"OK",
                Status::No => b"NO",
                Status::Bad => b"BAD",
            });
            dst.put_u8(b' ');
            dst.put_slice(information);
        }
    }
    dst.put_slice(b"\r\n");
    Ok(())
}

pub(super) fn parse_response(frame: &Bytes) -> Result<Response, ProtocolError> {
    let content = &frame[..frame.len() - 2];
    if content.first() == Some(&b'+') {
        if content.get(1) != Some(&b' ') {
            return Err(ProtocolError::new(
                ErrorKind::InvalidSyntax,
                "IMAP continuation response separator",
            )
            .at(1));
        }
        return Ok(Response::Continuation {
            data: frame.slice(2..content.len()),
        });
    }
    if content.first() == Some(&b'*') {
        if content.get(1) != Some(&b' ') || content.len() == 2 {
            return Err(ProtocolError::new(
                ErrorKind::InvalidSyntax,
                "IMAP untagged response separator",
            )
            .at(1));
        }
        return Ok(Response::Untagged {
            data: frame.slice(2..content.len()),
        });
    }
    let Some(tag_end) = content.iter().position(|byte| matches!(byte, b' ' | b'\t')) else {
        return Err(ProtocolError::new(
            ErrorKind::InvalidSyntax,
            "IMAP tagged response tag separator",
        ));
    };
    if tag_end == 0
        || content[tag_end] != b' '
        || content.get(tag_end + 1).is_none()
        || matches!(content.get(tag_end + 1), Some(b' ' | b'\t'))
    {
        return Err(ProtocolError::new(
            ErrorKind::InvalidSyntax,
            "IMAP tagged response tag separator",
        )
        .at(tag_end));
    }
    let tag = &content[..tag_end];
    let after_tag = &content[tag_end + 1..];
    validate_tag(tag)?;
    let status_end = after_tag
        .iter()
        .position(|byte| matches!(byte, b' ' | b'\t'));
    let (status, information) = if let Some(status_end) = status_end {
        if status_end == 0 || after_tag[status_end] != b' ' {
            return Err(ProtocolError::new(
                ErrorKind::InvalidSyntax,
                "IMAP tagged response text separator",
            )
            .at(tag_end + 1 + status_end));
        }
        (&after_tag[..status_end], &after_tag[status_end + 1..])
    } else {
        (after_tag, &after_tag[after_tag.len()..])
    };
    let status = if eq_ascii(status, b"OK") {
        Status::Ok
    } else if eq_ascii(status, b"NO") {
        Status::No
    } else if eq_ascii(status, b"BAD") {
        Status::Bad
    } else {
        return Err(ProtocolError::new(
            ErrorKind::InvalidSyntax,
            "IMAP tagged response status",
        ));
    };
    Ok(Response::Tagged {
        tag: slice_for(frame, tag),
        status,
        information: slice_for(frame, information),
    })
}
