use super::{
    Bytes, BytesMut, ErrorKind, Limits, LiteralRequest, ProtocolError, eq_ascii, find_crlf,
};

const REV2_NON_SYNC_LITERAL_LIMIT: usize = 4_096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LiteralKind {
    Synchronizing,
    NonSynchronizing,
    Binary,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct LiteralSpec {
    pub(super) length: usize,
    pub(super) kind: LiteralKind,
}

#[derive(Clone, Copy, Debug)]
struct PendingLiteral {
    header_end: usize,
    data_end: usize,
    spec: LiteralSpec,
    accepted: bool,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum FrameMode {
    Command {
        literal_plus: bool,
        require_continuation: bool,
    },
    Response,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FrameProgress {
    Complete(usize),
    Incomplete,
    ContinuationRequired(LiteralRequest),
}

#[derive(Clone, Debug, Default)]
pub(super) struct FrameScanner {
    cursor: usize,
    search_from: usize,
    pending_literal: Option<PendingLiteral>,
    observed_len: usize,
}

impl FrameScanner {
    pub(super) const fn new() -> Self {
        Self {
            cursor: 0,
            search_from: 0,
            pending_literal: None,
            observed_len: 0,
        }
    }

    pub(super) const fn reset(&mut self) {
        self.cursor = 0;
        self.search_from = 0;
        self.pending_literal = None;
        self.observed_len = 0;
    }

    pub(super) fn frame_end(
        &mut self,
        src: &[u8],
        limits: Limits,
        mode: FrameMode,
    ) -> Result<Option<usize>, ProtocolError> {
        match self.scan(src, limits, mode)? {
            FrameProgress::Complete(end) => Ok(Some(end)),
            FrameProgress::Incomplete => Ok(None),
            FrameProgress::ContinuationRequired(_) => Err(ProtocolError::new(
                ErrorKind::InvalidState,
                "IMAP literal continuation",
            )),
        }
    }

    pub(super) fn scan(
        &mut self,
        src: &[u8],
        limits: Limits,
        mode: FrameMode,
    ) -> Result<FrameProgress, ProtocolError> {
        if src.len() < self.observed_len {
            self.reset();
        }
        let result = self.scan_inner(src, limits, mode);
        if result.is_err() || matches!(result, Ok(FrameProgress::Complete(_))) {
            self.reset();
        }
        result
    }

    pub(super) fn acknowledge_literal(&mut self) -> Result<(), ProtocolError> {
        let Some(pending) = self.pending_literal.as_mut() else {
            return Err(ProtocolError::new(
                ErrorKind::InvalidState,
                "IMAP literal continuation",
            ));
        };
        if pending.spec.kind != LiteralKind::Synchronizing || pending.accepted {
            return Err(ProtocolError::new(
                ErrorKind::InvalidState,
                "IMAP literal continuation",
            ));
        }
        pending.accepted = true;
        Ok(())
    }

    fn scan_inner(
        &mut self,
        src: &[u8],
        limits: Limits,
        mode: FrameMode,
    ) -> Result<FrameProgress, ProtocolError> {
        loop {
            if let Some(pending) = self.pending_literal {
                if !pending.accepted {
                    if src.len() > pending.header_end {
                        return Err(ProtocolError::new(
                            ErrorKind::InvalidState,
                            "IMAP literal sent before continuation",
                        )
                        .at(pending.header_end));
                    }
                    self.observed_len = src.len();
                    return Ok(FrameProgress::ContinuationRequired(LiteralRequest {
                        length: pending.spec.length,
                    }));
                }
                if src.len() < pending.data_end {
                    self.observed_len = src.len();
                    return Ok(FrameProgress::Incomplete);
                }
                self.cursor = pending.data_end;
                self.search_from = pending.data_end;
                self.pending_literal = None;
            }
            if self.cursor > limits.max_frame_len() {
                return Err(ProtocolError::new(ErrorKind::FrameTooLarge, "IMAP frame"));
            }
            let Some(line_end) = find_crlf(src, self.search_from) else {
                validate_incomplete_line(&src[self.search_from..], "IMAP line", self.search_from)?;
                if src.len().saturating_sub(self.cursor) > limits.max_line_len() {
                    return Err(
                        ProtocolError::new(ErrorKind::LineTooLong, "IMAP line").at(self.cursor)
                    );
                }
                if src.len() > limits.max_frame_len() {
                    return Err(ProtocolError::new(ErrorKind::FrameTooLarge, "IMAP frame"));
                }
                self.search_from = src.len().saturating_sub(1).max(self.cursor);
                self.observed_len = src.len();
                return Ok(FrameProgress::Incomplete);
            };
            validate_complete_line(&src[self.cursor..line_end], "IMAP line", self.cursor)?;
            if line_end - self.cursor > limits.max_line_len() {
                return Err(ProtocolError::new(ErrorKind::LineTooLong, "IMAP line").at(self.cursor));
            }
            let after_line = line_end + 2;
            let line = &src[self.cursor..line_end];
            let spec = if matches!(mode, FrameMode::Response)
                && !response_segment_allows_literal(line, self.cursor)
            {
                None
            } else {
                literal_spec(line)?
            };
            let Some(spec) = spec else {
                if after_line > limits.max_frame_len() {
                    return Err(ProtocolError::new(ErrorKind::FrameTooLarge, "IMAP frame"));
                }
                return Ok(FrameProgress::Complete(after_line));
            };
            validate_literal_mode(spec, mode, line_end)?;
            if spec.length > limits.max_literal_len() {
                return Err(
                    ProtocolError::new(ErrorKind::LiteralTooLarge, "IMAP literal").at(line_end),
                );
            }
            let data_end = after_line.checked_add(spec.length).ok_or_else(|| {
                ProtocolError::new(ErrorKind::FrameTooLarge, "IMAP literal length")
            })?;
            if data_end > limits.max_frame_len() {
                return Err(ProtocolError::new(ErrorKind::FrameTooLarge, "IMAP frame"));
            }
            let require_continuation = matches!(
                mode,
                FrameMode::Command {
                    require_continuation: true,
                    ..
                }
            ) && spec.kind == LiteralKind::Synchronizing;
            self.pending_literal = Some(PendingLiteral {
                header_end: after_line,
                data_end,
                spec,
                accepted: !require_continuation,
            });
        }
    }
}

fn response_segment_allows_literal(line: &[u8], offset: usize) -> bool {
    if offset != 0 {
        return true;
    }
    if line.first() == Some(&b'+') || line.first() != Some(&b'*') {
        return false;
    }
    let Some(data) = line.strip_prefix(b"* ") else {
        return false;
    };
    let name_end = data
        .iter()
        .position(|byte| matches!(byte, b' ' | b'\t'))
        .unwrap_or(data.len());
    let name = &data[..name_end];
    ![b"OK".as_slice(), b"NO", b"BAD", b"BYE", b"PREAUTH"]
        .iter()
        .any(|status| eq_ascii(name, status))
}

pub(super) fn validate_literal_mode(
    spec: LiteralSpec,
    mode: FrameMode,
    offset: usize,
) -> Result<(), ProtocolError> {
    match (mode, spec.kind) {
        (FrameMode::Command { .. }, LiteralKind::Binary) => {
            Err(ProtocolError::new(ErrorKind::InvalidSyntax, "IMAP literal8 in command").at(offset))
        }
        (FrameMode::Response, LiteralKind::NonSynchronizing) => Err(ProtocolError::new(
            ErrorKind::InvalidSyntax,
            "IMAP non-synchronizing server literal",
        )
        .at(offset)),
        (
            FrameMode::Command {
                literal_plus: false,
                ..
            },
            LiteralKind::NonSynchronizing,
        ) if spec.length > REV2_NON_SYNC_LITERAL_LIMIT => Err(ProtocolError::new(
            ErrorKind::LiteralTooLarge,
            "IMAP4rev2 non-synchronizing literal",
        )
        .at(offset)),
        _ => Ok(()),
    }
}

pub(super) fn literal_spec(line: &[u8]) -> Result<Option<LiteralSpec>, ProtocolError> {
    if line.last() != Some(&b'}') {
        return Ok(None);
    }
    let Some(open) = line.iter().rposition(|byte| *byte == b'{') else {
        return Ok(None);
    };
    let binary = open > 0 && line[open - 1] == b'~';
    let marker_start = if binary { open - 1 } else { open };
    let mut digits = &line[open + 1..line.len() - 1];
    let non_synchronizing = digits.last() == Some(&b'+');
    if non_synchronizing {
        digits = &digits[..digits.len() - 1];
    }
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return Ok(None);
    }
    if !literal_marker_is_unquoted(line, marker_start)
        || (marker_start > 0 && !matches!(line[marker_start - 1], b' ' | b'\t' | b'('))
    {
        return Err(
            ProtocolError::new(ErrorKind::InvalidSyntax, "IMAP literal marker boundary")
                .at(marker_start),
        );
    }
    if binary && non_synchronizing {
        return Err(ProtocolError::new(
            ErrorKind::InvalidSyntax,
            "IMAP literal8 marker",
        ));
    }
    let mut value = 0u64;
    for digit in digits {
        value = value
            .checked_mul(10)
            .and_then(|number| number.checked_add(u64::from(*digit - b'0')))
            .ok_or_else(|| ProtocolError::new(ErrorKind::LiteralTooLarge, "IMAP literal length"))?;
    }
    if value > i64::MAX as u64 {
        return Err(ProtocolError::new(
            ErrorKind::LiteralTooLarge,
            "IMAP literal length",
        ));
    }
    let length = usize::try_from(value)
        .map_err(|_| ProtocolError::new(ErrorKind::LiteralTooLarge, "IMAP literal length"))?;
    let kind = if binary {
        LiteralKind::Binary
    } else if non_synchronizing {
        LiteralKind::NonSynchronizing
    } else {
        LiteralKind::Synchronizing
    };
    Ok(Some(LiteralSpec { length, kind }))
}

fn literal_marker_is_unquoted(line: &[u8], marker_start: usize) -> bool {
    let mut quoted = false;
    let mut escaped = false;
    for byte in &line[..marker_start] {
        if !quoted {
            if *byte == b'"' {
                quoted = true;
            }
        } else if escaped {
            escaped = false;
        } else if *byte == b'\\' {
            escaped = true;
        } else if *byte == b'"' {
            quoted = false;
        }
    }
    !quoted
}

pub(crate) fn validate_tag(tag: &[u8]) -> Result<(), ProtocolError> {
    if tag.is_empty()
        || tag.iter().any(|byte| {
            !byte.is_ascii()
                || byte.is_ascii_control()
                || matches!(
                    byte,
                    b' ' | b'(' | b')' | b'{' | b'%' | b'*' | b'"' | b'\\' | b'+' | b']'
                )
        })
    {
        return Err(ProtocolError::new(ErrorKind::InvalidSyntax, "IMAP tag"));
    }
    Ok(())
}

pub(super) fn validate_atom(value: &[u8], context: &'static str) -> Result<(), ProtocolError> {
    if value.is_empty()
        || value.iter().any(|byte| {
            byte.is_ascii_control()
                || matches!(
                    byte,
                    b' ' | b'(' | b')' | b'{' | b'%' | b'*' | b'"' | b'\\' | b']'
                )
        })
    {
        return Err(ProtocolError::new(ErrorKind::InvalidSyntax, context));
    }
    Ok(())
}

pub(super) fn validate_raw(value: &[u8]) -> Result<(), ProtocolError> {
    let mut cursor = 0;
    while let Some(line_end) = find_crlf(value, cursor) {
        validate_complete_line(&value[cursor..line_end], "raw IMAP value", cursor)?;
        let Some(spec) = literal_spec(&value[cursor..line_end])? else {
            return Err(
                ProtocolError::new(ErrorKind::InvalidSyntax, "raw IMAP literal boundary")
                    .at(line_end),
            );
        };
        cursor = (line_end + 2).checked_add(spec.length).ok_or_else(|| {
            ProtocolError::new(ErrorKind::FrameTooLarge, "raw IMAP literal length")
        })?;
        if cursor > value.len() {
            return Err(
                ProtocolError::new(ErrorKind::InvalidSyntax, "truncated raw IMAP literal")
                    .at(line_end),
            );
        }
    }
    validate_complete_line(&value[cursor..], "raw IMAP value", cursor)?;
    Ok(())
}

pub(super) fn validate_complete_line(
    line: &[u8],
    context: &'static str,
    offset: usize,
) -> Result<(), ProtocolError> {
    if let Some(relative) = line
        .iter()
        .position(|byte| matches!(byte, b'\0' | b'\r' | b'\n'))
    {
        return Err(ProtocolError::new(ErrorKind::InvalidSyntax, context).at(offset + relative));
    }
    if let Err(error) = core::str::from_utf8(line) {
        return Err(
            ProtocolError::new(ErrorKind::InvalidSyntax, context).at(offset + error.valid_up_to())
        );
    }
    Ok(())
}

pub(super) fn validate_incomplete_line(
    line: &[u8],
    context: &'static str,
    offset: usize,
) -> Result<(), ProtocolError> {
    if let Some(relative) = line
        .iter()
        .position(|byte| matches!(byte, b'\0' | b'\r' | b'\n'))
    {
        if line[relative] != b'\r' || relative + 1 != line.len() {
            return Err(ProtocolError::new(ErrorKind::InvalidSyntax, context).at(offset + relative));
        }
    }
    Ok(())
}

pub(super) fn restore_frame(src: &mut BytesMut, frame: Bytes) {
    if src.is_empty() {
        *src = BytesMut::new();
        match frame.try_into_mut() {
            Ok(restored) => {
                *src = restored;
                return;
            }
            Err(frame) => restore_frame_by_copy(src, &frame),
        }
        return;
    }
    restore_frame_by_copy(src, &frame);
}

fn restore_frame_by_copy(src: &mut BytesMut, frame: &[u8]) {
    let mut restored = BytesMut::with_capacity(frame.len() + src.len());
    restored.extend_from_slice(frame);
    restored.extend_from_slice(src);
    *src = restored;
}
