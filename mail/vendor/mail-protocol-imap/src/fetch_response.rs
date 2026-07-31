//! Typed, zero-copy FETCH response validation and BODYSTRUCTURE views.

mod body;
mod encode;
mod items;
mod values;

#[cfg(test)]
mod tests;

use mail_protocol_core::{ErrorKind, ProtocolError};

pub use body::{
    BodyDisposition, BodyExtensionIter, BodyExtensions, BodyFields, BodyLanguage, BodyLanguageIter,
    BodyParameter, BodyParameterIter, BodyParameters, BodyPartIter, BodyStructure,
    BodyStructureKind, BodyStructureView,
};
pub use items::{FetchResponse, FetchResponseItem, FetchResponseItemIter};
pub use values::{
    FetchAddress, FetchAddressIter, FetchAddressList, FetchBinaryData, FetchEnvelope, FetchFlag,
    FetchFlagIter, FetchFlags, FetchNString, FetchString, FetchStringKind,
};

use values::{parse_envelope, parse_nstring, parse_string};

/// Default maximum nesting accepted in FETCH response BODYSTRUCTURE and
/// extension values.
pub const DEFAULT_FETCH_RESPONSE_MAX_DEPTH: usize = 64;
pub(super) const MAX_NUMBER: u64 = u32::MAX as u64;
pub(super) const MAX_NUMBER64: u64 = i64::MAX as u64;

pub(super) fn parse_number_token(
    input: &[u8],
    start: usize,
    maximum: u64,
    non_zero: bool,
    reject_leading_zero: bool,
) -> Result<(u64, usize), ProtocolError> {
    let mut cursor = start;
    let mut value = 0u64;
    while let Some(digit) = input.get(cursor).filter(|byte| byte.is_ascii_digit()) {
        if reject_leading_zero && cursor == start && *digit == b'0' {
            return Err(invalid("IMAP FETCH response number").at(start));
        }
        value = value
            .checked_mul(10)
            .and_then(|number| number.checked_add(u64::from(*digit - b'0')))
            .filter(|number| *number <= maximum)
            .ok_or_else(|| invalid("IMAP FETCH response number").at(start))?;
        cursor += 1;
    }
    if cursor == start || non_zero && value == 0 {
        return Err(invalid("IMAP FETCH response number").at(start));
    }
    Ok((value, cursor))
}

pub(super) fn required_space(
    input: &[u8],
    cursor: usize,
    context: &'static str,
) -> Result<usize, ProtocolError> {
    if input.get(cursor) != Some(&b' ')
        || input.get(cursor + 1).is_none()
        || input.get(cursor + 1) == Some(&b' ')
    {
        Err(invalid(context).at(cursor))
    } else {
        Ok(cursor + 1)
    }
}

pub(super) fn validate_atom(value: &[u8], context: &'static str) -> Result<(), ProtocolError> {
    if value.is_empty()
        || value.iter().any(|byte| {
            !byte.is_ascii()
                || byte.is_ascii_control()
                || matches!(byte, b'(' | b')' | b'{' | b' ' | b'%' | b'*' | b'"' | b'\\')
        })
    {
        Err(invalid(context))
    } else {
        Ok(())
    }
}

pub(super) fn starts_ascii(input: &[u8], prefix: &[u8]) -> bool {
    input
        .get(..prefix.len())
        .is_some_and(|value| value.eq_ignore_ascii_case(prefix))
}

pub(super) fn shift_error(error: ProtocolError, start: usize) -> ProtocolError {
    error.at(start.saturating_add(error.offset().unwrap_or(0)))
}

pub(super) const fn invalid(context: &'static str) -> ProtocolError {
    ProtocolError::new(ErrorKind::InvalidSyntax, context)
}

pub(super) fn nesting_too_deep(offset: usize) -> ProtocolError {
    ProtocolError::new(ErrorKind::NestingTooDeep, "IMAP FETCH response nesting").at(offset)
}
