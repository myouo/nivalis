use std::ops::Range;

use bytes::{Bytes, BytesMut};
use mail_protocol_core::{ErrorKind, ProtocolError};

use crate::astring::parse_astring_prefix;
use crate::{AString, AStringKind, Command, CommandBody};

/// Maximum resource entries accepted in one SETQUOTA or QUOTA list.
pub const MAX_QUOTA_RESOURCES: usize = 256;

/// Maximum quota roots accepted in one QUOTAROOT response.
pub const MAX_QUOTA_ROOTS: usize = 256;

/// One RFC 9208 quota resource name.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum QuotaResourceName<'a> {
    /// Storage measured in 1,024-octet units.
    Storage,
    /// Message count.
    Message,
    /// Mailbox count.
    Mailbox,
    /// Annotation storage measured in 1,024-octet units.
    AnnotationStorage,
    /// An extension resource name.
    Other(&'a [u8]),
}

impl<'a> QuotaResourceName<'a> {
    /// Returns the resource name.
    pub const fn name(self) -> &'a [u8] {
        match self {
            Self::Storage => b"STORAGE",
            Self::Message => b"MESSAGE",
            Self::Mailbox => b"MAILBOX",
            Self::AnnotationStorage => b"ANNOTATION-STORAGE",
            Self::Other(name) => name,
        }
    }
}

/// One resource limit in SETQUOTA arguments.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct QuotaLimit<'a> {
    name: QuotaResourceName<'a>,
    limit: u64,
}

impl<'a> QuotaLimit<'a> {
    /// Returns the typed quota resource name.
    pub const fn name(self) -> QuotaResourceName<'a> {
        self.name
    }

    /// Returns the RFC 9208 `number64` limit.
    pub const fn limit(self) -> u64 {
        self.limit
    }
}

/// One resource usage/limit pair in a QUOTA response.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct QuotaResource<'a> {
    name: QuotaResourceName<'a>,
    usage: u64,
    limit: u64,
}

impl<'a> QuotaResource<'a> {
    /// Returns the typed quota resource name.
    pub const fn name(self) -> QuotaResourceName<'a> {
        self.name
    }

    /// Returns current resource usage.
    pub const fn usage(self) -> u64 {
        self.usage
    }

    /// Returns the advertised resource limit.
    pub const fn limit(self) -> u64 {
        self.limit
    }
}

/// Allocation-free iterator over SETQUOTA resource limits.
#[derive(Clone, Debug)]
pub struct QuotaLimitIter<'a> {
    remaining: &'a [u8],
}

impl<'a> Iterator for QuotaLimitIter<'a> {
    type Item = QuotaLimit<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining.is_empty() {
            return None;
        }
        let parsed = match parse_limit(self.remaining, 0) {
            Ok(parsed) => parsed,
            Err(error) => {
                debug_assert!(false, "validated SETQUOTA limit became invalid: {error}");
                self.remaining = b"";
                return None;
            }
        };
        self.remaining = next_item(self.remaining, parsed.end)?;
        Some(parsed.value)
    }
}

/// Allocation-free iterator over QUOTA response resources.
#[derive(Clone, Debug)]
pub struct QuotaResourceIter<'a> {
    remaining: &'a [u8],
}

impl<'a> Iterator for QuotaResourceIter<'a> {
    type Item = QuotaResource<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining.is_empty() {
            return None;
        }
        let parsed = match parse_resource(self.remaining, 0) {
            Ok(parsed) => parsed,
            Err(error) => {
                debug_assert!(false, "validated QUOTA resource became invalid: {error}");
                self.remaining = b"";
                return None;
            }
        };
        self.remaining = next_item(self.remaining, parsed.end)?;
        Some(parsed.value)
    }
}

/// Validated GETQUOTA command arguments.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GetQuotaArguments {
    wire: Bytes,
    root: AString,
}

impl GetQuotaArguments {
    /// Parses exactly one quota-root `astring`.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid root or trailing data.
    pub fn parse(wire: &Bytes) -> Result<Self, ProtocolError> {
        validate_get_quota_arguments(wire)?;
        let root = AString::parse(wire)?;
        Ok(Self {
            wire: wire.clone(),
            root,
        })
    }

    /// Returns the exact bytes following the GETQUOTA command name.
    pub const fn as_bytes(&self) -> &Bytes {
        &self.wire
    }

    /// Returns the quota root.
    pub const fn root(&self) -> &AString {
        &self.root
    }

    /// Appends the validated arguments exactly as received.
    pub fn encode(&self, destination: &mut BytesMut) {
        destination.extend_from_slice(&self.wire);
    }
}

/// Validated GETQUOTAROOT command arguments.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GetQuotaRootArguments {
    wire: Bytes,
    mailbox: AString,
}

impl GetQuotaRootArguments {
    /// Parses exactly one mailbox `astring`.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid mailbox or trailing data.
    pub fn parse(wire: &Bytes) -> Result<Self, ProtocolError> {
        validate_get_quota_root_arguments(wire)?;
        let mailbox = AString::parse(wire)?;
        Ok(Self {
            wire: wire.clone(),
            mailbox,
        })
    }

    /// Returns the exact bytes following the GETQUOTAROOT command name.
    pub const fn as_bytes(&self) -> &Bytes {
        &self.wire
    }

    /// Returns the mailbox whose applicable roots are requested.
    pub const fn mailbox(&self) -> &AString {
        &self.mailbox
    }

    /// Appends the validated arguments exactly as received.
    pub fn encode(&self, destination: &mut BytesMut) {
        destination.extend_from_slice(&self.wire);
    }
}

/// Validated, zero-copy SETQUOTA command arguments.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetQuotaArguments {
    wire: Bytes,
    root: AString,
    limits: Range<usize>,
    limit_count: usize,
}

impl SetQuotaArguments {
    /// Parses a quota root and RFC 9208 resource-limit list.
    ///
    /// Empty lists remove all limits. Resource names are unique within the
    /// list and values are restricted to the signed 63-bit `number64` range.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed syntax, duplicate/invalid resources,
    /// numeric overflow, or more than [`MAX_QUOTA_RESOURCES`] entries.
    pub fn parse(wire: &Bytes) -> Result<Self, ProtocolError> {
        let parsed = validate_set_quota_arguments(wire)?;
        let root = AString::parse(&wire.slice(..parsed.root_end))?;
        Ok(Self {
            wire: wire.clone(),
            root,
            limits: parsed.limits,
            limit_count: parsed.limit_count,
        })
    }

    /// Returns the exact bytes following the SETQUOTA command name.
    pub const fn as_bytes(&self) -> &Bytes {
        &self.wire
    }

    /// Returns the quota root.
    pub const fn root(&self) -> &AString {
        &self.root
    }

    /// Returns the complete parenthesized resource-limit list.
    pub fn limits_bytes(&self) -> &[u8] {
        &self.wire[self.limits.clone()]
    }

    /// Iterates over resource limits without allocating.
    pub fn limits(&self) -> QuotaLimitIter<'_> {
        QuotaLimitIter {
            remaining: list_interior(&self.wire, &self.limits),
        }
    }

    /// Returns the number of resource limits.
    pub const fn limit_count(&self) -> usize {
        self.limit_count
    }

    /// Appends the validated arguments exactly as received.
    pub fn encode(&self, destination: &mut BytesMut) {
        destination.extend_from_slice(&self.wire);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ParsedSetQuotaArguments {
    pub(crate) root_end: usize,
    pub(crate) limits: Range<usize>,
    pub(crate) limit_count: usize,
}

pub(crate) fn validate_get_quota_arguments(input: &[u8]) -> Result<(), ProtocolError> {
    validate_exact_astring(input, "IMAP GETQUOTA root")
}

pub(crate) fn validate_get_quota_root_arguments(input: &[u8]) -> Result<(), ProtocolError> {
    validate_exact_astring(input, "IMAP GETQUOTAROOT mailbox")
}

pub(crate) fn validate_set_quota_arguments(
    input: &[u8],
) -> Result<ParsedSetQuotaArguments, ProtocolError> {
    let root_end = validate_astring_at(input, 0, false)?.end;
    let list_start = require_space(input, root_end, "IMAP SETQUOTA list separator")?;
    let parsed = validate_limit_list(input, list_start)?;
    if parsed.end != input.len() {
        return Err(invalid("trailing IMAP SETQUOTA argument data").at(parsed.end));
    }
    Ok(ParsedSetQuotaArguments {
        root_end,
        limits: list_start..parsed.end,
        limit_count: parsed.count,
    })
}

impl Command {
    /// Returns typed GETQUOTA arguments when applicable.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed manually constructed raw arguments.
    pub fn parsed_get_quota_arguments(&self) -> Result<Option<GetQuotaArguments>, ProtocolError> {
        match &self.body {
            CommandBody::GetQuota { arguments } => Ok(Some(arguments.clone())),
            CommandBody::Raw { name, arguments } if name.eq_ignore_ascii_case(b"GETQUOTA") => {
                GetQuotaArguments::parse(arguments).map(Some)
            }
            _ => Ok(None),
        }
    }

    /// Returns typed GETQUOTAROOT arguments when applicable.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed manually constructed raw arguments.
    pub fn parsed_get_quota_root_arguments(
        &self,
    ) -> Result<Option<GetQuotaRootArguments>, ProtocolError> {
        match &self.body {
            CommandBody::GetQuotaRoot { arguments } => Ok(Some(arguments.clone())),
            CommandBody::Raw { name, arguments } if name.eq_ignore_ascii_case(b"GETQUOTAROOT") => {
                GetQuotaRootArguments::parse(arguments).map(Some)
            }
            _ => Ok(None),
        }
    }

    /// Returns typed SETQUOTA arguments when applicable.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed manually constructed raw arguments.
    pub fn parsed_set_quota_arguments(&self) -> Result<Option<SetQuotaArguments>, ProtocolError> {
        match &self.body {
            CommandBody::SetQuota { arguments } => Ok(Some(arguments.clone())),
            CommandBody::Raw { name, arguments } if name.eq_ignore_ascii_case(b"SETQUOTA") => {
                SetQuotaArguments::parse(arguments).map(Some)
            }
            _ => Ok(None),
        }
    }
}

/// Validated, zero-copy RFC 9208 QUOTA response data.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuotaResponse {
    wire: Bytes,
    root: AString,
    resources: Range<usize>,
    resource_count: usize,
}

impl QuotaResponse {
    /// Parses complete untagged response data beginning with `QUOTA`.
    ///
    /// Usage greater than a limit is intentionally accepted: enforcement and
    /// soft-limit policy belong to the server, not the protocol parser.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed syntax, non-synchronizing server
    /// literals, duplicate/invalid resources, `number64` overflow, or excessive
    /// resource count.
    pub fn parse(wire: &Bytes) -> Result<Self, ProtocolError> {
        const NAME: &[u8] = b"QUOTA";
        validate_name(wire, NAME, "IMAP QUOTA response name")?;
        let root_start = require_space(wire, NAME.len(), "IMAP QUOTA root separator")?;
        let (root, root_end) = parse_astring_at(wire, root_start, true)?;
        let list_start = require_space(wire, root_end, "IMAP QUOTA list separator")?;
        let parsed = validate_resource_list(wire, list_start)?;
        if parsed.end != wire.len() {
            return Err(invalid("trailing IMAP QUOTA response data").at(parsed.end));
        }
        Ok(Self {
            wire: wire.clone(),
            root,
            resources: list_start..parsed.end,
            resource_count: parsed.count,
        })
    }

    /// Returns complete response data beginning with `QUOTA`.
    pub const fn as_bytes(&self) -> &Bytes {
        &self.wire
    }

    /// Returns the quota root.
    pub const fn root(&self) -> &AString {
        &self.root
    }

    /// Returns the complete parenthesized resource list.
    pub fn resources_bytes(&self) -> &[u8] {
        &self.wire[self.resources.clone()]
    }

    /// Iterates over resource usage/limit values without allocating.
    pub fn resources(&self) -> QuotaResourceIter<'_> {
        QuotaResourceIter {
            remaining: list_interior(&self.wire, &self.resources),
        }
    }

    /// Returns the number of resources.
    pub const fn resource_count(&self) -> usize {
        self.resource_count
    }

    /// Appends the validated response data exactly as received.
    pub fn encode(&self, destination: &mut BytesMut) {
        destination.extend_from_slice(&self.wire);
    }
}

/// Allocation-free iterator over quota roots in a QUOTAROOT response.
///
/// Each yielded [`AString`] is a cheap [`Bytes`] slice sharing the response's
/// backing allocation.
#[derive(Clone, Debug)]
pub struct QuotaRootIter {
    remaining: Bytes,
}

impl Iterator for QuotaRootIter {
    type Item = AString;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining.is_empty() {
            return None;
        }
        let parsed = match parse_astring_prefix(&self.remaining) {
            Ok(parsed) => parsed,
            Err(error) => {
                debug_assert!(false, "validated QUOTAROOT root became invalid: {error}");
                self.remaining = Bytes::new();
                return None;
            }
        };
        let value = AString::parse(&self.remaining.slice(..parsed.end)).ok()?;
        self.remaining = if parsed.end == self.remaining.len() {
            Bytes::new()
        } else {
            self.remaining.slice(parsed.end + 1..)
        };
        Some(value)
    }
}

/// Validated, zero-copy RFC 9208 QUOTAROOT response data.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuotaRootResponse {
    wire: Bytes,
    mailbox: AString,
    roots: Option<Range<usize>>,
    root_count: usize,
}

impl QuotaRootResponse {
    /// Parses complete untagged response data beginning with `QUOTAROOT`.
    ///
    /// A mailbox with no applicable roots is valid.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed `astring` values, non-synchronizing
    /// server literals, malformed separators, or more than
    /// [`MAX_QUOTA_ROOTS`] roots.
    pub fn parse(wire: &Bytes) -> Result<Self, ProtocolError> {
        const NAME: &[u8] = b"QUOTAROOT";
        validate_name(wire, NAME, "IMAP QUOTAROOT response name")?;
        let mailbox_start = require_space(wire, NAME.len(), "IMAP QUOTAROOT mailbox separator")?;
        let (mailbox, mut cursor) = parse_astring_at(wire, mailbox_start, true)?;
        let roots_start = if cursor == wire.len() {
            None
        } else {
            cursor = require_space(wire, cursor, "IMAP QUOTAROOT root separator")?;
            if cursor == wire.len() {
                return Err(invalid("trailing IMAP QUOTAROOT separator").at(cursor));
            }
            Some(cursor)
        };

        let mut root_count = 0usize;
        while cursor < wire.len() {
            if root_count == MAX_QUOTA_ROOTS {
                return Err(invalid("too many IMAP QUOTAROOT roots").at(cursor));
            }
            let (_, end) = parse_astring_at(wire, cursor, true)?;
            root_count += 1;
            cursor = end;
            if cursor == wire.len() {
                break;
            }
            cursor = require_space(wire, cursor, "IMAP QUOTAROOT root separator")?;
            if cursor == wire.len() {
                return Err(invalid("trailing IMAP QUOTAROOT separator").at(cursor));
            }
        }

        Ok(Self {
            wire: wire.clone(),
            mailbox,
            roots: roots_start.map(|start| start..cursor),
            root_count,
        })
    }

    /// Returns complete response data beginning with `QUOTAROOT`.
    pub const fn as_bytes(&self) -> &Bytes {
        &self.wire
    }

    /// Returns the mailbox whose roots are listed.
    pub const fn mailbox(&self) -> &AString {
        &self.mailbox
    }

    /// Returns the exact space-separated quota roots.
    pub fn roots_bytes(&self) -> Option<&[u8]> {
        self.roots.as_ref().map(|range| &self.wire[range.clone()])
    }

    /// Iterates over quota-root `astring` values without copying payloads.
    pub fn roots(&self) -> QuotaRootIter {
        QuotaRootIter {
            remaining: self
                .roots
                .as_ref()
                .map_or_else(Bytes::new, |range| self.wire.slice(range.clone())),
        }
    }

    /// Returns the number of applicable roots.
    pub const fn root_count(&self) -> usize {
        self.root_count
    }

    /// Appends the validated response data exactly as received.
    pub fn encode(&self, destination: &mut BytesMut) {
        destination.extend_from_slice(&self.wire);
    }
}

struct ParsedList {
    end: usize,
    count: usize,
}

struct ParsedLimit<'a> {
    value: QuotaLimit<'a>,
    name: Range<usize>,
    end: usize,
}

struct ParsedResource<'a> {
    value: QuotaResource<'a>,
    name: Range<usize>,
    end: usize,
}

fn validate_limit_list(input: &[u8], start: usize) -> Result<ParsedList, ProtocolError> {
    validate_list(input, start, |input, cursor| {
        let parsed = parse_limit(input, cursor)?;
        Ok((parsed.name, parsed.end))
    })
}

fn validate_resource_list(input: &[u8], start: usize) -> Result<ParsedList, ProtocolError> {
    validate_list(input, start, |input, cursor| {
        let parsed = parse_resource(input, cursor)?;
        Ok((parsed.name, parsed.end))
    })
}

fn validate_list<F>(
    input: &[u8],
    start: usize,
    mut parse_item: F,
) -> Result<ParsedList, ProtocolError>
where
    F: FnMut(&[u8], usize) -> Result<(Range<usize>, usize), ProtocolError>,
{
    if input.get(start) != Some(&b'(') {
        return Err(invalid("IMAP QUOTA resource list").at(start));
    }
    let mut cursor = start + 1;
    if input.get(cursor) == Some(&b')') {
        return Ok(ParsedList {
            end: cursor + 1,
            count: 0,
        });
    }

    let mut names = [(0usize, 0usize); MAX_QUOTA_RESOURCES];
    let mut count = 0usize;
    loop {
        if count == MAX_QUOTA_RESOURCES {
            return Err(invalid("too many IMAP QUOTA resources").at(cursor));
        }
        let (name, end) = parse_item(input, cursor)?;
        if names[..count].iter().any(|&(known_start, known_end)| {
            input[known_start..known_end].eq_ignore_ascii_case(&input[name.clone()])
        }) {
            return Err(invalid("duplicate IMAP QUOTA resource").at(name.start));
        }
        names[count] = (name.start, name.end);
        count += 1;
        cursor = end;
        match input.get(cursor) {
            Some(b')') => {
                cursor += 1;
                break;
            }
            Some(b' ') => cursor += 1,
            _ => return Err(invalid("IMAP QUOTA resource separator").at(cursor)),
        }
        if input.get(cursor) == Some(&b')') {
            return Err(invalid("trailing IMAP QUOTA resource separator").at(cursor));
        }
    }
    Ok(ParsedList { end: cursor, count })
}

fn parse_limit(input: &[u8], start: usize) -> Result<ParsedLimit<'_>, ProtocolError> {
    let name_end = required_token_end(input, start, "IMAP SETQUOTA resource name")?;
    validate_atom(&input[start..name_end], "IMAP SETQUOTA resource name")?;
    let limit_start = require_space(input, name_end, "IMAP SETQUOTA limit separator")?;
    let limit_end = value_token_end(input, limit_start);
    let limit = parse_number64(&input[limit_start..limit_end], limit_start)?;
    Ok(ParsedLimit {
        value: QuotaLimit {
            name: classify_resource(&input[start..name_end]),
            limit,
        },
        name: start..name_end,
        end: limit_end,
    })
}

fn parse_resource(input: &[u8], start: usize) -> Result<ParsedResource<'_>, ProtocolError> {
    let name_end = required_token_end(input, start, "IMAP QUOTA resource name")?;
    validate_atom(&input[start..name_end], "IMAP QUOTA resource name")?;
    let usage_start = require_space(input, name_end, "IMAP QUOTA usage separator")?;
    let usage_end = value_token_end(input, usage_start);
    let usage = parse_number64(&input[usage_start..usage_end], usage_start)?;
    let limit_start = require_space(input, usage_end, "IMAP QUOTA limit separator")?;
    let limit_end = value_token_end(input, limit_start);
    let limit = parse_number64(&input[limit_start..limit_end], limit_start)?;
    Ok(ParsedResource {
        value: QuotaResource {
            name: classify_resource(&input[start..name_end]),
            usage,
            limit,
        },
        name: start..name_end,
        end: limit_end,
    })
}

fn parse_astring_at(
    input: &Bytes,
    start: usize,
    reject_non_synchronizing: bool,
) -> Result<(AString, usize), ProtocolError> {
    let parsed = validate_astring_at(input, start, reject_non_synchronizing)?;
    let end = parsed.end;
    Ok((AString::parse(&input.slice(start..end))?, end))
}

struct ParsedQuotaAString {
    end: usize,
}

fn validate_astring_at(
    input: &[u8],
    start: usize,
    reject_non_synchronizing: bool,
) -> Result<ParsedQuotaAString, ProtocolError> {
    let parsed =
        parse_astring_prefix(&input[start..]).map_err(|error| shift_error(error, start))?;
    if reject_non_synchronizing
        && matches!(
            parsed.kind,
            AStringKind::Literal {
                non_synchronizing: true
            }
        )
    {
        return Err(invalid("non-synchronizing IMAP QUOTA server literal").at(start));
    }
    let end = start + parsed.end;
    Ok(ParsedQuotaAString { end })
}

fn validate_exact_astring(input: &[u8], context: &'static str) -> Result<(), ProtocolError> {
    let end = validate_astring_at(input, 0, false)?.end;
    if end == input.len() {
        Ok(())
    } else {
        Err(invalid(context).at(end))
    }
}

fn classify_resource(name: &[u8]) -> QuotaResourceName<'_> {
    if name.eq_ignore_ascii_case(b"STORAGE") {
        QuotaResourceName::Storage
    } else if name.eq_ignore_ascii_case(b"MESSAGE") {
        QuotaResourceName::Message
    } else if name.eq_ignore_ascii_case(b"MAILBOX") {
        QuotaResourceName::Mailbox
    } else if name.eq_ignore_ascii_case(b"ANNOTATION-STORAGE") {
        QuotaResourceName::AnnotationStorage
    } else {
        QuotaResourceName::Other(name)
    }
}

fn validate_atom(input: &[u8], context: &'static str) -> Result<(), ProtocolError> {
    let parsed = parse_astring_prefix(input).map_err(|_| invalid(context))?;
    if parsed.end != input.len() || parsed.kind != AStringKind::Atom {
        return Err(invalid(context));
    }
    Ok(())
}

fn parse_number64(digits: &[u8], offset: usize) -> Result<u64, ProtocolError> {
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return Err(invalid("IMAP QUOTA number64").at(offset));
    }
    let value = digits.iter().try_fold(0u64, |value, digit| {
        value
            .checked_mul(10)
            .and_then(|value| value.checked_add(u64::from(*digit - b'0')))
    });
    value
        .filter(|value| i64::try_from(*value).is_ok())
        .ok_or_else(|| invalid("IMAP QUOTA number64 boundary").at(offset))
}

fn required_token_end(
    input: &[u8],
    start: usize,
    context: &'static str,
) -> Result<usize, ProtocolError> {
    let end = value_token_end(input, start);
    if end == start {
        Err(invalid(context).at(start))
    } else {
        Ok(end)
    }
}

fn value_token_end(input: &[u8], start: usize) -> usize {
    input[start..]
        .iter()
        .position(|byte| matches!(byte, b' ' | b')'))
        .map_or(input.len(), |offset| start + offset)
}

fn next_item(input: &[u8], end: usize) -> Option<&[u8]> {
    if end == input.len() {
        Some(b"")
    } else {
        input.get(end + 1..)
    }
}

fn list_interior<'a>(wire: &'a [u8], range: &Range<usize>) -> &'a [u8] {
    &wire[range.start + 1..range.end - 1]
}

fn validate_name(input: &[u8], name: &[u8], context: &'static str) -> Result<(), ProtocolError> {
    if input.len() < name.len() || !input[..name.len()].eq_ignore_ascii_case(name) {
        return Err(invalid(context));
    }
    Ok(())
}

fn require_space(
    input: &[u8],
    cursor: usize,
    context: &'static str,
) -> Result<usize, ProtocolError> {
    if input.get(cursor) == Some(&b' ') {
        Ok(cursor + 1)
    } else {
        Err(invalid(context).at(cursor))
    }
}

fn shift_error(error: ProtocolError, start: usize) -> ProtocolError {
    error.at(start.saturating_add(error.offset().unwrap_or(0)))
}

const fn invalid(context: &'static str) -> ProtocolError {
    ProtocolError::new(ErrorKind::InvalidSyntax, context)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_command_arguments_and_empty_limit_list() {
        let get = GetQuotaArguments::parse(&Bytes::from_static(b"\"user@example.com\"")).unwrap();
        assert_eq!(get.root().decoded().as_ref(), b"user@example.com");
        let get_root = GetQuotaRootArguments::parse(&Bytes::from_static(b"INBOX")).unwrap();
        assert_eq!(get_root.mailbox().decoded().as_ref(), b"INBOX");

        let set = SetQuotaArguments::parse(&Bytes::from_static(b"\"\" ()")).unwrap();
        assert_eq!(set.limit_count(), 0);
        assert!(set.limits().next().is_none());
    }

    #[test]
    fn parses_setquota_resource_limits() {
        let value = SetQuotaArguments::parse(&Bytes::from_static(
            b"\"user@example.com\" (STORAGE 512 MESSAGE 1000 X-VENDOR 42)",
        ))
        .unwrap();
        let limits: Vec<_> = value.limits().collect();
        assert_eq!(limits.len(), 3);
        assert_eq!(limits[0].name(), QuotaResourceName::Storage);
        assert_eq!(limits[0].limit(), 512);
        assert_eq!(limits[1].name(), QuotaResourceName::Message);
        assert_eq!(limits[2].name(), QuotaResourceName::Other(b"X-VENDOR"));
    }

    #[test]
    fn parses_quota_response_and_allows_soft_limit_excess() {
        let value = QuotaResponse::parse(&Bytes::from_static(
            b"QUOTA \"user@example.com\" (STORAGE 2048 1024 MESSAGE 7 100)",
        ))
        .unwrap();
        let resources: Vec<_> = value.resources().collect();
        assert_eq!(resources.len(), 2);
        assert_eq!(resources[0].name(), QuotaResourceName::Storage);
        assert_eq!(resources[0].usage(), 2048);
        assert_eq!(resources[0].limit(), 1024);
        assert_eq!(resources[1].name(), QuotaResourceName::Message);
    }

    #[test]
    fn parses_zero_or_many_quota_roots_without_copying_payloads() {
        let empty = QuotaRootResponse::parse(&Bytes::from_static(b"QUOTAROOT INBOX")).unwrap();
        assert_eq!(empty.root_count(), 0);

        let value = QuotaRootResponse::parse(&Bytes::from_static(
            b"QUOTAROOT INBOX \"user@example.com\" shared",
        ))
        .unwrap();
        let roots: Vec<_> = value.roots().collect();
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0].decoded().as_ref(), b"user@example.com");
        assert_eq!(roots[1].decoded().as_ref(), b"shared");
        let wire = value.as_bytes();
        assert!(roots[0].as_wire().as_ptr() >= wire.as_ptr());
        assert!(roots[0].as_wire().as_ptr() < wire.as_ptr().wrapping_add(wire.len()));
    }

    #[test]
    fn enforces_number64_resources_and_response_literal_rules() {
        assert!(
            QuotaResponse::parse(&Bytes::from_static(
                b"QUOTA root (STORAGE 9223372036854775807 9223372036854775807)"
            ))
            .is_ok()
        );
        for wire in [
            b"QUOTA root (STORAGE 1 9223372036854775808)".as_slice(),
            b"QUOTA root (STORAGE 1 2 storage 3 4)".as_slice(),
            b"QUOTA root (BAD*NAME 1 2)".as_slice(),
            b"QUOTA {4+}\r\nroot ()".as_slice(),
            b"QUOTAROOT INBOX {4+}\r\nroot".as_slice(),
        ] {
            if wire.starts_with(b"QUOTAROOT") {
                assert!(QuotaRootResponse::parse(&Bytes::copy_from_slice(wire)).is_err());
            } else {
                assert!(QuotaResponse::parse(&Bytes::copy_from_slice(wire)).is_err());
            }
        }
    }

    #[test]
    fn rejects_malformed_or_duplicate_setquota_resources() {
        for wire in [
            b"root".as_slice(),
            b"root (STORAGE)".as_slice(),
            b"root (STORAGE 1 )".as_slice(),
            b"root (STORAGE 1 storage 2)".as_slice(),
            b"root (STORAGE 9223372036854775808)".as_slice(),
        ] {
            assert!(SetQuotaArguments::parse(&Bytes::copy_from_slice(wire)).is_err());
        }
    }

    #[test]
    fn encoding_is_exact() {
        let value = SetQuotaArguments::parse(&Bytes::from_static(b"root (STORAGE 1)")).unwrap();
        let mut encoded = BytesMut::new();
        value.encode(&mut encoded);
        assert_eq!(&encoded[..], value.as_bytes().as_ref());

        let response = QuotaResponse::parse(&Bytes::from_static(b"QUOTA root ()")).unwrap();
        encoded.clear();
        response.encode(&mut encoded);
        assert_eq!(&encoded[..], response.as_bytes().as_ref());
    }
}
