//! Small wire-format helpers shared by protocol implementations.

use bytes::{Bytes, BytesMut};
use memchr::memchr_iter;

use crate::ProtocolError;

/// Creates a zero-copy [`Bytes`] view of a parser-produced subslice.
///
/// Empty slices are canonicalized without relying on their pointer provenance;
/// non-empty slices are checked by [`Bytes::slice_ref`] to belong to `frame`.
#[inline]
pub fn slice_ref(frame: &Bytes, subslice: &[u8]) -> Bytes {
    frame.slice_ref(subslice)
}

/// Appends to `dst` and rolls its length back when `append` returns an error.
///
/// Capacity growth is retained, but all bytes that existed before the call remain
/// unchanged. This lets encoders validate incrementally without exposing partial
/// wire frames to their caller.
///
/// # Errors
///
/// Returns the error produced by `append` after restoring the original length.
#[inline]
pub fn append_transactionally(
    dst: &mut BytesMut,
    append: impl FnOnce(&mut BytesMut) -> Result<(), ProtocolError>,
) -> Result<(), ProtocolError> {
    let checkpoint = dst.len();
    let result = append(dst);
    if result.is_err() {
        dst.truncate(checkpoint);
    }
    result
}

/// Finds a CRLF sequence at or after `from` and returns the index of `\r`.
pub fn find_crlf(input: &[u8], from: usize) -> Option<usize> {
    if from >= input.len() {
        return None;
    }

    memchr_iter(b'\n', &input[from..]).find_map(|relative| {
        let newline = from + relative;
        if newline > from && input[newline - 1] == b'\r' {
            Some(newline - 1)
        } else {
            None
        }
    })
}

/// Trims ASCII spaces and horizontal tabs.
pub fn trim_ascii(mut input: &[u8]) -> &[u8] {
    input = trim_ascii_start(input);
    while matches!(input.last(), Some(b' ' | b'\t')) {
        input = &input[..input.len() - 1];
    }
    input
}

/// Trims leading ASCII spaces and horizontal tabs without changing the tail.
///
/// This distinction is important when the tail can contain an opaque protocol
/// literal whose final octets happen to be spaces or tabs.
pub fn trim_ascii_start(mut input: &[u8]) -> &[u8] {
    while matches!(input.first(), Some(b' ' | b'\t')) {
        input = &input[1..];
    }
    input
}

/// Splits at the first ASCII space or tab, ignoring repeated separators.
pub fn split_token(input: &[u8]) -> (&[u8], &[u8]) {
    let boundary = input
        .iter()
        .position(|byte| matches!(byte, b' ' | b'\t'))
        .unwrap_or(input.len());
    (&input[..boundary], trim_ascii(&input[boundary..]))
}

/// Splits at the first ASCII space or tab while preserving trailing octets.
///
/// Repeated separators before the returned tail are ignored. Unlike
/// [`split_token`], spaces and tabs at the end of that tail are retained.
pub fn split_token_preserve_tail(input: &[u8]) -> (&[u8], &[u8]) {
    let boundary = input
        .iter()
        .position(|byte| matches!(byte, b' ' | b'\t'))
        .unwrap_or(input.len());
    (&input[..boundary], trim_ascii_start(&input[boundary..]))
}

/// Compares an ASCII token without case sensitivity.
pub fn eq_ascii(input: &[u8], expected: &[u8]) -> bool {
    input.eq_ignore_ascii_case(expected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorKind;

    #[test]
    fn finds_only_crlf() {
        assert_eq!(find_crlf(b"a\nb\r\nc", 0), Some(3));
        assert_eq!(find_crlf(b"a\r\nc", 3), None);
        assert_eq!(find_crlf(b"x\n\r\n", 1), Some(2));
    }

    #[test]
    fn transactional_append_rolls_back_on_error() {
        let mut output = BytesMut::from(&b"existing"[..]);
        let error = append_transactionally(&mut output, |output| {
            output.extend_from_slice(b" partial");
            Err(ProtocolError::new(ErrorKind::InvalidSyntax, "test"))
        })
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidSyntax);
        assert_eq!(output.as_ref(), b"existing");
    }

    #[test]
    fn tail_preserving_split_keeps_opaque_trailing_whitespace() {
        assert_eq!(
            split_token_preserve_tail(b"token \t value \t"),
            (&b"token"[..], &b"value \t"[..])
        );
        assert_eq!(trim_ascii_start(b" \tbody \t"), b"body \t");
        assert_eq!(
            split_token_preserve_tail(b"token \t"),
            (&b"token"[..], &b""[..])
        );
    }

    #[test]
    fn byte_views_accept_provenance_free_empty_slices() {
        let frame = Bytes::from_static(b"frame");
        assert_eq!(slice_ref(&frame, &frame[1..4]).as_ref(), b"ram");
        assert!(slice_ref(&frame, &[]).is_empty());
    }
}
