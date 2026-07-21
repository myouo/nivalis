use bytes::Bytes;
use mail_protocol_core::wire::{eq_ascii, slice_ref as slice_for, split_token_preserve_tail};
use mail_protocol_core::{ErrorKind, ProtocolError};

use crate::{
    CapabilitySet, DEFAULT_FETCH_RESPONSE_MAX_DEPTH, DEFAULT_NAMESPACE_MAX_ITEMS,
    DEFAULT_THREAD_MAX_NODES, ESearchResponse, FetchResponse, IdParameters, ListResponse,
    MailboxStatus, NamespaceResponse, QuotaResponse, QuotaRootResponse, Response, SearchResponse,
    Sequence, SequenceSet, SortResponse, ThreadResponse,
};

mod status_code;

pub(crate) use status_code::split_bracketed_response_text;
pub use status_code::{ResponseCode, ResponseCodeEncoder, validate_response_code};
use status_code::{parse_response_code, parse_tagged_response_code};

/// Status atom at the start of an untagged status response.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum StatusKind {
    /// Informational or successful status.
    Ok,
    /// Operational failure.
    No,
    /// Protocol/syntax failure.
    Bad,
    /// Server is closing the connection.
    Bye,
    /// Connection starts in authenticated state.
    Preauth,
}

/// Parsed untagged status response.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatusResponse {
    /// Leading status atom.
    pub kind: StatusKind,
    /// Optional bracketed response code.
    pub code: Option<ResponseCode>,
    /// Human-readable response text after the optional code.
    pub text: Bytes,
}

/// Common untagged server data with an extension-preserving fallback.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum UntaggedData {
    /// OK/NO/BAD/BYE/PREAUTH status response.
    Status(StatusResponse),
    /// Number of messages in the selected mailbox.
    Exists(u32),
    /// Legacy recent-message count.
    Recent(u32),
    /// Message sequence number removed by EXPUNGE.
    Expunge(u32),
    /// FETCH data for one message sequence number.
    Fetch { sequence: u32, data: FetchResponse },
    /// Mailbox flags.
    Flags(Bytes),
    /// Capability advertisement.
    Capability(CapabilitySet),
    /// Capabilities successfully activated by an ENABLE command.
    Enabled(CapabilitySet),
    /// STATUS values for one mailbox.
    MailboxStatus(MailboxStatus),
    /// RFC 2971 server implementation identification parameters.
    Id(IdParameters),
    /// Personal, other-user, and shared mailbox namespaces.
    Namespace(NamespaceResponse),
    /// LIST mailbox metadata.
    List(ListResponse),
    /// Legacy LSUB mailbox metadata.
    Lsub(ListResponse),
    /// VANISHED unique identifiers.
    Vanished { earlier: bool, uids: SequenceSet },
    /// Extended SEARCH result data.
    ESearch(ESearchResponse),
    /// Legacy SEARCH result data.
    Search(SearchResponse),
    /// RFC 5256 SORT result data.
    Sort(SortResponse),
    /// RFC 5256 threaded result forest.
    Thread(ThreadResponse),
    /// RFC 9208 quota resource usage and limits.
    Quota(QuotaResponse),
    /// RFC 9208 quota roots applicable to one mailbox.
    QuotaRoot(QuotaRootResponse),
    /// Named data not represented by a typed variant.
    Other { name: Bytes, data: Bytes },
}

impl Response {
    /// Parses the optional bracketed response code on a tagged or untagged
    /// status response.
    ///
    /// Returns `Ok(None)` for continuations, non-status untagged data, or a
    /// status response without a response code.
    ///
    /// # Errors
    ///
    /// Returns an error when a bracketed response code is malformed.
    pub fn parsed_response_code(&self) -> Result<Option<ResponseCode>, ProtocolError> {
        match self {
            Self::Tagged { information, .. } => parse_tagged_response_code(information),
            Self::Untagged { .. } => match parse_untagged(self)? {
                Some(UntaggedData::Status(status)) => Ok(status.code),
                _ => Ok(None),
            },
            Self::Continuation { .. } => Ok(None),
        }
    }
}

/// Parses the semantic content of an untagged response.
///
/// Returns `Ok(None)` for tagged and continuation responses.
///
/// # Errors
///
/// Returns an error when a recognized response uses malformed numbers,
/// sequence-sets, response codes, or required arguments.
pub fn parse_untagged(response: &Response) -> Result<Option<UntaggedData>, ProtocolError> {
    parse_untagged_with_max_depth(response, DEFAULT_FETCH_RESPONSE_MAX_DEPTH)
}

/// Parses semantic untagged response content with an explicit recursive
/// FETCH/LIST extension and NAMESPACE structural budget.
///
/// # Errors
///
/// Returns the same errors as [`parse_untagged`] and `NestingTooDeep` when a
/// FETCH BODYSTRUCTURE, LIST extension, or NAMESPACE structure exceeds
/// `max_depth`.
pub fn parse_untagged_with_max_depth(
    response: &Response,
    max_depth: usize,
) -> Result<Option<UntaggedData>, ProtocolError> {
    let Response::Untagged { data } = response else {
        return Ok(None);
    };
    let (first, remaining) = split_token_preserve_tail(data);
    if let Some(kind) = parse_status_kind(first) {
        return parse_status_response(data, kind).map(|status| Some(UntaggedData::Status(status)));
    }
    if first.iter().all(u8::is_ascii_digit) && !first.is_empty() {
        return parse_numeric_untagged(data, first, remaining, max_depth).map(Some);
    }
    if eq_ascii(first, b"CAPABILITY") {
        return CapabilitySet::parse_response(data).map(|set| Some(UntaggedData::Capability(set)));
    }
    if eq_ascii(first, b"ENABLED") {
        return CapabilitySet::parse_enabled_response(data)
            .map(|set| Some(UntaggedData::Enabled(set)));
    }
    if eq_ascii(first, b"STATUS") {
        let arguments = take_exact_named_arguments(data, first, "IMAP STATUS response")?;
        let status_data = slice_for(data, arguments);
        return MailboxStatus::parse(&status_data)
            .map(|status| Some(UntaggedData::MailboxStatus(status)));
    }
    if eq_ascii(first, b"ID") {
        let arguments = take_exact_named_arguments(data, first, "IMAP ID response")?;
        let id_data = slice_for(data, arguments);
        return IdParameters::parse_response(&id_data).map(|id| Some(UntaggedData::Id(id)));
    }
    if eq_ascii(first, b"NAMESPACE") {
        let arguments = take_exact_named_arguments(data, first, "IMAP NAMESPACE response")?;
        let namespace_data = slice_for(data, arguments);
        return NamespaceResponse::parse_with_limits(
            &namespace_data,
            max_depth,
            DEFAULT_NAMESPACE_MAX_ITEMS,
        )
        .map(|namespace| Some(UntaggedData::Namespace(namespace)));
    }
    if eq_ascii(first, b"LIST") {
        return ListResponse::parse_with_max_depth(data, max_depth)
            .map(|list| Some(UntaggedData::List(list)));
    }
    if eq_ascii(first, b"LSUB") {
        return ListResponse::parse_lsub_with_max_depth(data, max_depth)
            .map(|list| Some(UntaggedData::Lsub(list)));
    }
    if eq_ascii(first, b"FLAGS") {
        let flags = take_exact_named_arguments(data, first, "IMAP FLAGS response")?;
        validate_flag_list(flags, false)?;
        return Ok(Some(UntaggedData::Flags(slice_for(data, flags))));
    }
    if eq_ascii(first, b"VANISHED") {
        let arguments = take_exact_named_arguments(data, first, "IMAP VANISHED response")?;
        let (earlier, uids) = parse_vanished_arguments(arguments)?;
        return Ok(Some(UntaggedData::Vanished {
            earlier,
            uids: parse_uid_set(uids, "IMAP VANISHED UID set")?.0,
        }));
    }
    if eq_ascii(first, b"ESEARCH") {
        return ESearchResponse::parse(data).map(|result| Some(UntaggedData::ESearch(result)));
    }
    if eq_ascii(first, b"SEARCH") {
        return SearchResponse::parse(data).map(|result| Some(UntaggedData::Search(result)));
    }
    if eq_ascii(first, b"SORT") {
        return SortResponse::parse(data).map(|result| Some(UntaggedData::Sort(result)));
    }
    if eq_ascii(first, b"THREAD") {
        return ThreadResponse::parse_with_limits(data, max_depth, DEFAULT_THREAD_MAX_NODES)
            .map(|result| Some(UntaggedData::Thread(result)));
    }
    if eq_ascii(first, b"QUOTA") {
        return QuotaResponse::parse(data).map(|result| Some(UntaggedData::Quota(result)));
    }
    if eq_ascii(first, b"QUOTAROOT") {
        return QuotaRootResponse::parse(data).map(|result| Some(UntaggedData::QuotaRoot(result)));
    }
    validate_response_atom(first, "IMAP untagged response name")?;
    let other_data = if data.len() == first.len() {
        &data[data.len()..]
    } else {
        if data.get(first.len()) != Some(&b' ') {
            return Err(ProtocolError::new(
                ErrorKind::InvalidSyntax,
                "IMAP untagged response separator",
            ));
        }
        &data[first.len() + 1..]
    };
    Ok(Some(UntaggedData::Other {
        name: slice_for(data, first),
        data: slice_for(data, other_data),
    }))
}

fn parse_numeric_untagged(
    frame: &Bytes,
    number: &[u8],
    _: &[u8],
    max_depth: usize,
) -> Result<UntaggedData, ProtocolError> {
    let arguments = take_exact_named_arguments(frame, number, "IMAP numeric response separator")?;
    let (name, content) = split_exact_optional_argument(arguments, "IMAP numeric response data")?;
    if eq_ascii(name, b"EXISTS") {
        require_empty(content, "IMAP EXISTS response")?;
        return parse_number32(number, false, "IMAP EXISTS count").map(UntaggedData::Exists);
    }
    if eq_ascii(name, b"RECENT") {
        require_empty(content, "IMAP RECENT response")?;
        return parse_number32(number, false, "IMAP RECENT count").map(UntaggedData::Recent);
    }
    if eq_ascii(name, b"EXPUNGE") {
        require_empty(content, "IMAP EXPUNGE response")?;
        return parse_number32(number, true, "IMAP EXPUNGE sequence number")
            .map(UntaggedData::Expunge);
    }
    if eq_ascii(name, b"FETCH") {
        require_non_empty(content, "IMAP FETCH response")?;
        let sequence = parse_number32(number, true, "IMAP FETCH sequence number")?;
        let fetch_data = slice_for(frame, content);
        return Ok(UntaggedData::Fetch {
            sequence,
            data: FetchResponse::parse_with_max_depth(&fetch_data, max_depth)?,
        });
    }
    parse_number32(number, false, "IMAP numeric untagged response")?;
    validate_response_atom(name, "IMAP numeric untagged response name")?;
    Ok(UntaggedData::Other {
        name: slice_for(frame, name),
        data: slice_for(frame, content),
    })
}

fn split_exact_optional_argument<'a>(
    input: &'a [u8],
    context: &'static str,
) -> Result<(&'a [u8], &'a [u8]), ProtocolError> {
    let boundary = input
        .iter()
        .position(|byte| matches!(byte, b' ' | b'\t'))
        .unwrap_or(input.len());
    if boundary == input.len() {
        return Ok((input, &input[input.len()..]));
    }
    if input[boundary] != b' '
        || boundary == 0
        || input.get(boundary + 1).is_none()
        || matches!(input.get(boundary + 1), Some(b' ' | b'\t'))
    {
        return Err(ProtocolError::new(ErrorKind::InvalidSyntax, context));
    }
    Ok((&input[..boundary], &input[boundary + 1..]))
}

fn parse_number32(
    value: &[u8],
    non_zero: bool,
    context: &'static str,
) -> Result<u32, ProtocolError> {
    if non_zero && value.first() == Some(&b'0') {
        return Err(ProtocolError::new(ErrorKind::InvalidSyntax, context));
    }
    let number = parse_u64(value, context)?;
    if (non_zero && number == 0) || number > u64::from(u32::MAX) {
        return Err(ProtocolError::new(ErrorKind::InvalidSyntax, context));
    }
    u32::try_from(number).map_err(|_| ProtocolError::new(ErrorKind::InvalidSyntax, context))
}

fn parse_status_response(data: &Bytes, kind: StatusKind) -> Result<StatusResponse, ProtocolError> {
    let (name, _) = split_token_preserve_tail(data);
    let response_text = take_status_response_text(data, name)?;
    if response_text.first() != Some(&b'[') {
        return Ok(StatusResponse {
            kind,
            code: None,
            text: slice_for(data, response_text),
        });
    }
    let (code_data, text) = split_bracketed_response_text(response_text)?;
    Ok(StatusResponse {
        kind,
        code: Some(parse_response_code(data, code_data)?),
        text: slice_for(data, text),
    })
}

fn take_status_response_text<'a>(input: &'a [u8], name: &[u8]) -> Result<&'a [u8], ProtocolError> {
    let boundary = name.len();
    if input.get(boundary) != Some(&b' ') {
        return Err(ProtocolError::new(
            ErrorKind::InvalidSyntax,
            "IMAP status response text separator",
        ));
    }
    Ok(&input[boundary + 1..])
}

fn parse_status_kind(value: &[u8]) -> Option<StatusKind> {
    if eq_ascii(value, b"OK") {
        Some(StatusKind::Ok)
    } else if eq_ascii(value, b"NO") {
        Some(StatusKind::No)
    } else if eq_ascii(value, b"BAD") {
        Some(StatusKind::Bad)
    } else if eq_ascii(value, b"BYE") {
        Some(StatusKind::Bye)
    } else if eq_ascii(value, b"PREAUTH") {
        Some(StatusKind::Preauth)
    } else {
        None
    }
}

fn take_exact_named_arguments<'a>(
    input: &'a [u8],
    name: &[u8],
    context: &'static str,
) -> Result<&'a [u8], ProtocolError> {
    let boundary = name.len();
    if input.get(boundary) != Some(&b' ')
        || input.get(boundary + 1).is_none()
        || matches!(input.get(boundary + 1), Some(b' ' | b'\t'))
    {
        return Err(ProtocolError::new(ErrorKind::InvalidSyntax, context));
    }
    Ok(&input[boundary + 1..])
}

fn split_exact_required_argument<'a>(
    input: &'a [u8],
    context: &'static str,
) -> Result<(&'a [u8], &'a [u8]), ProtocolError> {
    let Some(boundary) = input.iter().position(|byte| matches!(byte, b' ' | b'\t')) else {
        return Err(ProtocolError::new(ErrorKind::InvalidSyntax, context));
    };
    if input[boundary] != b' '
        || boundary == 0
        || input.get(boundary + 1).is_none()
        || matches!(input.get(boundary + 1), Some(b' ' | b'\t'))
    {
        return Err(ProtocolError::new(ErrorKind::InvalidSyntax, context));
    }
    Ok((&input[..boundary], &input[boundary + 1..]))
}

fn parse_vanished_arguments(input: &[u8]) -> Result<(bool, &[u8]), ProtocolError> {
    const EARLIER: &[u8] = b"(EARLIER)";
    if input
        .get(..EARLIER.len())
        .is_some_and(|prefix| eq_ascii(prefix, EARLIER))
    {
        if input.get(EARLIER.len()) != Some(&b' ')
            || input.get(EARLIER.len() + 1).is_none()
            || matches!(input.get(EARLIER.len() + 1), Some(b' ' | b'\t'))
        {
            return Err(ProtocolError::new(
                ErrorKind::InvalidSyntax,
                "IMAP VANISHED EARLIER separator",
            ));
        }
        Ok((true, &input[EARLIER.len() + 1..]))
    } else {
        Ok((false, input))
    }
}

fn validate_flag_list(input: &[u8], allow_permanent_wildcard: bool) -> Result<(), ProtocolError> {
    if input.len() < 2 || input.first() != Some(&b'(') || input.last() != Some(&b')') {
        return Err(ProtocolError::new(
            ErrorKind::InvalidSyntax,
            "IMAP PERMANENTFLAGS flag list",
        ));
    }
    let inner = &input[1..input.len() - 1];
    if inner.is_empty() {
        return Ok(());
    }
    for flag in inner.split(|byte| *byte == b' ') {
        if flag.is_empty() || flag.contains(&b'\t') {
            return Err(ProtocolError::new(
                ErrorKind::InvalidSyntax,
                "IMAP PERMANENTFLAGS flag separator",
            ));
        }
        if flag == b"\\*" {
            if allow_permanent_wildcard {
                continue;
            }
            return Err(ProtocolError::new(
                ErrorKind::InvalidSyntax,
                "IMAP FLAGS wildcard",
            ));
        }
        let atom = flag.strip_prefix(b"\\").unwrap_or(flag);
        validate_response_atom(atom, "IMAP PERMANENTFLAGS flag")?;
    }
    Ok(())
}

fn validate_response_atom(value: &[u8], context: &'static str) -> Result<(), ProtocolError> {
    if value.is_empty()
        || value.iter().any(|byte| {
            !byte.is_ascii()
                || byte.is_ascii_control()
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

fn parse_uid_set(input: &[u8], context: &'static str) -> Result<(SequenceSet, u64), ProtocolError> {
    let set = SequenceSet::parse(input)?;
    if set.is_saved_search() {
        return Err(ProtocolError::new(ErrorKind::InvalidSyntax, context));
    }
    let mut intervals = Vec::with_capacity(set.ranges().len());
    for range in set.ranges() {
        let Sequence::Number(start) = range.start else {
            return Err(ProtocolError::new(ErrorKind::InvalidSyntax, context));
        };
        let end = if let Some(end) = range.end {
            let Sequence::Number(end) = end else {
                return Err(ProtocolError::new(ErrorKind::InvalidSyntax, context));
            };
            end
        } else {
            start
        };
        intervals.push((start.min(end), start.max(end)));
    }
    intervals.sort_unstable_by_key(|interval| interval.0);
    let mut cardinality = 0u64;
    let mut merged_end: Option<u64> = None;
    for (start, end) in intervals {
        if let Some(previous_end) = merged_end {
            if start <= previous_end.saturating_add(1) {
                if end > previous_end {
                    cardinality = cardinality
                        .checked_add(end - previous_end)
                        .ok_or_else(|| ProtocolError::new(ErrorKind::InvalidSyntax, context))?;
                    merged_end = Some(end);
                }
                continue;
            }
        }
        cardinality = cardinality
            .checked_add(end - start + 1)
            .ok_or_else(|| ProtocolError::new(ErrorKind::InvalidSyntax, context))?;
        merged_end = Some(end);
    }
    Ok((set, cardinality))
}

fn parse_u64(value: &[u8], context: &'static str) -> Result<u64, ProtocolError> {
    if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
        return Err(ProtocolError::new(ErrorKind::InvalidSyntax, context));
    }
    let number = value.iter().try_fold(0u64, |number, digit| {
        number
            .checked_mul(10)
            .and_then(|number| number.checked_add(u64::from(*digit - b'0')))
    });
    number.ok_or_else(|| ProtocolError::new(ErrorKind::InvalidSyntax, context))
}

fn require_empty(value: &[u8], context: &'static str) -> Result<(), ProtocolError> {
    if value.is_empty() {
        Ok(())
    } else {
        Err(ProtocolError::new(ErrorKind::InvalidSyntax, context))
    }
}

fn require_non_empty(value: &[u8], context: &'static str) -> Result<(), ProtocolError> {
    if value.is_empty() {
        Err(ProtocolError::new(ErrorKind::InvalidSyntax, context))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn untagged(data: &'static [u8]) -> Response {
        Response::Untagged {
            data: Bytes::from_static(data),
        }
    }

    #[test]
    fn parses_status_response_code() {
        let parsed = parse_untagged(&untagged(b"OK [UIDVALIDITY 3857529045] UIDs valid"))
            .unwrap()
            .unwrap();
        assert_eq!(
            parsed,
            UntaggedData::Status(StatusResponse {
                kind: StatusKind::Ok,
                code: Some(ResponseCode::UidValidity(3_857_529_045)),
                text: Bytes::from_static(b"UIDs valid"),
            })
        );

        assert!(matches!(
            parse_untagged(&untagged(b"NO [NOTSAVED] result limit"))
                .unwrap()
                .unwrap(),
            UntaggedData::Status(StatusResponse {
                code: Some(ResponseCode::NotSaved),
                ..
            })
        ));
        assert!(parse_untagged(&untagged(b"NO [NOTSAVED extra] bad")).is_err());

        let tagged = Response::Tagged {
            tag: Bytes::from_static(b"A1"),
            status: crate::Status::No,
            information: Bytes::from_static(b"[NOTSAVED] result limit"),
        };
        assert_eq!(
            tagged.parsed_response_code().unwrap(),
            Some(ResponseCode::NotSaved)
        );
        assert!(
            Response::Tagged {
                tag: Bytes::from_static(b"A1"),
                status: crate::Status::No,
                information: Bytes::from_static(b"[NOTSAVED extra] bad"),
            }
            .parsed_response_code()
            .is_err()
        );
    }

    #[test]
    fn parses_numeric_and_fetch_data() {
        assert_eq!(
            parse_untagged(&untagged(b"0 EXISTS")).unwrap(),
            Some(UntaggedData::Exists(0))
        );
        assert_eq!(
            parse_untagged(&untagged(b"18 EXISTS")).unwrap(),
            Some(UntaggedData::Exists(18))
        );
        assert_eq!(
            parse_untagged(&untagged(b"12 FETCH (FLAGS (\\Seen))")).unwrap(),
            Some(UntaggedData::Fetch {
                sequence: 12,
                data: FetchResponse::parse(&Bytes::from_static(b"(FLAGS (\\Seen))")).unwrap(),
            })
        );
        assert_eq!(
            parse_untagged(&untagged(b"4294967295 FETCH (UID 4294967295)")).unwrap(),
            Some(UntaggedData::Fetch {
                sequence: u32::MAX,
                data: FetchResponse::parse(&Bytes::from_static(b"(UID 4294967295)")).unwrap(),
            })
        );

        for invalid in [
            &b"0 FETCH (UID 1)"[..],
            b"01 FETCH (UID 1)",
            b"4294967296 FETCH (UID 1)",
            b"1\tFETCH (UID 1)",
            b"1  FETCH (UID 1)",
            b"1 FETCH\t(UID 1)",
            b"1 FETCH  (UID 1)",
            b"1 EXISTS ",
            b"0 EXPUNGE",
            b"01 EXPUNGE",
            b"4294967296 EXPUNGE",
            b"4294967296 EXISTS",
        ] {
            assert!(parse_untagged(&untagged(invalid)).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn applies_explicit_fetch_response_depth_budget() {
        let response = untagged(
            b"1 FETCH (BODYSTRUCTURE ((\"TEXT\" \"PLAIN\" NIL NIL NIL \"7BIT\" 1 1) \"MIXED\"))",
        );
        assert!(parse_untagged_with_max_depth(&response, 1).is_err());
        assert!(parse_untagged_with_max_depth(&response, 2).is_ok());
    }

    #[test]
    fn parses_capability_and_vanished() {
        let capability = parse_untagged(&untagged(b"CAPABILITY IMAP4rev2 IDLE"))
            .unwrap()
            .unwrap();
        assert!(matches!(capability, UntaggedData::Capability(_)));
        assert_eq!(
            parse_untagged(&untagged(b"VANISHED (EARLIER) 1:3,9")).unwrap(),
            Some(UntaggedData::Vanished {
                earlier: true,
                uids: SequenceSet::parse(b"1:3,9").unwrap(),
            })
        );
    }

    #[test]
    fn parses_enabled_legacy_search_and_lsub_as_typed_data() {
        let enabled = parse_untagged(&untagged(b"ENABLED CONDSTORE UTF8=ACCEPT"))
            .unwrap()
            .unwrap();
        assert!(matches!(
            enabled,
            UntaggedData::Enabled(capabilities)
                if capabilities.contains(&crate::Capability::CondStore)
                    && capabilities.contains(&crate::Capability::Utf8Accept)
        ));
        assert!(matches!(
            parse_untagged(&untagged(b"ENABLED")).unwrap(),
            Some(UntaggedData::Enabled(capabilities)) if capabilities.is_empty()
        ));

        let search = parse_untagged(&untagged(b"SEARCH 2 5 (MODSEQ 9)"))
            .unwrap()
            .unwrap();
        assert!(matches!(
            search,
            UntaggedData::Search(response)
                if response.results().collect::<Vec<_>>() == vec![2, 5]
                    && response.mod_sequence() == Some(9)
        ));

        let lsub = parse_untagged(&untagged(b"LSUB (\\Noselect) \"/\" Archive"))
            .unwrap()
            .unwrap();
        assert!(matches!(
            lsub,
            UntaggedData::Lsub(response)
                if response.mailbox().decoded().as_ref() == b"Archive"
        ));
    }

    #[test]
    fn parses_sort_thread_and_quota_as_typed_data() {
        assert!(matches!(
            parse_untagged(&untagged(b"SORT 2 5 (MODSEQ 9)")).unwrap(),
            Some(UntaggedData::Sort(response))
                if response.results().collect::<Vec<_>>() == vec![2, 5]
                    && response.mod_sequence() == Some(9)
        ));
        assert!(matches!(
            parse_untagged(&untagged(b"THREAD (2)(3 6 (4)(5))")).unwrap(),
            Some(UntaggedData::Thread(response))
                if response.message_count() == 5 && response.maximum_depth() == 2
        ));
        assert!(matches!(
            parse_untagged(&untagged(b"QUOTA \"\" (STORAGE 12 100 MESSAGE 3 20)"))
                .unwrap(),
            Some(UntaggedData::Quota(response)) if response.resource_count() == 2
        ));
        assert!(matches!(
            parse_untagged(&untagged(b"QUOTAROOT INBOX \"\" archive")).unwrap(),
            Some(UntaggedData::QuotaRoot(response)) if response.root_count() == 2
        ));

        for invalid in [
            b"SORT 0".as_slice(),
            b"THREAD ((1))",
            b"QUOTA \"\" (STORAGE 1)",
            b"QUOTAROOT INBOX ",
        ] {
            assert!(parse_untagged(&untagged(invalid)).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn parses_namespace_as_bounded_typed_data() {
        let parsed = parse_untagged(&untagged(b"NAMESPACE ((\"\" \"/\")) ((\"~\" \"/\")) NIL"))
            .unwrap()
            .unwrap();
        let UntaggedData::Namespace(namespace) = parsed else {
            panic!("typed NAMESPACE expected");
        };
        assert_eq!(namespace.personal().len(), 1);
        assert_eq!(namespace.other_users().len(), 1);
        assert!(namespace.shared().is_nil());
        assert!(
            parse_untagged_with_max_depth(
                &untagged(b"NAMESPACE ((\"\" \"/\" \"X\" (\"v\"))) NIL NIL"),
                2,
            )
            .is_err()
        );
    }

    #[test]
    fn parses_id_response_with_privacy_preserving_nil() {
        assert!(matches!(
            parse_untagged(&untagged(b"ID NIL")).unwrap(),
            Some(UntaggedData::Id(parameters)) if parameters.is_nil()
        ));
        let parsed = parse_untagged(&untagged(
            b"ID (\"name\" \"mail-protocol\" \"version\" NIL)",
        ))
        .unwrap()
        .unwrap();
        let UntaggedData::Id(parameters) = parsed else {
            panic!("typed ID expected");
        };
        assert_eq!(
            parameters.get(b"NAME").unwrap().decoded().unwrap().as_ref(),
            b"mail-protocol"
        );
        assert!(parameters.get(b"version").unwrap().is_nil());
        assert!(parse_untagged(&untagged(b"ID (\"name\" \"a\" \"NAME\" \"b\")")).is_err());
    }

    #[test]
    fn parses_every_rfc9051_no_argument_response_code_strictly() {
        let cases = [
            (b"ALREADYEXISTS".as_slice(), ResponseCode::AlreadyExists),
            (b"AUTHENTICATIONFAILED", ResponseCode::AuthenticationFailed),
            (b"AUTHORIZATIONFAILED", ResponseCode::AuthorizationFailed),
            (b"CANNOT", ResponseCode::Cannot),
            (b"CLIENTBUG", ResponseCode::ClientBug),
            (b"CLOSED", ResponseCode::Closed),
            (b"CONTACTADMIN", ResponseCode::ContactAdmin),
            (b"CORRUPTION", ResponseCode::Corruption),
            (b"EXPIRED", ResponseCode::Expired),
            (b"EXPUNGEISSUED", ResponseCode::ExpungeIssued),
            (b"HASCHILDREN", ResponseCode::HasChildren),
            (b"INUSE", ResponseCode::InUse),
            (b"LIMIT", ResponseCode::Limit),
            (b"NONEXISTENT", ResponseCode::NonExistent),
            (b"NOPERM", ResponseCode::NoPerm),
            (b"OVERQUOTA", ResponseCode::OverQuota),
            (b"PRIVACYREQUIRED", ResponseCode::PrivacyRequired),
            (b"SERVERBUG", ResponseCode::ServerBug),
            (b"UIDNOTSTICKY", ResponseCode::UidNotSticky),
            (b"UNAVAILABLE", ResponseCode::Unavailable),
            (b"UNKNOWN-CTE", ResponseCode::UnknownCte),
        ];

        for (name, expected) in cases {
            let mut information = Vec::with_capacity(name.len() + 3);
            information.push(b'[');
            information.extend_from_slice(name);
            information.push(b']');
            information.push(b' ');
            let response = Response::Tagged {
                tag: Bytes::from_static(b"A1"),
                status: crate::Status::No,
                information: Bytes::from(information),
            };
            assert_eq!(response.parsed_response_code().unwrap(), Some(expected));

            let mut invalid_information = Vec::with_capacity(name.len() + 8);
            invalid_information.push(b'[');
            invalid_information.extend_from_slice(name);
            invalid_information.extend_from_slice(b" extra]");
            let invalid_response = Response::Tagged {
                tag: Bytes::from_static(b"A1"),
                status: crate::Status::No,
                information: Bytes::from(invalid_information),
            };
            assert!(invalid_response.parsed_response_code().is_err());
        }
    }

    #[test]
    fn parses_typed_esearch_data() {
        let parsed = parse_untagged(&untagged(
            b"ESEARCH (TAG \"A1\") UID COUNT 17 ALL 4:18,21,28",
        ))
        .unwrap()
        .unwrap();
        let UntaggedData::ESearch(result) = parsed else {
            panic!("typed ESEARCH expected");
        };
        assert_eq!(result.tag().unwrap().decoded().as_ref(), b"A1");
        assert!(result.is_uid());
        assert_eq!(result.count(), Some(17));
        assert_eq!(result.all().unwrap().as_bytes(), b"4:18,21,28");
    }

    #[test]
    fn parses_mailbox_status_data() {
        let parsed = parse_untagged(&untagged(
            b"STATUS INBOX (MESSAGES 231 UIDNEXT 44292 SIZE 123456)",
        ))
        .unwrap()
        .unwrap();
        let UntaggedData::MailboxStatus(status) = parsed else {
            panic!("typed mailbox STATUS expected");
        };
        assert_eq!(status.mailbox().decoded(), b"INBOX".as_slice());
        assert_eq!(
            status.values().iter().collect::<Vec<_>>(),
            vec![
                crate::StatusValue::Messages(231),
                crate::StatusValue::UidNext(44_292),
                crate::StatusValue::Size(123_456),
            ]
        );

        assert!(parse_untagged(&untagged(b"STATUS INBOX (UIDNEXT 0)")).is_err());
    }

    #[test]
    fn preserves_unknown_response_code() {
        let parsed = parse_untagged(&untagged(b"NO [X-VENDOR reason] failed"))
            .unwrap()
            .unwrap();
        assert!(matches!(
            parsed,
            UntaggedData::Status(StatusResponse {
                code: Some(ResponseCode::Other { name, data }),
                ..
            }) if name.as_ref() == b"X-VENDOR" && data.as_ref() == b"reason"
        ));
    }

    #[test]
    fn named_untagged_data_requires_one_space() {
        for valid in [
            b"ID NIL".as_slice(),
            b"NAMESPACE NIL NIL NIL",
            b"STATUS INBOX (MESSAGES 1)",
            b"FLAGS (\\Seen)",
            b"VANISHED 1:3",
        ] {
            assert!(parse_untagged(&untagged(valid)).is_ok(), "{valid:?}");
        }

        for invalid in [
            b"ID".as_slice(),
            b"ID\tNIL",
            b"ID  NIL",
            b"NAMESPACE\tNIL NIL NIL",
            b"NAMESPACE  NIL NIL NIL",
            b"STATUS\tINBOX (MESSAGES 1)",
            b"STATUS  INBOX (MESSAGES 1)",
            b"FLAGS\t(\\Seen)",
            b"FLAGS  (\\Seen)",
            b"FLAGS nope",
            b"FLAGS (\\Seen  custom)",
            b"FLAGS (\\Seen \\*)",
            b"VANISHED\t1:3",
            b"VANISHED  1:3",
            b"VANISHED *",
            b"VANISHED $",
            b"] garbage",
            b"( bad",
            b"X-EXT\tdata",
            b"1 ]",
        ] {
            assert!(parse_untagged(&untagged(invalid)).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn bracketed_code_requires_resp_text_space_and_exact_code_separator() {
        let empty_text = Response::Tagged {
            tag: Bytes::from_static(b"A1"),
            status: crate::Status::Ok,
            information: Bytes::from_static(b"[ALERT] "),
        };
        assert_eq!(
            empty_text.parsed_response_code().unwrap(),
            Some(ResponseCode::Alert)
        );

        let opaque = parse_untagged(&untagged(b"NO [X-VENDOR reason\twith  gaps ] failed"))
            .unwrap()
            .unwrap();
        assert!(matches!(
            opaque,
            UntaggedData::Status(StatusResponse {
                code: Some(ResponseCode::Other { name, data }),
                ..
            }) if name.as_ref() == b"X-VENDOR" && data.as_ref() == b"reason\twith  gaps "
        ));

        for invalid in [
            b"[ALERT]".as_slice(),
            b"[ALERT]\ttext",
            b"[UIDNEXT\t1] text",
            b"[UIDNEXT  1] text",
            b"[UIDNEXT ] text",
            b"[X%Y data] text",
        ] {
            let response = Response::Tagged {
                tag: Bytes::from_static(b"A1"),
                status: crate::Status::No,
                information: Bytes::copy_from_slice(invalid),
            };
            assert!(response.parsed_response_code().is_err(), "{invalid:?}");
        }
        let leading_space_text = parse_untagged(&untagged(b"OK  [ALERT] text"))
            .unwrap()
            .unwrap();
        assert!(matches!(
            leading_space_text,
            UntaggedData::Status(StatusResponse { code: None, text, .. })
                if text.as_ref() == b" [ALERT] text"
        ));

        for invalid in [b"OK".as_slice(), b"OK\t[ALERT] text"] {
            assert!(parse_untagged(&untagged(invalid)).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn validates_badcharset_and_permanentflags_grammar() {
        for valid in [
            b"OK [BADCHARSET] unsupported".as_slice(),
            b"OK [BADCHARSET (US-ASCII \"UTF-8\")] unsupported",
            b"OK [PERMANENTFLAGS ()] flags",
            b"OK [PERMANENTFLAGS (\\Seen custom \\*)] flags",
        ] {
            assert!(parse_untagged(&untagged(valid)).is_ok(), "{valid:?}");
        }
        for invalid in [
            b"OK [BADCHARSET US-ASCII] bad".as_slice(),
            b"OK [BADCHARSET ()] bad",
            b"OK [BADCHARSET (US-ASCII  UTF-8)] bad",
            b"OK [BADCHARSET (\"unterminated)] bad",
            b"OK [PERMANENTFLAGS flags] bad",
            b"OK [PERMANENTFLAGS (\\Seen  custom)] bad",
            b"OK [PERMANENTFLAGS (\\)] bad",
        ] {
            assert!(parse_untagged(&untagged(invalid)).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn response_code_numbers_follow_protocol_bounds() {
        assert!(matches!(
            parse_untagged(&untagged(b"OK [UIDNEXT 4294967295] next")).unwrap(),
            Some(UntaggedData::Status(StatusResponse {
                code: Some(ResponseCode::UidNext(value)),
                ..
            })) if value == u64::from(u32::MAX)
        ));
        assert!(matches!(
            parse_untagged(&untagged(b"OK [HIGHESTMODSEQ 0001] modseq")).unwrap(),
            Some(UntaggedData::Status(StatusResponse {
                code: Some(ResponseCode::HighestModSeq(1)),
                ..
            }))
        ));

        for invalid in [
            b"OK [UIDNEXT 4294967296] bad".as_slice(),
            b"OK [UIDVALIDITY 4294967296] bad",
            b"OK [UNSEEN 4294967296] bad",
            b"OK [UIDNEXT 01] bad",
            b"OK [HIGHESTMODSEQ 0] bad",
            b"OK [HIGHESTMODSEQ 9223372036854775808] bad",
        ] {
            assert!(parse_untagged(&untagged(invalid)).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn appenduid_and_copyuid_require_numeric_equal_cardinality_uid_sets() {
        for valid in [
            b"OK [APPENDUID 4294967295 9:7] appended".as_slice(),
            b"OK [APPENDUID 7 1:2,2] appended",
            b"OK [COPYUID 7 1:3,9 10:12,20] copied",
            b"OK [COPYUID 7 1:3,2 10:12] copied",
            b"OK [COPYUID 7 1,1 2,2] copied",
            b"OK [COPYUID 7 1:1 2:2] copied",
        ] {
            assert!(parse_untagged(&untagged(valid)).is_ok(), "{valid:?}");
        }
        for invalid in [
            b"OK [APPENDUID 4294967296 1] bad".as_slice(),
            b"OK [APPENDUID 7 *] bad",
            b"OK [APPENDUID 7 $] bad",
            b"OK [APPENDUID 7 9:9] bad",
            b"OK [APPENDUID 7 1,1] bad",
            b"OK [COPYUID 4294967296 1 2] bad",
            b"OK [COPYUID 7 * 2] bad",
            b"OK [COPYUID 7 1 $] bad",
            b"OK [COPYUID 7 1:3 4:5] bad",
            b"OK [COPYUID 7 1,1 2,3] bad",
        ] {
            assert!(parse_untagged(&untagged(invalid)).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn vanished_earlier_is_case_insensitive_with_exact_separators() {
        assert!(matches!(
            parse_untagged(&untagged(b"VANISHED (earlier) 1:3")).unwrap(),
            Some(UntaggedData::Vanished { earlier: true, .. })
        ));
        for invalid in [
            b"VANISHED (EARLIER)".as_slice(),
            b"VANISHED (EARLIER)1:3",
            b"VANISHED (EARLIER)\t1:3",
            b"VANISHED (EARLIER)  1:3",
        ] {
            assert!(parse_untagged(&untagged(invalid)).is_err(), "{invalid:?}");
        }
    }
}
