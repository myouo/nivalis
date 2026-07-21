use bytes::{BufMut, Bytes, BytesMut};
use mail_protocol_core::wire::{eq_ascii, slice_ref as slice_for};
use mail_protocol_core::{ErrorKind, ProtocolError};

const MAX_CAPABILITIES: usize = 256;

/// One capability advertised by an IMAP server.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Capability {
    /// `IMAP4rev1` base protocol.
    Imap4Rev1,
    /// `IMAP4rev2` base protocol.
    Imap4Rev2,
    /// STARTTLS command.
    StartTls,
    /// Clear-text LOGIN is disabled.
    LoginDisabled,
    /// IDLE command.
    Idle,
    /// ENABLE command.
    Enable,
    /// SASL initial response.
    SaslIr,
    /// Non-synchronizing literals of any supported size.
    LiteralPlus,
    /// Non-synchronizing literals up to 4,096 octets.
    LiteralMinus,
    /// MOVE command.
    Move,
    /// UNSELECT command.
    Unselect,
    /// NAMESPACE command.
    Namespace,
    /// UIDPLUS extension.
    UidPlus,
    /// CONDSTORE extension.
    CondStore,
    /// QRESYNC extension.
    QResync,
    /// SORT command and response extension.
    Sort,
    /// One advertised THREAD algorithm.
    Thread {
        /// Registered or extension algorithm name without the `THREAD=` prefix.
        algorithm: Bytes,
    },
    /// Legacy RFC 2087 QUOTA support without RFC 9208 resource discovery.
    Quota,
    /// RFC 9208 SETQUOTA support.
    QuotaSet,
    /// One RFC 9208 quota resource type.
    QuotaResource {
        /// Registered or extension resource name without the `QUOTA=RES-` prefix.
        resource: Bytes,
    },
    /// UTF-8 mailbox and message support can be enabled.
    Utf8Accept,
    /// An advertised SASL authentication mechanism.
    Auth {
        /// Mechanism name without the `AUTH=` prefix.
        mechanism: Bytes,
    },
    /// Maximum accepted APPEND size, or support without a fixed advertised limit.
    AppendLimit {
        /// Maximum octets when the server advertised a numeric value.
        limit: Option<u64>,
    },
    /// A capability not represented by a typed variant.
    Other {
        /// Complete capability token exactly as received.
        token: Bytes,
    },
}

impl Capability {
    fn parse(token: Bytes) -> Result<Self, ProtocolError> {
        validate_token(&token)?;
        let value = token.as_ref();
        if eq_ascii(value, b"IMAP4REV1") {
            Ok(Self::Imap4Rev1)
        } else if eq_ascii(value, b"IMAP4REV2") {
            Ok(Self::Imap4Rev2)
        } else if eq_ascii(value, b"STARTTLS") {
            Ok(Self::StartTls)
        } else if eq_ascii(value, b"LOGINDISABLED") {
            Ok(Self::LoginDisabled)
        } else if eq_ascii(value, b"IDLE") {
            Ok(Self::Idle)
        } else if eq_ascii(value, b"ENABLE") {
            Ok(Self::Enable)
        } else if eq_ascii(value, b"SASL-IR") {
            Ok(Self::SaslIr)
        } else if eq_ascii(value, b"LITERAL+") {
            Ok(Self::LiteralPlus)
        } else if eq_ascii(value, b"LITERAL-") {
            Ok(Self::LiteralMinus)
        } else if eq_ascii(value, b"MOVE") {
            Ok(Self::Move)
        } else if eq_ascii(value, b"UNSELECT") {
            Ok(Self::Unselect)
        } else if eq_ascii(value, b"NAMESPACE") {
            Ok(Self::Namespace)
        } else if eq_ascii(value, b"UIDPLUS") {
            Ok(Self::UidPlus)
        } else if eq_ascii(value, b"CONDSTORE") {
            Ok(Self::CondStore)
        } else if eq_ascii(value, b"QRESYNC") {
            Ok(Self::QResync)
        } else if eq_ascii(value, b"SORT") {
            Ok(Self::Sort)
        } else if value.len() > 7 && eq_ascii(&value[..7], b"THREAD=") {
            validate_parameter_atom(&value[7..], "IMAP THREAD capability")?;
            Ok(Self::Thread {
                algorithm: token.slice(7..),
            })
        } else if eq_ascii(value, b"QUOTA") {
            Ok(Self::Quota)
        } else if eq_ascii(value, b"QUOTASET") {
            Ok(Self::QuotaSet)
        } else if value.len() > 10 && eq_ascii(&value[..10], b"QUOTA=RES-") {
            validate_parameter_atom(&value[10..], "IMAP QUOTA resource capability")?;
            Ok(Self::QuotaResource {
                resource: token.slice(10..),
            })
        } else if eq_ascii(value, b"UTF8=ACCEPT") {
            Ok(Self::Utf8Accept)
        } else if value.len() >= 5 && eq_ascii(&value[..5], b"AUTH=") {
            validate_sasl_mechanism(&value[5..])?;
            Ok(Self::Auth {
                mechanism: token.slice(5..),
            })
        } else if eq_ascii(value, b"APPENDLIMIT") {
            Ok(Self::AppendLimit { limit: None })
        } else if value.len() > 12 && eq_ascii(&value[..12], b"APPENDLIMIT=") {
            Ok(parse_u64(&value[12..])
                .filter(|limit| u32::try_from(*limit).is_ok())
                .map_or_else(
                    || Self::Other { token },
                    |limit| Self::AppendLimit { limit: Some(limit) },
                ))
        } else {
            Ok(Self::Other { token })
        }
    }

    /// Appends the canonical wire token to `dst`.
    ///
    /// Validation is completed before `dst` is modified, so an error never
    /// leaves a partial capability token in the output.
    ///
    /// # Errors
    ///
    /// Returns an error when a caller-constructed `AUTH`, `THREAD`, quota
    /// resource, or unknown capability is not valid wire syntax.
    pub fn encode(&self, dst: &mut BytesMut) -> Result<(), ProtocolError> {
        self.validate_for_wire()?;
        match self {
            Self::Imap4Rev1 => dst.put_slice(b"IMAP4rev1"),
            Self::Imap4Rev2 => dst.put_slice(b"IMAP4rev2"),
            Self::StartTls => dst.put_slice(b"STARTTLS"),
            Self::LoginDisabled => dst.put_slice(b"LOGINDISABLED"),
            Self::Idle => dst.put_slice(b"IDLE"),
            Self::Enable => dst.put_slice(b"ENABLE"),
            Self::SaslIr => dst.put_slice(b"SASL-IR"),
            Self::LiteralPlus => dst.put_slice(b"LITERAL+"),
            Self::LiteralMinus => dst.put_slice(b"LITERAL-"),
            Self::Move => dst.put_slice(b"MOVE"),
            Self::Unselect => dst.put_slice(b"UNSELECT"),
            Self::Namespace => dst.put_slice(b"NAMESPACE"),
            Self::UidPlus => dst.put_slice(b"UIDPLUS"),
            Self::CondStore => dst.put_slice(b"CONDSTORE"),
            Self::QResync => dst.put_slice(b"QRESYNC"),
            Self::Sort => dst.put_slice(b"SORT"),
            Self::Thread { algorithm } => {
                dst.put_slice(b"THREAD=");
                dst.put_slice(algorithm);
            }
            Self::Quota => dst.put_slice(b"QUOTA"),
            Self::QuotaSet => dst.put_slice(b"QUOTASET"),
            Self::QuotaResource { resource } => {
                dst.put_slice(b"QUOTA=RES-");
                dst.put_slice(resource);
            }
            Self::Utf8Accept => dst.put_slice(b"UTF8=ACCEPT"),
            Self::Auth { mechanism } => {
                dst.put_slice(b"AUTH=");
                dst.put_slice(mechanism);
            }
            Self::AppendLimit { limit } => {
                dst.put_slice(b"APPENDLIMIT");
                if let Some(limit) = limit {
                    dst.put_u8(b'=');
                    put_u64(*limit, dst);
                }
            }
            Self::Other { token } => dst.put_slice(token),
        }
        Ok(())
    }

    fn validate_for_wire(&self) -> Result<(), ProtocolError> {
        match self {
            Self::Auth { mechanism } => validate_sasl_mechanism(mechanism),
            Self::Thread { algorithm } => {
                validate_parameter_atom(algorithm, "IMAP THREAD capability")
            }
            Self::QuotaResource { resource } => {
                validate_parameter_atom(resource, "IMAP QUOTA resource capability")
            }
            Self::Other { token } => Self::parse(token.clone()).map(|_| ()),
            Self::AppendLimit { limit: Some(limit) } if *limit > u64::from(u32::MAX) => Err(
                ProtocolError::new(ErrorKind::InvalidSyntax, "IMAP APPENDLIMIT capability"),
            ),
            _ => Ok(()),
        }
    }

    fn equivalent(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::AppendLimit { limit: left }, Self::AppendLimit { limit: right }) => {
                left == right
            }
            (Self::Auth { mechanism: left }, Self::Auth { mechanism: right })
            | (Self::Thread { algorithm: left }, Self::Thread { algorithm: right })
            | (Self::QuotaResource { resource: left }, Self::QuotaResource { resource: right }) => {
                eq_ascii(left, right)
            }
            (Self::Other { token: left }, Self::Other { token: right }) => eq_ascii(left, right),
            (
                Self::Auth { .. }
                | Self::AppendLimit { .. }
                | Self::Thread { .. }
                | Self::QuotaResource { .. }
                | Self::Other { .. },
                _,
            )
            | (
                _,
                Self::Auth { .. }
                | Self::AppendLimit { .. }
                | Self::Thread { .. }
                | Self::QuotaResource { .. }
                | Self::Other { .. },
            ) => false,
            _ => core::mem::discriminant(self) == core::mem::discriminant(other),
        }
    }
}

/// Deduplicated, insertion-ordered IMAP capability collection.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CapabilitySet {
    items: Vec<Capability>,
}

impl CapabilitySet {
    /// Creates an empty capability set.
    pub const fn new() -> Self {
        Self { items: Vec::new() }
    }

    /// Parses the data following an untagged IMAP response marker.
    ///
    /// The expected input form is `CAPABILITY token ...` without the leading `*`.
    ///
    /// # Errors
    ///
    /// Returns an error when the response is not a CAPABILITY response, omits
    /// both base-protocol capabilities, contains an invalid token, or exceeds
    /// 256 distinct capabilities.
    pub fn parse_response(data: &Bytes) -> Result<Self, ProtocolError> {
        Self::parse_named_response(data, b"CAPABILITY", "IMAP CAPABILITY response", true)
    }

    pub(crate) fn parse_enabled_response(data: &Bytes) -> Result<Self, ProtocolError> {
        Self::parse_named_response(data, b"ENABLED", "IMAP ENABLED response", false)
    }

    fn parse_named_response(
        data: &Bytes,
        expected: &[u8],
        context: &'static str,
        require_base_protocol: bool,
    ) -> Result<Self, ProtocolError> {
        if data.len() < expected.len() || !eq_ascii(&data[..expected.len()], expected) {
            return Err(ProtocolError::new(ErrorKind::InvalidSyntax, context));
        }

        let mut capabilities = Self::default();
        if data.len() == expected.len() {
            return if require_base_protocol {
                Err(ProtocolError::new(ErrorKind::InvalidSyntax, context))
            } else {
                Ok(capabilities)
            };
        }
        if data.get(expected.len()) != Some(&b' ') {
            return Err(ProtocolError::new(ErrorKind::InvalidSyntax, context).at(expected.len()));
        }
        let remaining = &data[expected.len() + 1..];
        if remaining.is_empty()
            || remaining.first() == Some(&b' ')
            || remaining.last() == Some(&b' ')
            || remaining.contains(&b'\t')
        {
            return Err(
                ProtocolError::new(ErrorKind::InvalidSyntax, context).at(expected.len() + 1)
            );
        }
        for token in remaining.split(|byte| *byte == b' ') {
            if token.is_empty() {
                return Err(ProtocolError::new(ErrorKind::InvalidSyntax, context));
            }
            capabilities.try_insert(Capability::parse(slice_for(data, token))?)?;
        }
        if require_base_protocol
            && !capabilities.contains(&Capability::Imap4Rev1)
            && !capabilities.contains(&Capability::Imap4Rev2)
        {
            return Err(ProtocolError::new(ErrorKind::InvalidSyntax, context));
        }
        Ok(capabilities)
    }

    /// Validates and inserts a capability unless an ASCII-case-insensitive
    /// equivalent exists.
    ///
    /// Unknown tokens that spell a supported capability are normalized to the
    /// corresponding typed variant before deduplication.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid caller-constructed values or when inserting
    /// a 257th distinct capability would exceed the collection budget.
    pub fn try_insert(&mut self, capability: Capability) -> Result<bool, ProtocolError> {
        let capability = normalize_capability(capability)?;
        if self.items.iter().any(|item| item.equivalent(&capability)) {
            return Ok(false);
        }
        if self.items.len() == MAX_CAPABILITIES {
            return Err(ProtocolError::new(
                ErrorKind::FrameTooLarge,
                "IMAP capability count",
            ));
        }
        self.items.push(capability);
        Ok(true)
    }

    /// Inserts a valid capability unless an equivalent exists.
    ///
    /// This compatibility helper returns `false` for duplicates, invalid
    /// caller-constructed values, and values beyond the 256-item budget. Use
    /// [`Self::try_insert`] when the distinction matters.
    pub fn insert(&mut self, capability: Capability) -> bool {
        self.try_insert(capability).unwrap_or(false)
    }

    /// Atomically adds every capability from `other`, preserving first-seen order.
    ///
    /// # Errors
    ///
    /// Returns an error without changing `self` when the combined set would
    /// exceed 256 distinct capabilities.
    pub fn try_extend_from(&mut self, other: &Self) -> Result<(), ProtocolError> {
        let additional = other
            .items
            .iter()
            .filter(|capability| !self.contains(capability))
            .count();
        if self.items.len().saturating_add(additional) > MAX_CAPABILITIES {
            return Err(ProtocolError::new(
                ErrorKind::FrameTooLarge,
                "IMAP capability count",
            ));
        }
        for capability in &other.items {
            if !self.contains(capability) {
                self.items.push(capability.clone());
            }
        }
        Ok(())
    }

    /// Adds every capability from `other`, preserving first-seen order.
    ///
    /// This compatibility helper leaves `self` unchanged if the combined set
    /// would exceed 256 items. Use [`Self::try_extend_from`] when the error
    /// distinction matters.
    pub fn extend_from(&mut self, other: &Self) {
        let _ = self.try_extend_from(other);
    }

    /// Parses and deduplicates already separated capability tokens.
    ///
    /// # Errors
    ///
    /// Returns an error when a token is invalid or the input contains more than
    /// 256 distinct capabilities.
    pub fn from_tokens(tokens: impl IntoIterator<Item = Bytes>) -> Result<Self, ProtocolError> {
        let mut capabilities = Self::default();
        for token in tokens {
            capabilities.try_insert(Capability::parse(token)?)?;
        }
        Ok(capabilities)
    }

    /// Returns whether an equivalent capability is present.
    pub fn contains(&self, capability: &Capability) -> bool {
        let Ok(capability) = normalize_capability(capability.clone()) else {
            return false;
        };
        self.items.iter().any(|item| item.equivalent(&capability))
    }

    /// Removes an equivalent capability if present.
    pub fn remove(&mut self, capability: &Capability) -> bool {
        let Ok(capability) = normalize_capability(capability.clone()) else {
            return false;
        };
        let Some(index) = self
            .items
            .iter()
            .position(|item| item.equivalent(&capability))
        else {
            return false;
        };
        self.items.remove(index);
        true
    }

    /// Returns capabilities in advertisement order.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &Capability> {
        self.items.iter()
    }

    /// Returns advertised SASL mechanism names.
    pub fn auth_mechanisms(&self) -> impl Iterator<Item = &Bytes> {
        self.items.iter().filter_map(|capability| match capability {
            Capability::Auth { mechanism } => Some(mechanism),
            _ => None,
        })
    }

    /// Returns advertised RFC 5256 THREAD algorithm names.
    pub fn thread_algorithms(&self) -> impl Iterator<Item = &Bytes> {
        self.items.iter().filter_map(|capability| match capability {
            Capability::Thread { algorithm } => Some(algorithm),
            _ => None,
        })
    }

    /// Returns advertised RFC 9208 quota resource names.
    pub fn quota_resources(&self) -> impl Iterator<Item = &Bytes> {
        self.items.iter().filter_map(|capability| match capability {
            Capability::QuotaResource { resource } => Some(resource),
            _ => None,
        })
    }

    /// Returns the number of distinct capabilities.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Returns whether no capabilities are present.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

fn normalize_capability(capability: Capability) -> Result<Capability, ProtocolError> {
    match capability {
        Capability::Auth { mechanism } => {
            validate_sasl_mechanism(&mechanism)?;
            Ok(Capability::Auth { mechanism })
        }
        Capability::Thread { algorithm } => {
            validate_parameter_atom(&algorithm, "IMAP THREAD capability")?;
            Ok(Capability::Thread { algorithm })
        }
        Capability::QuotaResource { resource } => {
            validate_parameter_atom(&resource, "IMAP QUOTA resource capability")?;
            Ok(Capability::QuotaResource { resource })
        }
        Capability::Other { token } => Capability::parse(token),
        capability => Ok(capability),
    }
}

fn validate_token(token: &[u8]) -> Result<(), ProtocolError> {
    if token.is_empty()
        || token.iter().any(|byte| {
            !byte.is_ascii()
                || byte.is_ascii_control()
                || matches!(
                    byte,
                    b' ' | b'(' | b')' | b'{' | b'%' | b'*' | b'"' | b'\\' | b']'
                )
        })
    {
        Err(ProtocolError::new(
            ErrorKind::InvalidSyntax,
            "IMAP capability token",
        ))
    } else {
        Ok(())
    }
}

fn validate_sasl_mechanism(mechanism: &[u8]) -> Result<(), ProtocolError> {
    if !(1..=20).contains(&mechanism.len())
        || !mechanism
            .iter()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || b"-_".contains(byte))
    {
        return Err(ProtocolError::new(
            ErrorKind::InvalidSyntax,
            "IMAP SASL mechanism",
        ));
    }
    Ok(())
}

fn validate_parameter_atom(value: &[u8], context: &'static str) -> Result<(), ProtocolError> {
    if value.is_empty()
        || value.iter().any(|byte| {
            !byte.is_ascii()
                || byte.is_ascii_control()
                || matches!(
                    byte,
                    b' ' | b'(' | b')' | b'{' | b'%' | b'*' | b'"' | b'\\' | b']' | b'='
                )
        })
    {
        Err(ProtocolError::new(ErrorKind::InvalidSyntax, context))
    } else {
        Ok(())
    }
}

fn parse_u64(value: &[u8]) -> Option<u64> {
    if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
        return None;
    }
    value.iter().try_fold(0u64, |number, digit| {
        number
            .checked_mul(10)
            .and_then(|number| number.checked_add(u64::from(*digit - b'0')))
    })
}

fn put_u64(value: u64, dst: &mut BytesMut) {
    let mut buffer = [0u8; 20];
    let mut cursor = buffer.len();
    let mut remaining = value;
    loop {
        cursor -= 1;
        buffer[cursor] = b'0' + u8::try_from(remaining % 10).expect("decimal digit fits u8");
        remaining /= 10;
        if remaining == 0 {
            break;
        }
    }
    dst.put_slice(&buffer[cursor..]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_parameterized_and_unknown_capabilities() {
        let data = Bytes::from_static(
            b"CAPABILITY IMAP4rev2 STARTTLS AUTH=SCRAM-SHA-256 APPENDLIMIT=123 SORT THREAD=REFERENCES QUOTA=RES-STORAGE QUOTASET X-VENDOR",
        );
        let capabilities = CapabilitySet::parse_response(&data).unwrap();
        assert!(capabilities.contains(&Capability::Imap4Rev2));
        assert!(capabilities.contains(&Capability::StartTls));
        assert!(capabilities.contains(&Capability::AppendLimit { limit: Some(123) }));
        assert_eq!(
            capabilities.auth_mechanisms().collect::<Vec<_>>(),
            vec![&Bytes::from_static(b"SCRAM-SHA-256")]
        );
        assert!(capabilities.contains(&Capability::Sort));
        assert!(capabilities.contains(&Capability::Thread {
            algorithm: Bytes::from_static(b"REFERENCES"),
        }));
        assert!(capabilities.contains(&Capability::QuotaResource {
            resource: Bytes::from_static(b"STORAGE"),
        }));
        assert!(capabilities.contains(&Capability::QuotaSet));
        assert_eq!(
            capabilities.thread_algorithms().collect::<Vec<_>>(),
            vec![&Bytes::from_static(b"REFERENCES")]
        );
        assert_eq!(
            capabilities.quota_resources().collect::<Vec<_>>(),
            vec![&Bytes::from_static(b"STORAGE")]
        );
        assert_eq!(capabilities.len(), 9);
    }

    #[test]
    fn validates_parameterized_sort_thread_and_quota_capabilities_atomically() {
        let capabilities = CapabilitySet::from_tokens([
            Bytes::from_static(b"SORT"),
            Bytes::from_static(b"THREAD=ORDEREDSUBJECT"),
            Bytes::from_static(b"THREAD=REFERENCES"),
            Bytes::from_static(b"QUOTA"),
            Bytes::from_static(b"QUOTASET"),
            Bytes::from_static(b"QUOTA=RES-STORAGE"),
            Bytes::from_static(b"QUOTA=RES-X-VENDOR"),
        ])
        .unwrap();
        assert_eq!(capabilities.len(), 7);

        for invalid in [
            Capability::Thread {
                algorithm: Bytes::new(),
            },
            Capability::Thread {
                algorithm: Bytes::from_static(b"REF=ERENCES"),
            },
            Capability::QuotaResource {
                resource: Bytes::from_static(b"BAD RESOURCE"),
            },
        ] {
            let mut output = BytesMut::from(&b"prefix"[..]);
            assert!(invalid.encode(&mut output).is_err());
            assert_eq!(output.as_ref(), b"prefix");
        }
    }

    #[test]
    fn deduplicates_capabilities_without_case_sensitivity() {
        let data = Bytes::from_static(b"CAPABILITY IMAP4rev2 IDLE idle auth=PLAIN AUTH=PLAIN");
        let capabilities = CapabilitySet::parse_response(&data).unwrap();
        assert_eq!(capabilities.len(), 3);
    }

    #[test]
    fn malformed_append_limit_remains_available_as_unknown() {
        let data =
            Bytes::from_static(b"CAPABILITY IMAP4rev1 APPENDLIMIT=many APPENDLIMIT=4294967296");
        let capabilities = CapabilitySet::parse_response(&data).unwrap();
        assert!(matches!(
            capabilities.iter().nth(1),
            Some(Capability::Other { token }) if token.as_ref() == b"APPENDLIMIT=many"
        ));
        assert!(matches!(
            capabilities.iter().nth(2),
            Some(Capability::Other { token }) if token.as_ref() == b"APPENDLIMIT=4294967296"
        ));
    }

    #[test]
    fn capability_response_requires_a_base_protocol_but_enabled_can_be_empty() {
        for invalid in [b"CAPABILITY".as_slice(), b"CAPABILITY IDLE"] {
            assert!(CapabilitySet::parse_response(&Bytes::copy_from_slice(invalid)).is_err());
        }

        assert_eq!(
            CapabilitySet::parse_response(&Bytes::from_static(b"CAPABILITY IMAP4rev1"))
                .unwrap()
                .len(),
            1
        );
        assert!(
            CapabilitySet::parse_enabled_response(&Bytes::from_static(b"ENABLED"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn enforces_rfc_4422_sasl_mechanism_syntax() {
        for valid in [
            b"AUTH=A".as_slice(),
            b"AUTH=PLAIN",
            b"AUTH=SCRAM-SHA-256",
            b"AUTH=A-_09",
            b"AUTH=ABCDEFGHIJKLMNOPQRST",
        ] {
            let capabilities = CapabilitySet::from_tokens([Bytes::copy_from_slice(valid)]).unwrap();
            assert_eq!(capabilities.len(), 1);
        }

        for invalid in [
            b"AUTH=".as_slice(),
            b"AUTH=plain",
            b"AUTH=A.B",
            b"AUTH=ABCDEFGHIJKLMNOPQRSTU",
        ] {
            assert!(CapabilitySet::from_tokens([Bytes::copy_from_slice(invalid)]).is_err());
        }
    }

    #[test]
    fn validates_and_normalizes_public_insertions() {
        let mut capabilities = CapabilitySet::new();
        assert!(
            capabilities
                .try_insert(Capability::Other {
                    token: Bytes::from_static(b"IDLE"),
                })
                .unwrap()
        );
        assert!(matches!(capabilities.iter().next(), Some(Capability::Idle)));
        let idle_alias = Capability::Other {
            token: Bytes::from_static(b"idle"),
        };
        assert!(capabilities.contains(&idle_alias));
        assert!(capabilities.remove(&idle_alias));
        assert!(capabilities.is_empty());
        assert!(capabilities.try_insert(idle_alias).unwrap());
        assert!(!capabilities.try_insert(Capability::Idle).unwrap());

        assert!(
            capabilities
                .try_insert(Capability::Other {
                    token: Bytes::from_static(b"AUTH=PLAIN"),
                })
                .unwrap()
        );
        assert!(matches!(
            capabilities.iter().nth(1),
            Some(Capability::Auth { mechanism }) if mechanism.as_ref() == b"PLAIN"
        ));

        let length = capabilities.len();
        assert!(
            capabilities
                .try_insert(Capability::Auth {
                    mechanism: Bytes::from_static(b"plain"),
                })
                .is_err()
        );
        assert!(!capabilities.insert(Capability::Other {
            token: Bytes::from_static(b"BAD\r\nINJECT"),
        }));
        assert_eq!(capabilities.len(), length);
    }

    #[test]
    fn capability_encoding_is_validated_and_atomic() {
        let mut output = BytesMut::from(&b"prefix"[..]);
        for invalid in [
            Capability::Auth {
                mechanism: Bytes::from_static(b"plain"),
            },
            Capability::Other {
                token: Bytes::from_static(b"IDLE\r\nINJECTED"),
            },
            Capability::AppendLimit {
                limit: Some(u64::from(u32::MAX) + 1),
            },
        ] {
            assert!(invalid.encode(&mut output).is_err());
            assert_eq!(output.as_ref(), b"prefix");
        }

        Capability::Auth {
            mechanism: Bytes::from_static(b"SCRAM-SHA-256"),
        }
        .encode(&mut output)
        .unwrap();
        assert_eq!(output.as_ref(), b"prefixAUTH=SCRAM-SHA-256");
    }

    #[test]
    fn limits_distinct_capabilities_to_256() {
        let mut wire = BytesMut::from(&b"CAPABILITY IMAP4rev2"[..]);
        for index in 0..255 {
            wire.put_u8(b' ');
            wire.put_slice(format!("X-{index}").as_bytes());
        }

        let capabilities = CapabilitySet::parse_response(&wire.clone().freeze()).unwrap();
        assert_eq!(capabilities.len(), MAX_CAPABILITIES);

        let mut duplicate = wire.clone();
        duplicate.put_slice(b" X-0");
        assert_eq!(
            CapabilitySet::parse_response(&duplicate.freeze())
                .unwrap()
                .len(),
            MAX_CAPABILITIES
        );

        wire.put_slice(b" X-OVERFLOW");
        let error = CapabilitySet::parse_response(&wire.freeze()).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::FrameTooLarge);
    }

    #[test]
    fn capability_extension_is_atomic_at_the_item_budget() {
        let mut full = CapabilitySet::from_tokens(
            (0..MAX_CAPABILITIES).map(|index| Bytes::from(format!("X-{index}"))),
        )
        .unwrap();
        let before = full.clone();
        let overflow = CapabilitySet::from_tokens([
            Bytes::from_static(b"X-255"),
            Bytes::from_static(b"X-OVERFLOW"),
        ])
        .unwrap();

        assert_eq!(
            full.try_extend_from(&overflow).unwrap_err().kind(),
            ErrorKind::FrameTooLarge
        );
        assert_eq!(full, before);
        full.extend_from(&overflow);
        assert_eq!(full, before);
    }

    #[test]
    fn parses_empty_enabled_and_extends_additively() {
        assert!(
            CapabilitySet::parse_enabled_response(&Bytes::from_static(b"ENABLED"))
                .unwrap()
                .is_empty()
        );

        let first = CapabilitySet::parse_enabled_response(&Bytes::from_static(
            b"ENABLED CONDSTORE x-vendor",
        ))
        .unwrap();
        let second = CapabilitySet::parse_enabled_response(&Bytes::from_static(
            b"ENABLED X-VENDOR UTF8=ACCEPT",
        ))
        .unwrap();
        let mut combined = first;
        combined.extend_from(&second);
        assert_eq!(combined.len(), 3);
        assert!(combined.contains(&Capability::CondStore));
        assert!(combined.contains(&Capability::Utf8Accept));
    }

    #[test]
    fn rejects_wrong_enabled_name_and_invalid_command_tokens() {
        assert!(
            CapabilitySet::parse_enabled_response(&Bytes::from_static(b"CAPABILITY ENABLE"))
                .is_err()
        );
        assert!(CapabilitySet::from_tokens([Bytes::from_static(b"bad token")]).is_err());
        for invalid in [
            b"ENABLED ".as_slice(),
            b"ENABLED  CONDSTORE",
            b"ENABLED\tCONDSTORE",
            b"ENABLED CONDSTORE ",
        ] {
            assert!(
                CapabilitySet::parse_enabled_response(&Bytes::copy_from_slice(invalid)).is_err()
            );
        }
    }
}
