use bytes::{BufMut, Bytes, BytesMut};
use mail_protocol_core::wire::{append_transactionally, eq_ascii, slice_ref as slice_for};
use mail_protocol_core::{Encoder, ErrorKind, ProtocolError};

use crate::{CapabilitySet, SequenceSet};

use super::{
    parse_uid_set, require_empty, require_non_empty, split_exact_required_argument,
    validate_flag_list, validate_response_atom,
};

/// Structured response code carried in square brackets.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ResponseCode {
    /// Human-facing alert.
    Alert,
    /// Destination mailbox already exists.
    AlreadyExists,
    /// Authentication failed for the supplied credentials.
    AuthenticationFailed,
    /// The authenticated identity is not authorized for the requested identity.
    AuthorizationFailed,
    /// Supported character sets, preserved as response-code data.
    BadCharset { charsets: Bytes },
    /// Capabilities supplied in a response code.
    Capability(CapabilitySet),
    /// The selected mailbox was closed.
    Closed,
    /// The command cannot be completed in the current server context.
    Cannot,
    /// The server detected an error in the client implementation.
    ClientBug,
    /// The user should contact the server administrator.
    ContactAdmin,
    /// The server detected corrupt data.
    Corruption,
    /// Authentication credentials or an account have expired.
    Expired,
    /// The command caused messages to be expunged.
    ExpungeIssued,
    /// A mailbox cannot be deleted because it has children.
    HasChildren,
    /// Highest modification sequence.
    HighestModSeq(u64),
    /// No modification sequence information is available.
    NoModSeq,
    /// The requested resource is currently in use.
    InUse,
    /// An implementation or administrative limit was exceeded.
    Limit,
    /// The requested resource does not exist.
    NonExistent,
    /// The authenticated identity lacks permission.
    NoPerm,
    /// The server refused to retain SEARCH results in the `$` variable.
    NotSaved,
    /// Message set modified during a conditional STORE.
    Modified(SequenceSet),
    /// The command contained a parse error in message data.
    Parse,
    /// A storage or administrative quota was exceeded.
    OverQuota,
    /// Flags that can be changed permanently.
    PermanentFlags { flags: Bytes },
    /// Mailbox selected read-only.
    ReadOnly,
    /// Mailbox selected read-write.
    ReadWrite,
    /// The operation requires stronger privacy protection.
    PrivacyRequired,
    /// The server detected an internal implementation error.
    ServerBug,
    /// The client should create the target mailbox.
    TryCreate,
    /// Next predicted unique identifier.
    UidNext(u64),
    /// Unique identifier validity value.
    UidValidity(u64),
    /// The selected mailbox does not provide persistent UIDs.
    UidNotSticky,
    /// A temporary subsystem failure made the operation unavailable.
    Unavailable,
    /// First unseen message sequence number.
    Unseen(u64),
    /// APPEND-assigned unique identifiers.
    AppendUid {
        uid_validity: u64,
        assigned: SequenceSet,
    },
    /// COPY/MOVE source-to-destination unique identifier mapping.
    CopyUid {
        uid_validity: u64,
        source: SequenceSet,
        destination: SequenceSet,
    },
    /// The requested section uses an unknown content-transfer encoding.
    UnknownCte,
    /// A response code not represented by a typed variant.
    Other { name: Bytes, data: Bytes },
}

impl ResponseCode {
    /// Parses exactly one response-code value without surrounding brackets.
    ///
    /// Field bytes are retained as zero-copy [`Bytes`] slices of `wire`.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid name, argument grammar, numeric bound,
    /// UID set, flag list, or extension payload.
    pub fn parse(wire: &Bytes) -> Result<Self, ProtocolError> {
        parse_response_code(wire, wire)
    }

    /// Appends the canonical response-code value without surrounding brackets.
    ///
    /// Validation completes before `dst` is changed.
    ///
    /// # Errors
    ///
    /// Returns an error when caller-constructed fields cannot be represented by
    /// the corresponding response-code grammar.
    pub fn encode(&self, dst: &mut BytesMut) -> Result<(), ProtocolError> {
        append_transactionally(dst, |dst| encode_response_code(self, dst))
    }

    /// Appends this value enclosed in `[` and `]`, without following response
    /// text or a line terminator.
    ///
    /// Validation completes before `dst` is changed.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::encode`].
    pub fn encode_bracketed(&self, dst: &mut BytesMut) -> Result<(), ProtocolError> {
        append_transactionally(dst, |dst| {
            dst.put_u8(b'[');
            encode_response_code(self, dst)?;
            dst.put_u8(b']');
            Ok(())
        })
    }
}

/// Validates exactly one unbracketed IMAP response-code value.
///
/// # Errors
///
/// Returns the same errors as [`ResponseCode::parse`].
pub fn validate_response_code(wire: &Bytes) -> Result<(), ProtocolError> {
    ResponseCode::parse(wire).map(|_| ())
}

/// Encoder for one unbracketed [`ResponseCode`] value.
#[derive(Clone, Copy, Debug, Default)]
pub struct ResponseCodeEncoder;

impl Encoder<ResponseCode> for ResponseCodeEncoder {
    fn encode(&mut self, item: &ResponseCode, dst: &mut BytesMut) -> Result<(), ProtocolError> {
        item.encode(dst)
    }
}

pub(super) fn parse_tagged_response_code(
    information: &Bytes,
) -> Result<Option<ResponseCode>, ProtocolError> {
    if information.first() != Some(&b'[') {
        return Ok(None);
    }
    let (code_data, _) = split_bracketed_response_text(information)?;
    parse_response_code(information, code_data).map(Some)
}

pub(super) fn parse_response_code(
    frame: &Bytes,
    input: &[u8],
) -> Result<ResponseCode, ProtocolError> {
    let (name, data) = split_response_code_name_data(input)?;
    if let Some(code) = parse_no_argument_response_code(name) {
        require_empty(data, "IMAP no-argument response code")?;
        Ok(code)
    } else if eq_ascii(name, b"BADCHARSET") {
        validate_badcharset_list(data)?;
        Ok(ResponseCode::BadCharset {
            charsets: slice_for(frame, data),
        })
    } else if eq_ascii(name, b"CAPABILITY") {
        let capability_data = slice_for(frame, input);
        Ok(ResponseCode::Capability(CapabilitySet::parse_response(
            &capability_data,
        )?))
    } else if eq_ascii(name, b"HIGHESTMODSEQ") {
        Ok(ResponseCode::HighestModSeq(parse_bounded_positive_u64(
            data,
            9_223_372_036_854_775_807,
            "IMAP HIGHESTMODSEQ response code",
        )?))
    } else if eq_ascii(name, b"MODIFIED") {
        Ok(ResponseCode::Modified(SequenceSet::parse(data)?))
    } else if eq_ascii(name, b"PERMANENTFLAGS") {
        require_non_empty(data, "IMAP PERMANENTFLAGS response code")?;
        validate_flag_list(data, true)?;
        Ok(ResponseCode::PermanentFlags {
            flags: slice_for(frame, data),
        })
    } else if eq_ascii(name, b"UIDNEXT") {
        Ok(ResponseCode::UidNext(parse_bounded_non_zero_u64(
            data,
            u64::from(u32::MAX),
            "IMAP UIDNEXT response code",
        )?))
    } else if eq_ascii(name, b"UIDVALIDITY") {
        Ok(ResponseCode::UidValidity(parse_bounded_non_zero_u64(
            data,
            u64::from(u32::MAX),
            "IMAP UIDVALIDITY response code",
        )?))
    } else if eq_ascii(name, b"UNSEEN") {
        Ok(ResponseCode::Unseen(parse_bounded_non_zero_u64(
            data,
            u64::from(u32::MAX),
            "IMAP UNSEEN response code",
        )?))
    } else if eq_ascii(name, b"APPENDUID") {
        let (validity, assigned) =
            split_exact_required_argument(data, "IMAP APPENDUID response code")?;
        let (assigned, assigned_count) =
            parse_uid_set(assigned, "IMAP APPENDUID assigned UID set")?;
        if assigned_count == 1
            && (assigned.ranges().len() != 1 || assigned.ranges()[0].end.is_some())
        {
            return Err(ProtocolError::new(
                ErrorKind::InvalidSyntax,
                "IMAP APPENDUID singleton range",
            ));
        }
        Ok(ResponseCode::AppendUid {
            uid_validity: parse_bounded_non_zero_u64(
                validity,
                u64::from(u32::MAX),
                "IMAP APPENDUID validity",
            )?,
            assigned,
        })
    } else if eq_ascii(name, b"COPYUID") {
        let (validity, remaining) =
            split_exact_required_argument(data, "IMAP COPYUID response code")?;
        let (source, destination) =
            split_exact_required_argument(remaining, "IMAP COPYUID source set")?;
        let (source, source_count) = parse_uid_set(source, "IMAP COPYUID source set")?;
        let (destination, destination_count) =
            parse_uid_set(destination, "IMAP COPYUID destination set")?;
        if source_count != destination_count {
            return Err(ProtocolError::new(
                ErrorKind::InvalidSyntax,
                "IMAP COPYUID source/destination cardinality",
            ));
        }
        Ok(ResponseCode::CopyUid {
            uid_validity: parse_bounded_non_zero_u64(
                validity,
                u64::from(u32::MAX),
                "IMAP COPYUID validity",
            )?,
            source,
            destination,
        })
    } else {
        Ok(ResponseCode::Other {
            name: slice_for(frame, name),
            data: slice_for(frame, data),
        })
    }
}

pub(crate) fn split_bracketed_response_text(input: &[u8]) -> Result<(&[u8], &[u8]), ProtocolError> {
    if input.first() != Some(&b'[') {
        return Err(invalid("IMAP bracketed response code"));
    }

    let mut quoted = false;
    let mut escaped = false;
    let mut close = None;
    for (index, byte) in input.iter().copied().enumerate().skip(1) {
        if escaped {
            escaped = false;
            continue;
        }
        if quoted && byte == b'\\' {
            escaped = true;
        } else if byte == b'"' {
            quoted = !quoted;
        } else if !quoted && byte == b']' {
            close = Some(index);
            break;
        }
    }
    let Some(close) = close else {
        return Err(invalid("IMAP bracketed response code"));
    };
    let code = &input[1..close];
    require_non_empty(code, "IMAP response code")?;
    if input.get(close + 1) != Some(&b' ') {
        return Err(invalid("IMAP response-code text separator"));
    }
    Ok((code, &input[close + 2..]))
}

fn encode_response_code(item: &ResponseCode, dst: &mut BytesMut) -> Result<(), ProtocolError> {
    if let Some(name) = no_argument_name(item) {
        dst.put_slice(name);
        return Ok(());
    }
    match item {
        ResponseCode::BadCharset { charsets } => {
            validate_badcharset_list(charsets)?;
            dst.put_slice(b"BADCHARSET");
            if !charsets.is_empty() {
                dst.put_u8(b' ');
                dst.put_slice(charsets);
            }
        }
        ResponseCode::Capability(capabilities) => {
            if !capabilities.contains(&crate::Capability::Imap4Rev1)
                && !capabilities.contains(&crate::Capability::Imap4Rev2)
            {
                return Err(invalid("IMAP CAPABILITY response code"));
            }
            dst.put_slice(b"CAPABILITY");
            for capability in capabilities.iter() {
                dst.put_u8(b' ');
                capability.encode(dst)?;
            }
        }
        ResponseCode::HighestModSeq(value) => {
            validate_number(
                *value,
                i64::MAX as u64,
                true,
                "IMAP HIGHESTMODSEQ response code",
            )?;
            dst.put_slice(b"HIGHESTMODSEQ ");
            put_u64(*value, dst);
        }
        ResponseCode::Modified(set) => {
            dst.put_slice(b"MODIFIED ");
            set.encode(dst);
        }
        ResponseCode::PermanentFlags { flags } => {
            validate_flag_list(flags, true)?;
            dst.put_slice(b"PERMANENTFLAGS ");
            dst.put_slice(flags);
        }
        ResponseCode::UidNext(value) => {
            encode_bounded_number(b"UIDNEXT", *value, u32::MAX.into(), dst)?;
        }
        ResponseCode::UidValidity(value) => {
            encode_bounded_number(b"UIDVALIDITY", *value, u32::MAX.into(), dst)?;
        }
        ResponseCode::Unseen(value) => {
            encode_bounded_number(b"UNSEEN", *value, u32::MAX.into(), dst)?;
        }
        ResponseCode::AppendUid {
            uid_validity,
            assigned,
        } => encode_append_uid(*uid_validity, assigned, dst)?,
        ResponseCode::CopyUid {
            uid_validity,
            source,
            destination,
        } => encode_copy_uid(*uid_validity, source, destination, dst)?,
        ResponseCode::Other { name, data } => {
            validate_response_atom(name, "IMAP response code name")?;
            if parse_no_argument_response_code(name).is_some() || is_typed_argument_name(name) {
                return Err(invalid("typed IMAP response code encoded as Other"));
            }
            validate_extension_data(data)?;
            dst.put_slice(name);
            if !data.is_empty() {
                dst.put_u8(b' ');
                dst.put_slice(data);
            }
        }
        _ => unreachable!("no-argument response code handled above"),
    }
    Ok(())
}

fn encode_append_uid(
    uid_validity: u64,
    assigned: &SequenceSet,
    dst: &mut BytesMut,
) -> Result<(), ProtocolError> {
    validate_number(
        uid_validity,
        u32::MAX.into(),
        true,
        "IMAP APPENDUID validity",
    )?;
    validate_uid_sequence_set(assigned, "IMAP APPENDUID assigned UID set")?;
    let (_, cardinality) = parse_uid_set_from_owned(assigned, "IMAP APPENDUID assigned UID set")?;
    if cardinality == 1 && (assigned.ranges().len() != 1 || assigned.ranges()[0].end.is_some()) {
        return Err(invalid("IMAP APPENDUID singleton range"));
    }
    dst.put_slice(b"APPENDUID ");
    put_u64(uid_validity, dst);
    dst.put_u8(b' ');
    assigned.encode(dst);
    Ok(())
}

fn encode_copy_uid(
    uid_validity: u64,
    source: &SequenceSet,
    destination: &SequenceSet,
    dst: &mut BytesMut,
) -> Result<(), ProtocolError> {
    validate_number(uid_validity, u32::MAX.into(), true, "IMAP COPYUID validity")?;
    let (_, source_count) = parse_uid_set_from_owned(source, "IMAP COPYUID source set")?;
    let (_, destination_count) =
        parse_uid_set_from_owned(destination, "IMAP COPYUID destination set")?;
    if source_count != destination_count {
        return Err(invalid("IMAP COPYUID source/destination cardinality"));
    }
    dst.put_slice(b"COPYUID ");
    put_u64(uid_validity, dst);
    dst.put_u8(b' ');
    source.encode(dst);
    dst.put_u8(b' ');
    destination.encode(dst);
    Ok(())
}

fn parse_uid_set_from_owned<'a>(
    set: &'a SequenceSet,
    context: &'static str,
) -> Result<(&'a SequenceSet, u64), ProtocolError> {
    validate_uid_sequence_set(set, context)?;
    let mut cardinality = 0u64;
    let mut merged_end: Option<u64> = None;
    loop {
        if let Some(end) = merged_end {
            let extension = set
                .ranges()
                .iter()
                .filter_map(numeric_interval)
                .filter(|(start, candidate_end)| {
                    *start <= end.saturating_add(1) && *candidate_end > end
                })
                .map(|(_, candidate_end)| candidate_end)
                .max();
            if let Some(extension) = extension {
                cardinality = cardinality
                    .checked_add(extension - end)
                    .ok_or_else(|| invalid(context))?;
                merged_end = Some(extension);
                continue;
            }
        }

        let next = set
            .ranges()
            .iter()
            .filter_map(numeric_interval)
            .filter(|(start, _)| merged_end.is_none_or(|end| *start > end))
            .min_by_key(|(start, _)| *start);
        let Some((start, end)) = next else {
            break;
        };
        cardinality = cardinality
            .checked_add(end - start + 1)
            .ok_or_else(|| invalid(context))?;
        merged_end = Some(end);
    }
    Ok((set, cardinality))
}

fn numeric_interval(range: &crate::SequenceRange) -> Option<(u64, u64)> {
    let crate::Sequence::Number(start) = range.start else {
        return None;
    };
    let end = match range.end {
        Some(crate::Sequence::Number(end)) => end,
        Some(crate::Sequence::Asterisk) => return None,
        None => start,
    };
    Some((start.min(end), start.max(end)))
}

fn validate_uid_sequence_set(
    set: &SequenceSet,
    context: &'static str,
) -> Result<(), ProtocolError> {
    if set.is_saved_search()
        || set.ranges().iter().any(|range| {
            matches!(range.start, crate::Sequence::Asterisk)
                || matches!(range.end, Some(crate::Sequence::Asterisk))
        })
    {
        Err(invalid(context))
    } else {
        Ok(())
    }
}

fn no_argument_name(item: &ResponseCode) -> Option<&'static [u8]> {
    Some(match item {
        ResponseCode::Alert => b"ALERT",
        ResponseCode::AlreadyExists => b"ALREADYEXISTS",
        ResponseCode::AuthenticationFailed => b"AUTHENTICATIONFAILED",
        ResponseCode::AuthorizationFailed => b"AUTHORIZATIONFAILED",
        ResponseCode::Closed => b"CLOSED",
        ResponseCode::Cannot => b"CANNOT",
        ResponseCode::ClientBug => b"CLIENTBUG",
        ResponseCode::ContactAdmin => b"CONTACTADMIN",
        ResponseCode::Corruption => b"CORRUPTION",
        ResponseCode::Expired => b"EXPIRED",
        ResponseCode::ExpungeIssued => b"EXPUNGEISSUED",
        ResponseCode::HasChildren => b"HASCHILDREN",
        ResponseCode::NoModSeq => b"NOMODSEQ",
        ResponseCode::InUse => b"INUSE",
        ResponseCode::Limit => b"LIMIT",
        ResponseCode::NonExistent => b"NONEXISTENT",
        ResponseCode::NoPerm => b"NOPERM",
        ResponseCode::NotSaved => b"NOTSAVED",
        ResponseCode::Parse => b"PARSE",
        ResponseCode::OverQuota => b"OVERQUOTA",
        ResponseCode::ReadOnly => b"READ-ONLY",
        ResponseCode::ReadWrite => b"READ-WRITE",
        ResponseCode::PrivacyRequired => b"PRIVACYREQUIRED",
        ResponseCode::ServerBug => b"SERVERBUG",
        ResponseCode::TryCreate => b"TRYCREATE",
        ResponseCode::UidNotSticky => b"UIDNOTSTICKY",
        ResponseCode::Unavailable => b"UNAVAILABLE",
        ResponseCode::UnknownCte => b"UNKNOWN-CTE",
        _ => return None,
    })
}

fn parse_no_argument_response_code(name: &[u8]) -> Option<ResponseCode> {
    Some(if eq_ascii(name, b"ALERT") {
        ResponseCode::Alert
    } else if eq_ascii(name, b"ALREADYEXISTS") {
        ResponseCode::AlreadyExists
    } else if eq_ascii(name, b"AUTHENTICATIONFAILED") {
        ResponseCode::AuthenticationFailed
    } else if eq_ascii(name, b"AUTHORIZATIONFAILED") {
        ResponseCode::AuthorizationFailed
    } else if eq_ascii(name, b"CANNOT") {
        ResponseCode::Cannot
    } else if eq_ascii(name, b"CLIENTBUG") {
        ResponseCode::ClientBug
    } else if eq_ascii(name, b"CLOSED") {
        ResponseCode::Closed
    } else if eq_ascii(name, b"CONTACTADMIN") {
        ResponseCode::ContactAdmin
    } else if eq_ascii(name, b"CORRUPTION") {
        ResponseCode::Corruption
    } else if eq_ascii(name, b"EXPIRED") {
        ResponseCode::Expired
    } else if eq_ascii(name, b"EXPUNGEISSUED") {
        ResponseCode::ExpungeIssued
    } else if eq_ascii(name, b"HASCHILDREN") {
        ResponseCode::HasChildren
    } else if eq_ascii(name, b"INUSE") {
        ResponseCode::InUse
    } else if eq_ascii(name, b"LIMIT") {
        ResponseCode::Limit
    } else if eq_ascii(name, b"NOMODSEQ") {
        ResponseCode::NoModSeq
    } else if eq_ascii(name, b"NONEXISTENT") {
        ResponseCode::NonExistent
    } else if eq_ascii(name, b"NOPERM") {
        ResponseCode::NoPerm
    } else if eq_ascii(name, b"NOTSAVED") {
        ResponseCode::NotSaved
    } else if eq_ascii(name, b"OVERQUOTA") {
        ResponseCode::OverQuota
    } else if eq_ascii(name, b"PARSE") {
        ResponseCode::Parse
    } else if eq_ascii(name, b"PRIVACYREQUIRED") {
        ResponseCode::PrivacyRequired
    } else if eq_ascii(name, b"READ-ONLY") {
        ResponseCode::ReadOnly
    } else if eq_ascii(name, b"READ-WRITE") {
        ResponseCode::ReadWrite
    } else if eq_ascii(name, b"SERVERBUG") {
        ResponseCode::ServerBug
    } else if eq_ascii(name, b"TRYCREATE") {
        ResponseCode::TryCreate
    } else if eq_ascii(name, b"UIDNOTSTICKY") {
        ResponseCode::UidNotSticky
    } else if eq_ascii(name, b"UNAVAILABLE") {
        ResponseCode::Unavailable
    } else if eq_ascii(name, b"UNKNOWN-CTE") {
        ResponseCode::UnknownCte
    } else {
        return None;
    })
}

fn split_response_code_name_data(input: &[u8]) -> Result<(&[u8], &[u8]), ProtocolError> {
    let boundary = input
        .iter()
        .position(|byte| matches!(byte, b' ' | b'\t'))
        .unwrap_or(input.len());
    let name = &input[..boundary];
    validate_response_atom(name, "IMAP response code name")?;
    if boundary == input.len() {
        return Ok((name, &input[input.len()..]));
    }
    if input[boundary] != b' ' {
        return Err(invalid("IMAP response code data separator"));
    }
    let data = &input[boundary + 1..];
    if data.is_empty() || data.first() == Some(&b' ') {
        return Err(invalid("IMAP response code data separator"));
    }
    Ok((name, data))
}

fn validate_badcharset_list(input: &[u8]) -> Result<(), ProtocolError> {
    if input.is_empty() {
        return Ok(());
    }
    if input.len() < 3 || input.first() != Some(&b'(') || input.last() != Some(&b')') {
        return Err(invalid("IMAP BADCHARSET charset list"));
    }
    let end = input.len() - 1;
    let mut cursor = 1;
    while cursor < end {
        cursor = parse_charset_token(input, cursor, end)?;
        if cursor == end {
            return Ok(());
        }
        if input.get(cursor) != Some(&b' ')
            || cursor + 1 == end
            || matches!(input.get(cursor + 1), Some(b' ' | b'\t'))
        {
            break;
        }
        cursor += 1;
    }
    Err(invalid("IMAP BADCHARSET charset list"))
}

fn parse_charset_token(input: &[u8], start: usize, end: usize) -> Result<usize, ProtocolError> {
    if input.get(start) != Some(&b'"') {
        let token_end = input[start..end]
            .iter()
            .position(|byte| *byte == b' ')
            .map_or(end, |offset| start + offset);
        validate_response_atom(&input[start..token_end], "IMAP BADCHARSET charset atom")?;
        return Ok(token_end);
    }
    let mut cursor = start + 1;
    while cursor < end {
        match input[cursor] {
            b'"' => return Ok(cursor + 1),
            b'\\' => {
                cursor += 1;
                if cursor == end || !matches!(input[cursor], b'"' | b'\\') {
                    break;
                }
            }
            byte if !byte.is_ascii() || byte.is_ascii_control() => break,
            _ => {}
        }
        cursor += 1;
    }
    Err(invalid("IMAP BADCHARSET quoted charset"))
}

fn validate_extension_data(input: &[u8]) -> Result<(), ProtocolError> {
    if input.iter().any(|byte| {
        !byte.is_ascii() || byte.is_ascii_control() || matches!(byte, b'\r' | b'\n' | b']')
    }) {
        Err(invalid("IMAP response code extension data"))
    } else {
        Ok(())
    }
}

fn is_typed_argument_name(name: &[u8]) -> bool {
    [
        b"BADCHARSET".as_slice(),
        b"CAPABILITY",
        b"HIGHESTMODSEQ",
        b"MODIFIED",
        b"PERMANENTFLAGS",
        b"UIDNEXT",
        b"UIDVALIDITY",
        b"UNSEEN",
        b"APPENDUID",
        b"COPYUID",
    ]
    .iter()
    .any(|candidate| eq_ascii(name, candidate))
}

fn encode_bounded_number(
    name: &[u8],
    value: u64,
    maximum: u64,
    dst: &mut BytesMut,
) -> Result<(), ProtocolError> {
    validate_number(value, maximum, true, "IMAP response code number")?;
    dst.put_slice(name);
    dst.put_u8(b' ');
    put_u64(value, dst);
    Ok(())
}

fn validate_number(
    value: u64,
    maximum: u64,
    non_zero: bool,
    context: &'static str,
) -> Result<(), ProtocolError> {
    if value > maximum || (non_zero && value == 0) {
        Err(invalid(context))
    } else {
        Ok(())
    }
}

fn parse_bounded_non_zero_u64(
    value: &[u8],
    maximum: u64,
    context: &'static str,
) -> Result<u64, ProtocolError> {
    let number = parse_u64(value, context)?;
    if number == 0 || value.first() == Some(&b'0') || number > maximum {
        return Err(invalid(context));
    }
    Ok(number)
}

fn parse_bounded_positive_u64(
    value: &[u8],
    maximum: u64,
    context: &'static str,
) -> Result<u64, ProtocolError> {
    let number = parse_u64(value, context)?;
    if number == 0 || number > maximum {
        return Err(invalid(context));
    }
    Ok(number)
}

fn parse_u64(value: &[u8], context: &'static str) -> Result<u64, ProtocolError> {
    if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
        return Err(invalid(context));
    }
    value
        .iter()
        .try_fold(0u64, |number, digit| {
            number
                .checked_mul(10)
                .and_then(|number| number.checked_add(u64::from(*digit - b'0')))
        })
        .ok_or_else(|| invalid(context))
}

fn put_u64(mut value: u64, dst: &mut BytesMut) {
    let mut digits = [0u8; 20];
    let mut cursor = digits.len();
    loop {
        cursor -= 1;
        digits[cursor] = b'0' + u8::try_from(value % 10).expect("one decimal digit");
        value /= 10;
        if value == 0 {
            break;
        }
    }
    dst.put_slice(&digits[cursor..]);
}

const fn invalid(context: &'static str) -> ProtocolError {
    ProtocolError::new(ErrorKind::InvalidSyntax, context)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_parse_is_zero_copy_and_encoder_round_trips() {
        let wire = Bytes::from_static(b"BADCHARSET (US-ASCII \"UTF]8\")");
        let pointer = wire.as_ptr();
        let parsed = ResponseCode::parse(&wire).unwrap();
        let ResponseCode::BadCharset { charsets } = &parsed else {
            panic!("expected BADCHARSET");
        };
        assert!(charsets.as_ptr() >= pointer);

        let mut encoded = BytesMut::new();
        parsed.encode(&mut encoded).unwrap();
        assert_eq!(encoded.as_ref(), wire.as_ref());

        let mut bracketed = BytesMut::new();
        parsed.encode_bracketed(&mut bracketed).unwrap();
        assert_eq!(bracketed.as_ref(), b"[BADCHARSET (US-ASCII \"UTF]8\")]");
    }

    #[test]
    fn every_argument_shape_round_trips_through_the_typed_boundary() {
        for wire in [
            b"ALERT".as_slice(),
            b"BADCHARSET",
            b"BADCHARSET (US-ASCII UTF-8)",
            b"CAPABILITY IMAP4rev2 ENABLE QRESYNC",
            b"HIGHESTMODSEQ 9223372036854775807",
            b"MODIFIED 1:3,*",
            b"PERMANENTFLAGS (\\Seen \\*)",
            b"UIDNEXT 4294967295",
            b"UIDVALIDITY 1",
            b"UNSEEN 7",
            b"APPENDUID 9 42",
            b"COPYUID 9 1:3 10:12",
            b"X-VENDOR reason",
        ] {
            let wire = Bytes::copy_from_slice(wire);
            let parsed = ResponseCode::parse(&wire).unwrap();
            let mut encoded = BytesMut::new();
            parsed.encode(&mut encoded).unwrap();
            assert_eq!(encoded.as_ref(), wire.as_ref(), "{wire:?}");
        }
    }

    #[test]
    fn encoding_is_atomic_for_invalid_public_fields() {
        let invalid = ResponseCode::Other {
            name: Bytes::from_static(b"UIDNEXT"),
            data: Bytes::from_static(b"1"),
        };
        let mut output = BytesMut::from(&b"prefix"[..]);
        assert!(invalid.encode(&mut output).is_err());
        assert_eq!(output.as_ref(), b"prefix");
    }

    #[test]
    fn bracket_scanner_ignores_closing_bracket_inside_quoted_charset() {
        let (code, text) =
            split_bracketed_response_text(b"[BADCHARSET (\"UTF]8\")] choose another").unwrap();
        assert_eq!(code, b"BADCHARSET (\"UTF]8\")");
        assert_eq!(text, b"choose another");
    }
}
