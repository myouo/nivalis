use std::num::NonZeroU32;

use bytes::{Bytes, BytesMut};
use mail_protocol_core::{DecodeStatus, Decoder, ErrorKind, Limits};
use mail_protocol_imap::{
    Capability, CapabilitySet, Response, ResponseCode, ResponseDecoder, StatusKind, UntaggedData,
    parse_untagged,
};
use rustls::pki_types::ServerName;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::{Instant, timeout_at},
};
use tokio_rustls::{TlsConnector, client::TlsStream};
use zeroize::Zeroize;

use super::{
    BoundedIo, ImapDiagnosticFailureKind, ImapDiagnosticStage, ImapInboxFetchFailure,
    InboxFetchLimits, Secret, failure, map_diagnostic_failure, map_receive_io_error,
    tls_failure_kind,
};

const READ_CHUNK_BYTES: usize = 16 * 1024;
const MAX_TAG: u32 = 1_000_000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct ProtocolMailbox {
    pub(super) exists: u32,
    pub(super) uid_validity: Option<u32>,
    pub(super) uid_next: Option<u32>,
    pub(super) highest_modseq: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ProtocolTag(Bytes);

impl ProtocolTag {
    pub(super) fn from_bytes(value: Bytes) -> Self {
        Self(value)
    }

    pub(super) fn matches(&self, value: &Bytes) -> bool {
        self.0 == *value
    }
}

pub(super) struct ProtocolSession {
    io: BoundedIo<TlsStream<TcpStream>>,
    decoder: ResponseDecoder,
    input: BytesMut,
    read_buffer: Box<[u8]>,
    capabilities: CapabilitySet,
    next_tag: u32,
}

impl ProtocolSession {
    pub(super) fn supports_condstore(&self) -> bool {
        self.capabilities.contains(&Capability::CondStore)
            || self.capabilities.contains(&Capability::QResync)
    }

    pub(super) async fn examine_inbox(
        &mut self,
        deadline: Instant,
    ) -> Result<ProtocolMailbox, ImapInboxFetchFailure> {
        let command = if self.supports_condstore() {
            b"EXAMINE INBOX (CONDSTORE)".as_slice()
        } else {
            b"EXAMINE INBOX".as_slice()
        };
        let tag = self.send_command(command, deadline).await?;
        let mut mailbox = ProtocolMailbox::default();
        loop {
            let response = self.read_response(deadline).await?;
            match &response {
                Response::Tagged {
                    tag: response_tag,
                    status,
                    ..
                } if tag.matches(response_tag) => {
                    if *status != mail_protocol_imap::Status::Ok {
                        return Err(match status {
                            mail_protocol_imap::Status::No => ImapInboxFetchFailure::Permission,
                            _ => ImapInboxFetchFailure::Protocol,
                        });
                    }
                    merge_response_code(&response, &mut mailbox)?;
                    return Ok(mailbox);
                }
                Response::Tagged { .. } => return Err(ImapInboxFetchFailure::Protocol),
                _ => match parse_untagged(&response).map_err(map_protocol_error)? {
                    Some(UntaggedData::Exists(exists)) => mailbox.exists = exists,
                    Some(UntaggedData::Status(status)) => {
                        if status.kind == StatusKind::Bye {
                            return Err(ImapInboxFetchFailure::Offline);
                        }
                        merge_status_code(status.code, &mut mailbox)?;
                    }
                    _ => {}
                },
            }
        }
    }

    pub(super) async fn run_command(
        &mut self,
        command: impl AsRef<[u8]>,
        deadline: Instant,
    ) -> Result<ProtocolTag, ImapInboxFetchFailure> {
        self.send_command(command.as_ref(), deadline).await
    }

    pub(super) async fn read_response(
        &mut self,
        deadline: Instant,
    ) -> Result<Response, ImapInboxFetchFailure> {
        loop {
            match self
                .decoder
                .decode(&mut self.input)
                .map_err(map_protocol_error)?
            {
                DecodeStatus::Complete(response) => return Ok(response),
                DecodeStatus::Incomplete => {}
            }
            let read = match timeout_at(deadline, self.io.read(&mut self.read_buffer)).await {
                Ok(Ok(read)) => read,
                Ok(Err(error)) => return Err(map_receive_io_error(&error)),
                Err(_) => return Err(ImapInboxFetchFailure::Timeout),
            };
            if read == 0 {
                return Err(ImapInboxFetchFailure::Offline);
            }
            self.input.extend_from_slice(&self.read_buffer[..read]);
        }
    }

    pub(super) async fn uid_search(
        &mut self,
        criteria: impl AsRef<[u8]>,
        deadline: Instant,
    ) -> Result<Vec<u32>, ImapInboxFetchFailure> {
        let criteria = criteria.as_ref();
        let mut command = Vec::with_capacity(b"UID SEARCH ".len() + criteria.len());
        command.extend_from_slice(b"UID SEARCH ");
        command.extend_from_slice(criteria);
        let tag = self.send_command(&command, deadline).await?;
        let mut results = None;
        loop {
            let response = self.read_response(deadline).await?;
            match &response {
                Response::Tagged {
                    tag: response_tag,
                    status,
                    ..
                } if tag.matches(response_tag) => {
                    if *status != mail_protocol_imap::Status::Ok {
                        return Err(match status {
                            mail_protocol_imap::Status::No => ImapInboxFetchFailure::Permission,
                            _ => ImapInboxFetchFailure::Protocol,
                        });
                    }
                    return Ok(results.unwrap_or_default());
                }
                Response::Tagged { .. } => return Err(ImapInboxFetchFailure::Protocol),
                _ => match parse_untagged(&response).map_err(map_protocol_error)? {
                    Some(UntaggedData::Search(search)) => {
                        if results.is_some() {
                            return Err(ImapInboxFetchFailure::Protocol);
                        }
                        results = Some(search.results().collect());
                    }
                    Some(UntaggedData::Status(status)) if status.kind == StatusKind::Bye => {
                        return Err(ImapInboxFetchFailure::Offline);
                    }
                    _ => {}
                },
            }
        }
    }

    pub(super) async fn logout(mut self, deadline: Instant) {
        let Ok(tag) = self.send_command(b"LOGOUT", deadline).await else {
            return;
        };
        while let Ok(response) = self.read_response(deadline).await {
            if matches!(
                response,
                Response::Tagged { tag: ref response_tag, .. } if tag.matches(response_tag)
            ) {
                break;
            }
        }
    }

    async fn send_command(
        &mut self,
        command: &[u8],
        deadline: Instant,
    ) -> Result<ProtocolTag, ImapInboxFetchFailure> {
        if command.is_empty()
            || command.len() > 32 * 1024
            || command.contains(&b'\r')
            || command.contains(&b'\n')
        {
            return Err(ImapInboxFetchFailure::Protocol);
        }
        let nonce = self.next_tag;
        self.next_tag = nonce
            .checked_add(1)
            .filter(|next| *next <= MAX_TAG)
            .ok_or(ImapInboxFetchFailure::ResourceLimit)?;
        let tag = Bytes::from(format!("N{nonce}"));
        let mut wire = Vec::with_capacity(tag.len() + command.len() + 3);
        wire.extend_from_slice(&tag);
        wire.push(b' ');
        wire.extend_from_slice(command);
        wire.extend_from_slice(b"\r\n");
        let write_result = timeout_at(deadline, self.io.write_all(&wire)).await;
        wire.zeroize();
        match write_result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(map_receive_io_error(&error)),
            Err(_) => return Err(ImapInboxFetchFailure::Timeout),
        }
        match timeout_at(deadline, self.io.flush()).await {
            Ok(Ok(())) => Ok(ProtocolTag(tag)),
            Ok(Err(error)) => Err(map_receive_io_error(&error)),
            Err(_) => Err(ImapInboxFetchFailure::Timeout),
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn open_protocol_session(
    host: &str,
    port: u16,
    login: &str,
    secret: &Secret,
    connector: TlsConnector,
    deadline: Instant,
    limits: InboxFetchLimits,
) -> Result<ProtocolSession, ImapInboxFetchFailure> {
    let server_name =
        ServerName::try_from(host.to_owned()).map_err(|_| ImapInboxFetchFailure::Protocol)?;
    let tcp = match timeout_at(deadline, TcpStream::connect((host, port))).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(error)) => {
            return Err(map_diagnostic_failure(failure(
                ImapDiagnosticStage::Connect,
                match error.kind() {
                    std::io::ErrorKind::TimedOut => ImapDiagnosticFailureKind::Timeout,
                    _ => ImapDiagnosticFailureKind::Offline,
                },
            )));
        }
        Err(_) => return Err(ImapInboxFetchFailure::Timeout),
    };
    let _ = tcp.set_nodelay(true);
    let tls = match timeout_at(deadline, connector.connect(server_name, tcp)).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(error)) => {
            return Err(map_diagnostic_failure(failure(
                ImapDiagnosticStage::Tls,
                tls_failure_kind(&error),
            )));
        }
        Err(_) => return Err(ImapInboxFetchFailure::Timeout),
    };
    let codec_limits = Limits::new(
        64 * 1024,
        // Metadata sessions are cached and reused by the on-demand body path.
        // The operation-specific checks below the codec retain the stricter
        // per-message publication ceiling.
        limits.max_page_literal_bytes,
        limits.max_server_bytes,
        256,
    );
    let mut session = ProtocolSession {
        io: BoundedIo::new(tls, limits.max_server_bytes, limits.max_client_bytes),
        decoder: ResponseDecoder::new(codec_limits),
        input: BytesMut::with_capacity(READ_CHUNK_BYTES),
        read_buffer: vec![0_u8; READ_CHUNK_BYTES].into_boxed_slice(),
        capabilities: CapabilitySet::new(),
        next_tag: 1,
    };
    let preauthenticated = session.read_greeting(deadline).await?;
    if !preauthenticated {
        session
            .login(login.as_bytes(), secret.expose(), deadline)
            .await?;
    }
    if session.capabilities.is_empty() {
        session.load_capabilities(deadline).await?;
    }
    if !session.capabilities.contains(&Capability::Imap4Rev1)
        && !session.capabilities.contains(&Capability::Imap4Rev2)
    {
        return Err(ImapInboxFetchFailure::Protocol);
    }
    Ok(session)
}

impl ProtocolSession {
    async fn read_greeting(&mut self, deadline: Instant) -> Result<bool, ImapInboxFetchFailure> {
        let response = self.read_response(deadline).await?;
        let Some(UntaggedData::Status(status)) =
            parse_untagged(&response).map_err(map_protocol_error)?
        else {
            return Err(ImapInboxFetchFailure::Protocol);
        };
        match status.kind {
            StatusKind::Ok => {
                self.merge_capability_code(status.code)?;
                Ok(false)
            }
            StatusKind::Preauth => {
                self.merge_capability_code(status.code)?;
                Ok(true)
            }
            StatusKind::Bye => Err(ImapInboxFetchFailure::Offline),
            _ => Err(ImapInboxFetchFailure::Protocol),
        }
    }

    async fn login(
        &mut self,
        login: &[u8],
        secret: &[u8],
        deadline: Instant,
    ) -> Result<(), ImapInboxFetchFailure> {
        let login = quote_astring(login)?;
        let mut password = quote_astring(secret)?;
        let mut command = Vec::with_capacity(login.len() + password.len() + 7);
        command.extend_from_slice(b"LOGIN ");
        command.extend_from_slice(&login);
        command.push(b' ');
        command.extend_from_slice(&password);
        let send_result = self.send_command(&command, deadline).await;
        command.zeroize();
        password.zeroize();
        let tag = send_result?;
        loop {
            let response = self.read_response(deadline).await?;
            match &response {
                Response::Tagged {
                    tag: response_tag,
                    status,
                    ..
                } if tag.matches(response_tag) => {
                    self.merge_capability_response(&response)?;
                    return match status {
                        mail_protocol_imap::Status::Ok => Ok(()),
                        mail_protocol_imap::Status::No => {
                            Err(ImapInboxFetchFailure::Authentication)
                        }
                        _ => Err(ImapInboxFetchFailure::Protocol),
                    };
                }
                Response::Tagged { .. } => return Err(ImapInboxFetchFailure::Protocol),
                _ => self.merge_capability_response(&response)?,
            }
        }
    }

    async fn load_capabilities(&mut self, deadline: Instant) -> Result<(), ImapInboxFetchFailure> {
        let tag = self.send_command(b"CAPABILITY", deadline).await?;
        loop {
            let response = self.read_response(deadline).await?;
            match &response {
                Response::Tagged {
                    tag: response_tag,
                    status,
                    ..
                } if tag.matches(response_tag) => {
                    return if *status == mail_protocol_imap::Status::Ok {
                        Ok(())
                    } else {
                        Err(ImapInboxFetchFailure::Protocol)
                    };
                }
                Response::Tagged { .. } => return Err(ImapInboxFetchFailure::Protocol),
                _ => self.merge_capability_response(&response)?,
            }
        }
    }

    fn merge_capability_response(
        &mut self,
        response: &Response,
    ) -> Result<(), ImapInboxFetchFailure> {
        if let Some(UntaggedData::Capability(capabilities)) =
            parse_untagged(response).map_err(map_protocol_error)?
        {
            self.capabilities
                .try_extend_from(&capabilities)
                .map_err(map_protocol_error)?;
        }
        let code = response
            .parsed_response_code()
            .map_err(map_protocol_error)?;
        self.merge_capability_code(code)
    }

    fn merge_capability_code(
        &mut self,
        code: Option<ResponseCode>,
    ) -> Result<(), ImapInboxFetchFailure> {
        if let Some(ResponseCode::Capability(capabilities)) = code {
            self.capabilities
                .try_extend_from(&capabilities)
                .map_err(map_protocol_error)?;
        }
        Ok(())
    }
}

fn quote_astring(value: &[u8]) -> Result<Vec<u8>, ImapInboxFetchFailure> {
    if value.is_empty()
        || std::str::from_utf8(value).is_err()
        || value
            .iter()
            .any(|byte| matches!(byte, b'\0' | b'\r' | b'\n'))
    {
        return Err(ImapInboxFetchFailure::Protocol);
    }
    let mut quoted = Vec::with_capacity(value.len() + 2);
    quoted.push(b'"');
    for byte in value {
        if matches!(byte, b'"' | b'\\') {
            quoted.push(b'\\');
        }
        quoted.push(*byte);
    }
    quoted.push(b'"');
    Ok(quoted)
}

fn merge_response_code(
    response: &Response,
    mailbox: &mut ProtocolMailbox,
) -> Result<(), ImapInboxFetchFailure> {
    let code = response
        .parsed_response_code()
        .map_err(map_protocol_error)?;
    merge_status_code(code, mailbox)
}

fn merge_status_code(
    code: Option<ResponseCode>,
    mailbox: &mut ProtocolMailbox,
) -> Result<(), ImapInboxFetchFailure> {
    match code {
        Some(ResponseCode::UidValidity(value)) => {
            mailbox.uid_validity =
                Some(u32::try_from(value).map_err(|_| ImapInboxFetchFailure::Protocol)?);
        }
        Some(ResponseCode::UidNext(value)) => {
            mailbox.uid_next =
                Some(u32::try_from(value).map_err(|_| ImapInboxFetchFailure::Protocol)?);
        }
        Some(ResponseCode::HighestModSeq(value)) => mailbox.highest_modseq = Some(value),
        Some(ResponseCode::NoModSeq) => mailbox.highest_modseq = None,
        _ => {}
    }
    Ok(())
}

pub(super) fn map_protocol_error(
    error: mail_protocol_core::ProtocolError,
) -> ImapInboxFetchFailure {
    match error.kind() {
        ErrorKind::LineTooLong
        | ErrorKind::FrameTooLarge
        | ErrorKind::LiteralTooLarge
        | ErrorKind::NestingTooDeep => ImapInboxFetchFailure::ResourceLimit,
        _ => ImapInboxFetchFailure::Protocol,
    }
}

pub(super) fn non_zero_uid(value: u64) -> Result<NonZeroU32, ImapInboxFetchFailure> {
    u32::try_from(value)
        .ok()
        .and_then(NonZeroU32::new)
        .ok_or(ImapInboxFetchFailure::Protocol)
}
