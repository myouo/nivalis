use std::{
    fmt,
    fs::File,
    future::{Future, poll_fn},
    io::{self, Read, Seek, SeekFrom},
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
    task::Poll,
    time::Duration,
};

use lettre::{
    Address,
    transport::smtp::{
        Error as LettreError,
        authentication::{Credentials, Mechanism},
        client::{AsyncSmtpConnection, TlsParameters},
        commands::{Data, Mail, Rcpt},
        extension::{ClientId, Extension, MailBodyParameter, MailParameter},
    },
};
use rustls::pki_types::ServerName;
use tokio::{sync::oneshot, time::Instant};

use crate::credentials::Secret;

const MAX_HOST_BYTES: usize = 253;
const MAX_LOGIN_BYTES: usize = 320;
const MAX_ADDRESS_BYTES: usize = 320;
const MAX_RECIPIENTS: usize = 64;
const MIME_CHUNK_BYTES: usize = 64 * 1024;
const MAX_OUTBOUND_MIME_BYTES: u64 = 8 * 1024 * 1024;
const SUBMISSION_TIMEOUT: Duration = Duration::from_secs(60);
const QUIT_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SmtpSecurity {
    ImplicitTls,
    StartTls,
}

pub(crate) struct SmtpSubmissionRequest {
    host: Box<str>,
    port: u16,
    login: Box<str>,
    secret: Secret,
    security: SmtpSecurity,
    envelope_from: Address,
    recipients: Box<[Address]>,
    mime_file: File,
    wire_byte_count: u64,
}

impl SmtpSubmissionRequest {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new<I, S>(
        host: &str,
        port: u16,
        login: &str,
        secret: Secret,
        security: SmtpSecurity,
        envelope_from: &str,
        recipients: I,
        mime_file: File,
        wire_byte_count: u64,
    ) -> Result<Self, SmtpSubmissionInputError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        validate_connection_input(host, port, login, &secret)?;
        let envelope_from =
            parse_address(envelope_from).map_err(|_| SmtpSubmissionInputError::EnvelopeFrom)?;

        let mut parsed_recipients = Vec::new();
        for recipient in recipients {
            if parsed_recipients.len() == MAX_RECIPIENTS {
                return Err(SmtpSubmissionInputError::RecipientCount);
            }
            parsed_recipients.push(
                parse_address(recipient.as_ref())
                    .map_err(|_| SmtpSubmissionInputError::Recipient)?,
            );
        }
        if parsed_recipients.is_empty() {
            return Err(SmtpSubmissionInputError::RecipientCount);
        }
        if !(1..=MAX_OUTBOUND_MIME_BYTES).contains(&wire_byte_count) {
            return Err(SmtpSubmissionInputError::WireByteCount);
        }

        Ok(Self {
            host: host.into(),
            port,
            login: login.into(),
            secret,
            security,
            envelope_from,
            recipients: parsed_recipients.into_boxed_slice(),
            mime_file,
            wire_byte_count,
        })
    }
}

impl fmt::Debug for SmtpSubmissionRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SmtpSubmissionRequest([REDACTED])")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SmtpSubmissionInputError {
    Host,
    Port,
    Login,
    SecretEncoding,
    EnvelopeFrom,
    Recipient,
    RecipientCount,
    WireByteCount,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum SmtpSubmissionStage {
    LoadMime = 0,
    Connect = 1,
    Tls = 2,
    Authenticate = 3,
    MailFrom = 4,
    Recipient = 5,
    DataFence = 6,
    Data = 7,
    Body = 8,
}

impl SmtpSubmissionStage {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Connect,
            2 => Self::Tls,
            3 => Self::Authenticate,
            4 => Self::MailFrom,
            5 => Self::Recipient,
            6 => Self::DataFence,
            7 => Self::Data,
            8 => Self::Body,
            _ => Self::LoadMime,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SmtpSubmissionFailureKind {
    Retryable,
    Authentication,
    Permanent,
    Certificate,
    Timeout,
    Protocol,
    ResourceLimit,
    LocalFile,
    LocalState,
    Cancelled,
    Uncertain,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SmtpSubmissionFailure {
    pub(crate) stage: SmtpSubmissionStage,
    pub(crate) kind: SmtpSubmissionFailureKind,
}

impl fmt::Display for SmtpSubmissionFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self.kind {
            SmtpSubmissionFailureKind::Retryable => {
                "the SMTP submission failed before delivery and can be retried"
            }
            SmtpSubmissionFailureKind::Authentication => {
                "the SMTP server rejected the account credentials"
            }
            SmtpSubmissionFailureKind::Permanent => {
                "the SMTP server permanently rejected the message"
            }
            SmtpSubmissionFailureKind::Certificate => {
                "the SMTP server certificate could not be verified"
            }
            SmtpSubmissionFailureKind::Timeout => "the SMTP submission timed out",
            SmtpSubmissionFailureKind::Protocol => {
                "the server did not complete a valid SMTP submission"
            }
            SmtpSubmissionFailureKind::ResourceLimit => {
                "the SMTP submission exceeded its resource limit"
            }
            SmtpSubmissionFailureKind::LocalFile => {
                "the outbound MIME file could not be read exactly"
            }
            SmtpSubmissionFailureKind::LocalState => {
                "the durable outbox could not fence the delivery attempt"
            }
            SmtpSubmissionFailureKind::Cancelled => "the SMTP submission was cancelled",
            SmtpSubmissionFailureKind::Uncertain => {
                "the SMTP connection ended after delivery started; acceptance is uncertain"
            }
        })
    }
}

impl std::error::Error for SmtpSubmissionFailure {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SmtpSubmissionReceipt {
    pub(crate) response_code: u16,
    pub(crate) wire_byte_count: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SmtpDataFenceFailure;

impl SmtpDataFenceFailure {
    pub(crate) const fn new() -> Self {
        Self
    }
}

pub(crate) type SmtpDataFence =
    Pin<Box<dyn Future<Output = Result<(), SmtpDataFenceFailure>> + Send + 'static>>;

pub(crate) struct SmtpSubmissionCancelHandle(Option<oneshot::Sender<()>>);

impl SmtpSubmissionCancelHandle {
    pub(crate) fn cancel(mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

pub(crate) struct SmtpSubmissionCancellation(oneshot::Receiver<()>);

pub(crate) fn smtp_submission_cancellation_pair()
-> (SmtpSubmissionCancelHandle, SmtpSubmissionCancellation) {
    let (sender, receiver) = oneshot::channel();
    (
        SmtpSubmissionCancelHandle(Some(sender)),
        SmtpSubmissionCancellation(receiver),
    )
}

pub(crate) async fn submit(
    request: SmtpSubmissionRequest,
) -> Result<SmtpSubmissionReceipt, SmtpSubmissionFailure> {
    submit_guarded(
        request,
        None,
        no_op_data_fence(),
        SubmissionLimits::production(),
        None,
    )
    .await
}

pub(crate) async fn submit_cancellable(
    request: SmtpSubmissionRequest,
    cancellation: SmtpSubmissionCancellation,
) -> Result<SmtpSubmissionReceipt, SmtpSubmissionFailure> {
    submit_guarded(
        request,
        Some(cancellation),
        no_op_data_fence(),
        SubmissionLimits::production(),
        None,
    )
    .await
}

pub(crate) async fn submit_with_data_fence(
    request: SmtpSubmissionRequest,
    cancellation: Option<SmtpSubmissionCancellation>,
    data_fence: SmtpDataFence,
) -> Result<SmtpSubmissionReceipt, SmtpSubmissionFailure> {
    submit_guarded(
        request,
        cancellation,
        data_fence,
        SubmissionLimits::production(),
        None,
    )
    .await
}

fn no_op_data_fence() -> SmtpDataFence {
    Box::pin(async { Ok(()) })
}

#[derive(Clone, Copy)]
struct SubmissionLimits {
    timeout: Duration,
}

impl SubmissionLimits {
    const fn production() -> Self {
        Self {
            timeout: SUBMISSION_TIMEOUT,
        }
    }
}

struct SubmissionProgress {
    stage: AtomicU8,
    data_started: AtomicBool,
}

impl SubmissionProgress {
    fn new() -> Self {
        Self {
            stage: AtomicU8::new(SmtpSubmissionStage::LoadMime as u8),
            data_started: AtomicBool::new(false),
        }
    }

    fn mark(&self, stage: SmtpSubmissionStage) {
        self.stage.store(stage as u8, Ordering::Relaxed);
    }

    fn stage(&self) -> SmtpSubmissionStage {
        SmtpSubmissionStage::from_u8(self.stage.load(Ordering::Relaxed))
    }

    fn mark_data_started(&self) {
        self.data_started.store(true, Ordering::Release);
    }

    fn data_started(&self) -> bool {
        self.data_started.load(Ordering::Acquire)
    }
}

async fn submit_guarded(
    request: SmtpSubmissionRequest,
    cancellation: Option<SmtpSubmissionCancellation>,
    data_fence: SmtpDataFence,
    limits: SubmissionLimits,
    tls_parameters: Option<TlsParameters>,
) -> Result<SmtpSubmissionReceipt, SmtpSubmissionFailure> {
    let progress = Arc::new(SubmissionProgress::new());
    let operation = submit_inner(request, progress.clone(), data_fence, tls_parameters);
    let mut operation = Box::pin(tokio::time::timeout_at(
        Instant::now() + limits.timeout,
        operation,
    ));
    let mut cancellation = cancellation.map(|value| Box::pin(value.0));

    poll_fn(move |context| {
        if let Some(signal) = cancellation.as_mut() {
            match signal.as_mut().poll(context) {
                Poll::Ready(Ok(())) => {
                    return Poll::Ready(Err(interrupted_failure(
                        &progress,
                        SmtpSubmissionFailureKind::Cancelled,
                    )));
                }
                Poll::Ready(Err(_)) => cancellation = None,
                Poll::Pending => {}
            }
        }

        match operation.as_mut().poll(context) {
            Poll::Ready(Ok(result)) => Poll::Ready(result),
            Poll::Ready(Err(_)) => Poll::Ready(Err(interrupted_failure(
                &progress,
                SmtpSubmissionFailureKind::Timeout,
            ))),
            Poll::Pending => Poll::Pending,
        }
    })
    .await
}

fn interrupted_failure(
    progress: &SubmissionProgress,
    before_data: SmtpSubmissionFailureKind,
) -> SmtpSubmissionFailure {
    failure(
        progress.stage(),
        if progress.data_started() {
            SmtpSubmissionFailureKind::Uncertain
        } else {
            before_data
        },
    )
}

async fn submit_inner(
    request: SmtpSubmissionRequest,
    progress: Arc<SubmissionProgress>,
    data_fence: SmtpDataFence,
    supplied_tls_parameters: Option<TlsParameters>,
) -> Result<SmtpSubmissionReceipt, SmtpSubmissionFailure> {
    let SmtpSubmissionRequest {
        host,
        port,
        login,
        secret,
        security,
        envelope_from,
        recipients,
        mut mime_file,
        wire_byte_count,
    } = request;

    progress.mark(SmtpSubmissionStage::LoadMime);
    let contains_non_ascii = inspect_mime(&mut mime_file, wire_byte_count)?;

    let tls_parameters = match supplied_tls_parameters {
        Some(parameters) => parameters,
        None => TlsParameters::new_rustls(host.to_string()).map_err(|_| {
            failure(
                SmtpSubmissionStage::Tls,
                SmtpSubmissionFailureKind::Certificate,
            )
        })?,
    };

    progress.mark(SmtpSubmissionStage::Connect);
    let implicit_tls =
        matches!(security, SmtpSecurity::ImplicitTls).then_some(tls_parameters.clone());
    let mut connection = AsyncSmtpConnection::connect_tokio1(
        (host.as_ref(), port),
        None,
        &ClientId::default(),
        implicit_tls,
        None,
    )
    .await
    .map_err(|error| map_lettre_error(&error, SmtpSubmissionStage::Connect, false))?;

    if security == SmtpSecurity::StartTls {
        progress.mark(SmtpSubmissionStage::Tls);
        connection
            .starttls(tls_parameters, &ClientId::default())
            .await
            .map_err(|error| map_lettre_error(&error, SmtpSubmissionStage::Tls, false))?;
    }

    progress.mark(SmtpSubmissionStage::Authenticate);
    let password = std::str::from_utf8(secret.expose()).map_err(|_| {
        failure(
            SmtpSubmissionStage::Authenticate,
            SmtpSubmissionFailureKind::Protocol,
        )
    })?;
    let credentials = Credentials::new(login.into(), password.to_owned());
    let auth_response = connection
        .auth(&[Mechanism::Plain, Mechanism::Login], &credentials)
        .await
        .map_err(|error| map_lettre_error(&error, SmtpSubmissionStage::Authenticate, false))?;
    if !auth_response.has_code(235) {
        return Err(failure(
            SmtpSubmissionStage::Authenticate,
            SmtpSubmissionFailureKind::Protocol,
        ));
    }

    let uses_smtp_utf8 = !AsRef::<str>::as_ref(&envelope_from).is_ascii()
        || recipients
            .iter()
            .any(|recipient| !AsRef::<str>::as_ref(recipient).is_ascii());
    let mut mail_parameters = Vec::with_capacity(2);
    if uses_smtp_utf8 {
        if !connection
            .server_info()
            .supports_feature(Extension::SmtpUtfEight)
        {
            return Err(failure(
                SmtpSubmissionStage::MailFrom,
                SmtpSubmissionFailureKind::Permanent,
            ));
        }
        mail_parameters.push(MailParameter::SmtpUtfEight);
    }
    if contains_non_ascii {
        if !connection
            .server_info()
            .supports_feature(Extension::EightBitMime)
        {
            return Err(failure(
                SmtpSubmissionStage::MailFrom,
                SmtpSubmissionFailureKind::Permanent,
            ));
        }
        mail_parameters.push(MailParameter::Body(MailBodyParameter::EightBitMime));
    }

    progress.mark(SmtpSubmissionStage::MailFrom);
    let mail_response = connection
        .command(Mail::new(Some(envelope_from), mail_parameters))
        .await
        .map_err(|error| map_lettre_error(&error, SmtpSubmissionStage::MailFrom, false))?;
    if !mail_response.has_code(250) {
        return Err(failure(
            SmtpSubmissionStage::MailFrom,
            SmtpSubmissionFailureKind::Protocol,
        ));
    }

    progress.mark(SmtpSubmissionStage::Recipient);
    for recipient in recipients {
        let response = connection
            .command(Rcpt::new(recipient, Vec::new()))
            .await
            .map_err(|error| map_lettre_error(&error, SmtpSubmissionStage::Recipient, false))?;
        if !response.has_code(250) && !response.has_code(251) && !response.has_code(252) {
            return Err(failure(
                SmtpSubmissionStage::Recipient,
                SmtpSubmissionFailureKind::Protocol,
            ));
        }
    }

    progress.mark(SmtpSubmissionStage::DataFence);
    data_fence.await.map_err(|_| {
        failure(
            SmtpSubmissionStage::DataFence,
            SmtpSubmissionFailureKind::LocalState,
        )
    })?;

    // The durable state is conservative from this point, even if DATA is not yet observed.
    progress.mark_data_started();
    progress.mark(SmtpSubmissionStage::Data);
    let data_response = connection
        .command(Data)
        .await
        .map_err(|error| map_lettre_error(&error, SmtpSubmissionStage::Data, false))?;
    if !data_response.has_code(354) {
        return Err(failure(
            SmtpSubmissionStage::Data,
            SmtpSubmissionFailureKind::Protocol,
        ));
    }

    progress.mark(SmtpSubmissionStage::Body);
    let read_state = Arc::new(AtomicU8::new(FileReadState::Reading as u8));
    let chunks = FileChunkIterator::new(mime_file, wire_byte_count, read_state.clone());
    let response = connection
        .message_iter(chunks)
        .await
        .map_err(|error| map_lettre_error(&error, SmtpSubmissionStage::Body, true))?;

    if FileReadState::from_u8(read_state.load(Ordering::Acquire)) != FileReadState::Complete {
        return Err(failure(
            SmtpSubmissionStage::Body,
            SmtpSubmissionFailureKind::Uncertain,
        ));
    }
    if !response.has_code(250) {
        return Err(failure(
            SmtpSubmissionStage::Body,
            SmtpSubmissionFailureKind::Uncertain,
        ));
    }

    let _ = tokio::time::timeout(QUIT_TIMEOUT, connection.quit()).await;
    Ok(SmtpSubmissionReceipt {
        response_code: 250,
        wire_byte_count,
    })
}

fn validate_connection_input(
    host: &str,
    port: u16,
    login: &str,
    secret: &Secret,
) -> Result<(), SmtpSubmissionInputError> {
    if host.is_empty()
        || host.len() > MAX_HOST_BYTES
        || host.trim() != host
        || ServerName::try_from(host.to_owned()).is_err()
    {
        return Err(SmtpSubmissionInputError::Host);
    }
    if port == 0 {
        return Err(SmtpSubmissionInputError::Port);
    }
    if login.is_empty()
        || login.len() > MAX_LOGIN_BYTES
        || login.trim() != login
        || login.contains(['\r', '\n', '\0'])
    {
        return Err(SmtpSubmissionInputError::Login);
    }
    if std::str::from_utf8(secret.expose()).is_err() {
        return Err(SmtpSubmissionInputError::SecretEncoding);
    }
    Ok(())
}

fn parse_address(value: &str) -> Result<Address, ()> {
    if value.is_empty()
        || value.len() > MAX_ADDRESS_BYTES
        || value.trim() != value
        || value.contains(['\r', '\n', '\0'])
    {
        return Err(());
    }
    value.parse().map_err(|_| ())
}

fn inspect_mime(file: &mut File, expected_byte_count: u64) -> Result<bool, SmtpSubmissionFailure> {
    let metadata = file.metadata().map_err(|_| local_file_failure())?;
    if !metadata.is_file() {
        return Err(local_file_failure());
    }
    if metadata.len() > MAX_OUTBOUND_MIME_BYTES {
        return Err(failure(
            SmtpSubmissionStage::LoadMime,
            SmtpSubmissionFailureKind::ResourceLimit,
        ));
    }
    if metadata.len() != expected_byte_count {
        return Err(local_file_failure());
    }

    file.seek(SeekFrom::Start(0))
        .map_err(|_| local_file_failure())?;
    let mut buffer = [0_u8; MIME_CHUNK_BYTES];
    let mut total = 0_u64;
    let mut contains_non_ascii = false;
    loop {
        let read = read_chunk(file, &mut buffer).map_err(|_| local_file_failure())?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .ok_or_else(resource_limit_failure)?;
        if total > MAX_OUTBOUND_MIME_BYTES {
            return Err(resource_limit_failure());
        }
        if total > expected_byte_count {
            return Err(local_file_failure());
        }
        contains_non_ascii |= !buffer[..read].is_ascii();
    }
    if total != expected_byte_count {
        return Err(local_file_failure());
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|_| local_file_failure())?;
    Ok(contains_non_ascii)
}

fn read_chunk(file: &mut File, buffer: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match file.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum FileReadState {
    Reading = 0,
    Complete = 1,
    Failed = 2,
}

impl FileReadState {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Complete,
            2 => Self::Failed,
            _ => Self::Reading,
        }
    }
}

struct FileChunkIterator {
    file: File,
    expected_byte_count: u64,
    read_byte_count: u64,
    state: Arc<AtomicU8>,
    finished: bool,
}

impl FileChunkIterator {
    fn new(file: File, expected_byte_count: u64, state: Arc<AtomicU8>) -> Self {
        Self {
            file,
            expected_byte_count,
            read_byte_count: 0,
            state,
            finished: false,
        }
    }

    fn finish(&mut self, state: FileReadState) {
        self.finished = true;
        self.state.store(state as u8, Ordering::Release);
    }
}

impl Iterator for FileChunkIterator {
    type Item = Vec<u8>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }

        let mut chunk = vec![0_u8; MIME_CHUNK_BYTES];
        let read = match read_chunk(&mut self.file, &mut chunk) {
            Ok(read) => read,
            Err(_) => {
                self.finish(FileReadState::Failed);
                return None;
            }
        };
        if read == 0 {
            let final_state = if self.read_byte_count == self.expected_byte_count {
                FileReadState::Complete
            } else {
                FileReadState::Failed
            };
            self.finish(final_state);
            return None;
        }

        let Some(total) = self.read_byte_count.checked_add(read as u64) else {
            self.finish(FileReadState::Failed);
            return None;
        };
        if total > self.expected_byte_count || total > MAX_OUTBOUND_MIME_BYTES {
            self.finish(FileReadState::Failed);
            return None;
        }
        self.read_byte_count = total;
        chunk.truncate(read);
        Some(chunk)
    }
}

fn map_lettre_error(
    error: &LettreError,
    stage: SmtpSubmissionStage,
    data_started: bool,
) -> SmtpSubmissionFailure {
    if error.is_transient() {
        return failure(stage, SmtpSubmissionFailureKind::Retryable);
    }
    if error.is_permanent() {
        return failure(
            stage,
            if stage == SmtpSubmissionStage::Authenticate {
                SmtpSubmissionFailureKind::Authentication
            } else {
                SmtpSubmissionFailureKind::Permanent
            },
        );
    }
    if data_started {
        return failure(stage, SmtpSubmissionFailureKind::Uncertain);
    }
    if error.is_tls() {
        return failure(stage, SmtpSubmissionFailureKind::Certificate);
    }
    if error.is_timeout() {
        return failure(stage, SmtpSubmissionFailureKind::Timeout);
    }
    if error_chain_has_io_kind(error, io::ErrorKind::FileTooLarge) {
        return failure(stage, SmtpSubmissionFailureKind::ResourceLimit);
    }
    if error.is_response() || error.is_client() {
        return failure(stage, SmtpSubmissionFailureKind::Protocol);
    }
    failure(stage, SmtpSubmissionFailureKind::Retryable)
}

fn error_chain_has_io_kind(error: &(dyn std::error::Error + 'static), kind: io::ErrorKind) -> bool {
    let mut source = Some(error);
    while let Some(current) = source {
        if let Some(error) = current.downcast_ref::<io::Error>()
            && error.kind() == kind
        {
            return true;
        }
        source = current.source();
    }
    false
}

const fn failure(
    stage: SmtpSubmissionStage,
    kind: SmtpSubmissionFailureKind,
) -> SmtpSubmissionFailure {
    SmtpSubmissionFailure { stage, kind }
}

const fn local_file_failure() -> SmtpSubmissionFailure {
    failure(
        SmtpSubmissionStage::LoadMime,
        SmtpSubmissionFailureKind::LocalFile,
    )
}

const fn resource_limit_failure() -> SmtpSubmissionFailure {
    failure(
        SmtpSubmissionStage::LoadMime,
        SmtpSubmissionFailureKind::ResourceLimit,
    )
}

#[cfg(test)]
mod tests {
    use std::{
        fs::OpenOptions,
        future::Future,
        io::{Seek as _, Write as _},
        net::SocketAddr,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicU64, Ordering},
        },
    };

    use lettre::transport::smtp::client::{Certificate, CertificateStore};
    use rcgen::{CertifiedKey, generate_simple_self_signed};
    use rustls::{ServerConfig, crypto, pki_types::PrivateKeyDer};
    use tokio::{
        io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
        net::{TcpListener, TcpStream},
        sync::oneshot,
        task::JoinHandle,
    };
    use tokio_rustls::TlsAcceptor;

    use super::*;

    const HOST: &str = "127.0.0.1";
    const LOGIN: &str = "sender@example.test";
    const PASSWORD: &[u8] = b"app-password";
    const TEST_TIMEOUT: Duration = Duration::from_secs(3);
    static FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    #[derive(Clone, Copy)]
    enum TestSecurity {
        ImplicitTls,
        StartTls,
    }

    #[derive(Clone, Copy)]
    enum MailBehavior {
        Success,
        Authentication535,
        Data451,
        Data550,
        DisconnectAfterData,
        ExpectNoData,
    }

    enum ServerScript {
        Mail(MailBehavior),
        StallGreeting(Option<oneshot::Sender<()>>),
    }

    struct TestServer {
        address: SocketAddr,
        security: TestSecurity,
        tls_parameters: TlsParameters,
        task: JoinHandle<io::Result<()>>,
    }

    fn run_async<F: Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .unwrap()
            .block_on(future)
    }

    fn test_limits(timeout: Duration) -> SubmissionLimits {
        SubmissionLimits { timeout }
    }

    fn request(server: &TestServer, body: &[u8]) -> SmtpSubmissionRequest {
        let file = private_mime_file(body);
        SmtpSubmissionRequest::new(
            HOST,
            server.address.port(),
            LOGIN,
            Secret::new(PASSWORD.to_vec()).unwrap(),
            match server.security {
                TestSecurity::ImplicitTls => SmtpSecurity::ImplicitTls,
                TestSecurity::StartTls => SmtpSecurity::StartTls,
            },
            LOGIN,
            ["recipient@example.test"],
            file,
            body.len() as u64,
        )
        .unwrap()
    }

    fn private_mime_file(bytes: &[u8]) -> File {
        let sequence = FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "nivalis-smtp-{}-{sequence}.eml",
            std::process::id()
        ));
        let mut options = OpenOptions::new();
        options.create_new(true).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&path).unwrap();
        file.write_all(bytes).unwrap();
        file.flush().unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        #[cfg(unix)]
        std::fs::remove_file(path).unwrap();
        file
    }

    fn test_tls() -> (TlsAcceptor, TlsParameters) {
        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(vec![HOST.to_owned()]).unwrap();
        let certificate = cert.der().clone();
        let private_key: PrivateKeyDer<'static> = signing_key.into();
        let server =
            ServerConfig::builder_with_provider(Arc::new(crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(vec![certificate.clone()], private_key)
                .unwrap();
        let client = TlsParameters::builder(HOST.to_owned())
            .certificate_store(CertificateStore::None)
            .add_root_certificate(Certificate::from_der(certificate.as_ref().to_vec()).unwrap())
            .build_rustls()
            .unwrap();
        (TlsAcceptor::from(Arc::new(server)), client)
    }

    async fn spawn_server(
        security: TestSecurity,
        script: ServerScript,
        data_fenced: Option<Arc<AtomicBool>>,
    ) -> TestServer {
        let (acceptor, tls_parameters) = test_tls();
        let listener = TcpListener::bind((HOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            match security {
                TestSecurity::ImplicitTls => {
                    let mut stream = acceptor.accept(stream).await?;
                    run_tls_session(&mut stream, script, true, data_fenced).await
                }
                TestSecurity::StartTls => {
                    run_starttls_session(stream, acceptor, script, data_fenced).await
                }
            }
        });
        TestServer {
            address,
            security,
            tls_parameters,
            task,
        }
    }

    async fn run_starttls_session(
        mut stream: TcpStream,
        acceptor: TlsAcceptor,
        script: ServerScript,
        data_fenced: Option<Arc<AtomicBool>>,
    ) -> io::Result<()> {
        write_reply(&mut stream, b"220 loopback ready\r\n").await?;
        let ehlo = read_command(&mut stream).await?;
        assert!(ehlo.starts_with("EHLO "));
        write_reply(&mut stream, b"250-loopback\r\n250 STARTTLS\r\n").await?;
        assert_eq!(read_command(&mut stream).await?, "STARTTLS");
        write_reply(&mut stream, b"220 begin TLS\r\n").await?;
        let mut stream = acceptor.accept(stream).await?;
        run_tls_session(&mut stream, script, false, data_fenced).await
    }

    async fn run_tls_session<S>(
        stream: &mut S,
        script: ServerScript,
        send_greeting: bool,
        data_fenced: Option<Arc<AtomicBool>>,
    ) -> io::Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let behavior = match script {
            ServerScript::StallGreeting(signal) => {
                if let Some(signal) = signal {
                    let _ = signal.send(());
                }
                tokio::time::sleep(Duration::from_secs(10)).await;
                return Ok(());
            }
            ServerScript::Mail(behavior) => behavior,
        };

        if send_greeting {
            write_reply(stream, b"220 loopback ready\r\n").await?;
        }
        let ehlo = read_command(stream).await?;
        assert!(ehlo.starts_with("EHLO "));
        write_reply(
            stream,
            b"250-loopback\r\n250-AUTH PLAIN LOGIN\r\n250-8BITMIME\r\n250 SMTPUTF8\r\n",
        )
        .await?;

        let auth = read_command(stream).await?;
        assert!(auth.starts_with("AUTH PLAIN "));
        if matches!(behavior, MailBehavior::Authentication535) {
            write_reply(stream, b"535 5.7.8 credentials rejected\r\n").await?;
            return Ok(());
        }
        write_reply(stream, b"235 2.7.0 authenticated\r\n").await?;

        assert!(read_command(stream).await?.starts_with("MAIL FROM:<"));
        write_reply(stream, b"250 2.1.0 sender accepted\r\n").await?;
        assert!(read_command(stream).await?.starts_with("RCPT TO:<"));
        write_reply(stream, b"250 2.1.5 recipient accepted\r\n").await?;

        match read_command(stream).await {
            Ok(command) if matches!(behavior, MailBehavior::ExpectNoData) => {
                panic!("DATA must not be sent after a failed fence: {command}")
            }
            Err(_) if matches!(behavior, MailBehavior::ExpectNoData) => return Ok(()),
            Ok(command) => assert_eq!(command, "DATA"),
            Err(error) => return Err(error),
        }
        if let Some(data_fenced) = data_fenced {
            assert!(
                data_fenced.load(Ordering::Acquire),
                "the durable fence must complete before DATA"
            );
        }

        match behavior {
            MailBehavior::Data451 => {
                write_reply(stream, b"451 4.3.0 try later\r\n").await?;
                return Ok(());
            }
            MailBehavior::Data550 => {
                write_reply(stream, b"550 5.7.1 message rejected\r\n").await?;
                return Ok(());
            }
            MailBehavior::DisconnectAfterData => {
                write_reply(stream, b"354 continue\r\n").await?;
                return Ok(());
            }
            MailBehavior::Success => {}
            MailBehavior::Authentication535 | MailBehavior::ExpectNoData => unreachable!(),
        }

        write_reply(stream, b"354 continue\r\n").await?;
        read_data_terminator(stream).await?;
        write_reply(stream, b"250 2.0.0 queued\r\n").await?;
        assert_eq!(read_command(stream).await?, "QUIT");
        write_reply(stream, b"221 2.0.0 bye\r\n").await
    }

    async fn read_command<S: AsyncRead + Unpin>(stream: &mut S) -> io::Result<String> {
        const MAX_COMMAND_BYTES: usize = 32 * 1024;
        let mut command = Vec::with_capacity(128);
        loop {
            let mut byte = [0_u8; 1];
            let read = stream.read(&mut byte).await?;
            if read == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "SMTP test connection closed",
                ));
            }
            command.push(byte[0]);
            if command.len() > MAX_COMMAND_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "SMTP test command too large",
                ));
            }
            if command.ends_with(b"\r\n") {
                command.truncate(command.len() - 2);
                return String::from_utf8(command)
                    .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-UTF-8 command"));
            }
        }
    }

    async fn read_data_terminator<S: AsyncRead + Unpin>(stream: &mut S) -> io::Result<()> {
        const MAX_TEST_DATA_BYTES: usize = (MAX_OUTBOUND_MIME_BYTES as usize) * 2;
        let mut total = 0_usize;
        let mut tail = Vec::with_capacity(4);
        let mut buffer = [0_u8; 4096];
        loop {
            let read = stream.read(&mut buffer).await?;
            if read == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "SMTP DATA ended before terminator",
                ));
            }
            total = total.saturating_add(read);
            if total > MAX_TEST_DATA_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "SMTP test DATA exceeded limit",
                ));
            }
            tail.extend_from_slice(&buffer[..read]);
            if tail.windows(5).any(|window| window == b"\r\n.\r\n") {
                return Ok(());
            }
            if tail.len() > 4 {
                tail.drain(..tail.len() - 4);
            }
        }
    }

    async fn write_reply<S: AsyncWrite + Unpin>(stream: &mut S, bytes: &[u8]) -> io::Result<()> {
        stream.write_all(bytes).await?;
        stream.flush().await
    }

    async fn submit_to_server(
        server: &TestServer,
        body: &[u8],
        cancellation: Option<SmtpSubmissionCancellation>,
        data_fence: SmtpDataFence,
        timeout: Duration,
    ) -> Result<SmtpSubmissionReceipt, SmtpSubmissionFailure> {
        submit_guarded(
            request(server, body),
            cancellation,
            data_fence,
            test_limits(timeout),
            Some(server.tls_parameters.clone()),
        )
        .await
    }

    #[test]
    fn submits_large_mime_in_fixed_bounded_chunks_after_fence() {
        run_async(async {
            let data_fenced = Arc::new(AtomicBool::new(false));
            let server = spawn_server(
                TestSecurity::ImplicitTls,
                ServerScript::Mail(MailBehavior::Success),
                Some(data_fenced.clone()),
            )
            .await;
            let mut body =
                b"From: sender@example.test\r\nTo: recipient@example.test\r\n\r\n".to_vec();
            body.resize(MIME_CHUNK_BYTES + 4096, b'x');
            let fence = Box::pin(async move {
                data_fenced.store(true, Ordering::Release);
                Ok(())
            });
            let receipt = submit_to_server(&server, &body, None, fence, TEST_TIMEOUT)
                .await
                .unwrap();
            assert_eq!(receipt.response_code, 250);
            assert_eq!(receipt.wire_byte_count, body.len() as u64);
            server.task.await.unwrap().unwrap();
        });
    }

    #[test]
    fn supports_required_starttls() {
        run_async(async {
            let server = spawn_server(
                TestSecurity::StartTls,
                ServerScript::Mail(MailBehavior::Success),
                None,
            )
            .await;
            let result = submit_to_server(
                &server,
                b"Subject: STARTTLS\r\n\r\nhello",
                None,
                no_op_data_fence(),
                TEST_TIMEOUT,
            )
            .await;
            assert!(result.is_ok());
            server.task.await.unwrap().unwrap();
        });
    }

    #[test]
    fn maps_535_to_authentication_failure() {
        run_async(async {
            let server = spawn_server(
                TestSecurity::ImplicitTls,
                ServerScript::Mail(MailBehavior::Authentication535),
                None,
            )
            .await;
            let failure = submit_to_server(
                &server,
                b"Subject: rejected\r\n\r\nbody",
                None,
                no_op_data_fence(),
                TEST_TIMEOUT,
            )
            .await
            .unwrap_err();
            assert_eq!(failure.stage, SmtpSubmissionStage::Authenticate);
            assert_eq!(failure.kind, SmtpSubmissionFailureKind::Authentication);
            server.task.await.unwrap().unwrap();
        });
    }

    #[test]
    fn distinguishes_transient_and_permanent_rejection_before_data() {
        run_async(async {
            for (behavior, expected) in [
                (MailBehavior::Data451, SmtpSubmissionFailureKind::Retryable),
                (MailBehavior::Data550, SmtpSubmissionFailureKind::Permanent),
            ] {
                let server = spawn_server(
                    TestSecurity::ImplicitTls,
                    ServerScript::Mail(behavior),
                    None,
                )
                .await;
                let failure = submit_to_server(
                    &server,
                    b"Subject: status\r\n\r\nbody",
                    None,
                    no_op_data_fence(),
                    TEST_TIMEOUT,
                )
                .await
                .unwrap_err();
                assert_eq!(failure.stage, SmtpSubmissionStage::Data);
                assert_eq!(failure.kind, expected);
                server.task.await.unwrap().unwrap();
            }
        });
    }

    #[test]
    fn disconnect_after_data_is_uncertain() {
        run_async(async {
            let server = spawn_server(
                TestSecurity::ImplicitTls,
                ServerScript::Mail(MailBehavior::DisconnectAfterData),
                None,
            )
            .await;
            let failure = submit_to_server(
                &server,
                b"Subject: uncertain\r\n\r\nbody",
                None,
                no_op_data_fence(),
                TEST_TIMEOUT,
            )
            .await
            .unwrap_err();
            assert_eq!(failure.stage, SmtpSubmissionStage::Body);
            assert_eq!(failure.kind, SmtpSubmissionFailureKind::Uncertain);
            server.task.await.unwrap().unwrap();
        });
    }

    #[test]
    fn failed_data_fence_never_sends_data() {
        run_async(async {
            let server = spawn_server(
                TestSecurity::ImplicitTls,
                ServerScript::Mail(MailBehavior::ExpectNoData),
                None,
            )
            .await;
            let failure = submit_to_server(
                &server,
                b"Subject: fence\r\n\r\nbody",
                None,
                Box::pin(async { Err(SmtpDataFenceFailure::new()) }),
                TEST_TIMEOUT,
            )
            .await
            .unwrap_err();
            assert_eq!(failure.stage, SmtpSubmissionStage::DataFence);
            assert_eq!(failure.kind, SmtpSubmissionFailureKind::LocalState);
            server.task.await.unwrap().unwrap();
        });
    }

    #[test]
    fn cancellation_before_data_is_definitive() {
        run_async(async {
            let (accepted, accepted_signal) = oneshot::channel();
            let server = spawn_server(
                TestSecurity::ImplicitTls,
                ServerScript::StallGreeting(Some(accepted)),
                None,
            )
            .await;
            let (cancel, cancellation) = smtp_submission_cancellation_pair();
            let cancel_task = tokio::spawn(async move {
                accepted_signal.await.unwrap();
                cancel.cancel();
            });
            let failure = submit_to_server(
                &server,
                b"Subject: cancel\r\n\r\nbody",
                Some(cancellation),
                no_op_data_fence(),
                TEST_TIMEOUT,
            )
            .await
            .unwrap_err();
            assert_eq!(failure.stage, SmtpSubmissionStage::Connect);
            assert_eq!(failure.kind, SmtpSubmissionFailureKind::Cancelled);
            cancel_task.await.unwrap();
            server.task.abort();
            let _ = server.task.await;
        });
    }

    #[test]
    fn one_deadline_bounds_the_submission() {
        run_async(async {
            let server = spawn_server(
                TestSecurity::ImplicitTls,
                ServerScript::StallGreeting(None),
                None,
            )
            .await;
            let failure = submit_to_server(
                &server,
                b"Subject: timeout\r\n\r\nbody",
                None,
                no_op_data_fence(),
                Duration::from_millis(50),
            )
            .await
            .unwrap_err();
            assert_eq!(failure.stage, SmtpSubmissionStage::Connect);
            assert_eq!(failure.kind, SmtpSubmissionFailureKind::Timeout);
            server.task.abort();
            let _ = server.task.await;
        });
    }

    #[test]
    fn rejects_a_changed_or_oversized_mime_before_connecting() {
        let changed = private_mime_file(b"body");
        let failure = inspect_mime(&mut changed.try_clone().unwrap(), 3).unwrap_err();
        assert_eq!(failure.kind, SmtpSubmissionFailureKind::LocalFile);

        assert_eq!(
            SmtpSubmissionRequest::new(
                HOST,
                465,
                LOGIN,
                Secret::new(PASSWORD.to_vec()).unwrap(),
                SmtpSecurity::ImplicitTls,
                LOGIN,
                ["recipient@example.test"],
                changed,
                MAX_OUTBOUND_MIME_BYTES + 1,
            )
            .unwrap_err(),
            SmtpSubmissionInputError::WireByteCount
        );
    }
}
