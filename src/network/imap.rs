use std::{
    cell::RefCell,
    fmt,
    future::{Future, poll_fn},
    io,
    num::NonZeroU32,
    pin::Pin,
    sync::{Arc, OnceLock},
    task::{Context, Poll},
    time::Duration,
};

use async_imap::{
    Client, Session,
    error::Error as ImapError,
    imap_proto::types::{Response, Status},
};
use mail_parser::{HeaderName, MessageParser};
use mail_protocol_imap::{
    FetchEnvelope as ProtocolEnvelope, FetchFlag, FetchResponseItem, FetchSectionText,
    Response as ProtocolResponse, Status as ProtocolStatus, StatusKind, UntaggedData,
    parse_untagged,
};
use rustls::{ClientConfig, Error as RustlsError, crypto, pki_types::ServerName};
use rustls_platform_verifier::BuilderVerifierExt;
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::TcpStream,
    sync::oneshot,
    time::{Instant, timeout_at},
};
use tokio_rustls::{TlsConnector, client::TlsStream};

use crate::credentials::Secret;

mod fast;

#[cfg(test)]
use fast::open_protocol_session;
use fast::{
    ProtocolIdleOutcome, ProtocolMailbox, ProtocolSession, ProtocolTag, map_protocol_error,
    open_protocol_inbox_session,
};

const MAX_HOST_BYTES: usize = 253;
const MAX_LOGIN_BYTES: usize = 320;
const MAX_SERVER_BYTES: usize = 256 * 1024;
const MAX_CLIENT_BYTES: usize = 64 * 1024;
const DIAGNOSTIC_TIMEOUT: Duration = Duration::from_secs(30);
const LOGOUT_TIMEOUT: Duration = Duration::from_secs(1);
const INBOX_FETCH_TIMEOUT: Duration = Duration::from_secs(45);
const PARALLEL_SESSION_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_INBOX_MESSAGES: usize = 50;
const FIRST_SCREEN_MESSAGES: usize = 50;
const MAX_BODY_CONNECTIONS: usize = 2;
const MIN_PARALLEL_BODY_MESSAGES: usize = 8;
const MAX_METADATA_CONNECTIONS: usize = 10;
const MIN_PARALLEL_METADATA_MESSAGES: usize = 24;
const MAX_INBOX_LITERAL_BYTES: usize = 1024 * 1024;
const MAX_INBOX_PAGE_LITERAL_BYTES: usize = 20 * 1024 * 1024;
const MAX_INBOX_SERVER_BYTES: usize = MAX_INBOX_PAGE_LITERAL_BYTES + 512 * 1024;
const MAX_INBOX_CLIENT_BYTES: usize = 64 * 1024;
const MAX_ON_DEMAND_MESSAGE_BYTES: usize = 20 * 1024 * 1024;
const MAX_ON_DEMAND_SERVER_BYTES: usize = MAX_ON_DEMAND_MESSAGE_BYTES + 512 * 1024;
const MAX_INTERNAL_DATE_BYTES: usize = 64;
const MAX_METADATA_HEADER_BYTES: usize = 16 * 1024;
const MAX_ENVELOPE_DATE_BYTES: usize = 128;
const MAX_ENVELOPE_SUBJECT_BYTES: usize = 998;
const MAX_ENVELOPE_NAME_BYTES: usize = 320;
const MAX_ENVELOPE_MAILBOX_BYTES: usize = 320;
const MAX_ENVELOPE_HOST_BYTES: usize = 253;
const MAX_ENVELOPE_MESSAGE_ID_BYTES: usize = 998;
const MAX_CACHED_INBOX_SESSIONS: usize = 10;
const MAX_CACHED_INBOX_SESSIONS_PER_ACCOUNT: usize = MAX_METADATA_CONNECTIONS;
const CACHED_INBOX_SESSION_TTL: Duration = Duration::from_secs(6 * 60);
const CACHED_SESSION_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const IDLE_WAIT_TIMEOUT: Duration = Duration::from_secs(4 * 60 + 30);

static PLATFORM_CONFIG: OnceLock<Arc<ClientConfig>> = OnceLock::new();

#[cfg(feature = "bench-harness")]
static PROTOCOL_SESSION_OPENS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
#[cfg(feature = "bench-harness")]
static FOREGROUND_SESSION_REUSES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
#[cfg(feature = "bench-harness")]
static IDLE_CANCELLATIONS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(feature = "bench-harness")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ImapRuntimeCounters {
    pub(crate) protocol_session_opens: u64,
    pub(crate) foreground_session_reuses: u64,
    pub(crate) idle_cancellations: u64,
}

#[cfg(feature = "bench-harness")]
pub(crate) fn imap_runtime_counters() -> ImapRuntimeCounters {
    use std::sync::atomic::Ordering;

    ImapRuntimeCounters {
        protocol_session_opens: PROTOCOL_SESSION_OPENS.load(Ordering::Relaxed),
        foreground_session_reuses: FOREGROUND_SESSION_REUSES.load(Ordering::Relaxed),
        idle_cancellations: IDLE_CANCELLATIONS.load(Ordering::Relaxed),
    }
}

#[cfg(feature = "bench-harness")]
fn record_protocol_session_open() {
    PROTOCOL_SESSION_OPENS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

pub(crate) struct ImapDiagnosticRequest {
    host: Box<str>,
    port: u16,
    login: Box<str>,
    secret: Secret,
}

impl ImapDiagnosticRequest {
    pub(crate) fn new(
        host: &str,
        port: u16,
        login: &str,
        secret: Secret,
    ) -> Result<Self, ImapDiagnosticInputError> {
        validate_connection_input(host, port, login, &secret)?;
        Ok(Self {
            host: host.into(),
            port,
            login: login.into(),
            secret,
        })
    }
}

impl fmt::Debug for ImapDiagnosticRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ImapDiagnosticRequest([REDACTED])")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ImapDiagnosticInputError {
    Host,
    Port,
    Login,
    SecretEncoding,
}

pub(crate) struct ImapInboxFetchRequest {
    host: Box<str>,
    port: u16,
    login: Box<str>,
    secret: Secret,
    first_uid: NonZeroU32,
    expected_uid_validity: Option<NonZeroU32>,
    history_cursor: Option<NonZeroU32>,
    history_complete: bool,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ImapIdleRequest {
    host: Box<str>,
    port: u16,
    login: Box<str>,
    uid_validity: NonZeroU32,
}

impl ImapIdleRequest {
    pub(crate) fn new(
        host: &str,
        port: u16,
        login: &str,
        uid_validity: NonZeroU32,
    ) -> Result<Self, ImapDiagnosticInputError> {
        validate_endpoint(host, port, login)?;
        Ok(Self {
            host: host.into(),
            port,
            login: login.into(),
            uid_validity,
        })
    }
}

impl fmt::Debug for ImapIdleRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ImapIdleRequest([REDACTED])")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ImapIdleOutcome {
    Changed,
    TimedOut,
    Cancelled,
    Unavailable,
    Disconnected(ImapInboxFetchFailure),
}

impl ImapInboxFetchRequest {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        host: &str,
        port: u16,
        login: &str,
        secret: Secret,
        first_uid: u32,
        expected_uid_validity: Option<u32>,
        history_cursor: Option<u32>,
        history_complete: bool,
    ) -> Result<Self, ImapInboxFetchInputError> {
        validate_connection_input(host, port, login, &secret)
            .map_err(ImapInboxFetchInputError::Connection)?;
        let first_uid = NonZeroU32::new(first_uid).ok_or(ImapInboxFetchInputError::FirstUid)?;
        let expected_uid_validity = expected_uid_validity
            .map(|value| {
                NonZeroU32::new(value).ok_or(ImapInboxFetchInputError::ExpectedUidValidity)
            })
            .transpose()?;
        let history_cursor = history_cursor
            .map(|value| NonZeroU32::new(value).ok_or(ImapInboxFetchInputError::HistoryCursor))
            .transpose()?;
        if history_complete && history_cursor.is_some() {
            return Err(ImapInboxFetchInputError::HistoryCursor);
        }
        Ok(Self {
            host: host.into(),
            port,
            login: login.into(),
            secret,
            first_uid,
            expected_uid_validity,
            history_cursor,
            history_complete,
        })
    }
}

impl fmt::Debug for ImapInboxFetchRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ImapInboxFetchRequest([REDACTED])")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ImapInboxFetchInputError {
    Connection(ImapDiagnosticInputError),
    FirstUid,
    ExpectedUidValidity,
    HistoryCursor,
}

pub(crate) struct ImapMessageContentFetchRequest {
    host: Box<str>,
    port: u16,
    login: Box<str>,
    secret: Secret,
    uid_validity: NonZeroU32,
    uid: NonZeroU32,
}

impl ImapMessageContentFetchRequest {
    pub(crate) fn new(
        host: &str,
        port: u16,
        login: &str,
        secret: Secret,
        uid_validity: u32,
        uid: u32,
    ) -> Result<Self, ImapMessageContentFetchInputError> {
        validate_connection_input(host, port, login, &secret)
            .map_err(ImapMessageContentFetchInputError::Connection)?;
        Ok(Self {
            host: host.into(),
            port,
            login: login.into(),
            secret,
            uid_validity: NonZeroU32::new(uid_validity)
                .ok_or(ImapMessageContentFetchInputError::UidValidity)?,
            uid: NonZeroU32::new(uid).ok_or(ImapMessageContentFetchInputError::Uid)?,
        })
    }
}

impl fmt::Debug for ImapMessageContentFetchRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ImapMessageContentFetchRequest([REDACTED])")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ImapMessageContentFetchInputError {
    Connection(ImapDiagnosticInputError),
    UidValidity,
    Uid,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ImapInboxFlags {
    pub(crate) seen: bool,
    pub(crate) flagged: bool,
    pub(crate) answered: bool,
    pub(crate) draft: bool,
    pub(crate) deleted: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ImapInboxEnvelope {
    pub(crate) date: Box<[u8]>,
    pub(crate) subject: Box<[u8]>,
    pub(crate) from_name: Box<[u8]>,
    pub(crate) from_mailbox: Box<[u8]>,
    pub(crate) from_host: Box<[u8]>,
    pub(crate) message_id: Box<[u8]>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ImapInboxContent {
    NotFetched,
    Fetched(Box<[u8]>),
    Oversized { declared_bytes: u32 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ImapInboxMessage {
    pub(crate) uid: NonZeroU32,
    pub(crate) flags: ImapInboxFlags,
    pub(crate) internal_date: Box<str>,
    pub(crate) envelope: ImapInboxEnvelope,
    pub(crate) declared_bytes: u32,
    pub(crate) content: ImapInboxContent,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ImapInboxPage {
    pub(crate) uid_validity: NonZeroU32,
    pub(crate) uid_next: NonZeroU32,
    pub(crate) scanned_through_uid: Option<NonZeroU32>,
    pub(crate) next_uid: Option<NonZeroU32>,
    pub(crate) bootstrap_history: Option<ImapBootstrapHistory>,
    pub(crate) history_page: Option<ImapHistoryPage>,
    pub(crate) messages: Box<[ImapInboxMessage]>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ImapBootstrapHistory {
    pub(crate) next_cursor: Option<NonZeroU32>,
    pub(crate) complete: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ImapHistoryPage {
    pub(crate) expected_cursor: NonZeroU32,
    pub(crate) next_cursor: Option<NonZeroU32>,
    pub(crate) complete: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ImapInboxFetchFailure {
    Authentication,
    Permission,
    Certificate,
    Timeout,
    Offline,
    Protocol,
    ResourceLimit,
    Cancelled,
    MissingUidValidity,
    MissingUidNext,
    UidValidityChanged { expected: u32, actual: u32 },
}

impl fmt::Display for ImapInboxFetchFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Authentication => "the IMAP server rejected the account credentials",
            Self::Permission => "the account cannot read its IMAP inbox",
            Self::Certificate => "the IMAP server certificate could not be verified",
            Self::Timeout => "the IMAP inbox fetch timed out",
            Self::Offline => "the IMAP server connection was lost",
            Self::Protocol => "the server returned an invalid IMAP inbox response",
            Self::ResourceLimit => "the IMAP inbox fetch exceeded its resource limit",
            Self::Cancelled => "the IMAP inbox fetch was cancelled",
            Self::MissingUidValidity => "the IMAP inbox did not provide UIDVALIDITY",
            Self::MissingUidNext => "the IMAP inbox did not provide UIDNEXT",
            Self::UidValidityChanged { .. } => "the IMAP inbox UIDVALIDITY changed",
        })
    }
}

impl std::error::Error for ImapInboxFetchFailure {}

pub(crate) struct ImapInboxFetchCancelHandle(Option<oneshot::Sender<()>>);

impl ImapInboxFetchCancelHandle {
    pub(crate) fn cancel(mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

pub(crate) struct ImapInboxFetchCancellation(oneshot::Receiver<()>);

pub(crate) fn imap_inbox_fetch_cancellation_pair()
-> (ImapInboxFetchCancelHandle, ImapInboxFetchCancellation) {
    let (sender, receiver) = oneshot::channel();
    (
        ImapInboxFetchCancelHandle(Some(sender)),
        ImapInboxFetchCancellation(receiver),
    )
}

fn validate_connection_input(
    host: &str,
    port: u16,
    login: &str,
    secret: &Secret,
) -> Result<(), ImapDiagnosticInputError> {
    validate_endpoint(host, port, login)?;
    if std::str::from_utf8(secret.expose()).is_err() {
        return Err(ImapDiagnosticInputError::SecretEncoding);
    }
    Ok(())
}

fn validate_endpoint(host: &str, port: u16, login: &str) -> Result<(), ImapDiagnosticInputError> {
    if host.is_empty()
        || host.len() > MAX_HOST_BYTES
        || host.trim() != host
        || ServerName::try_from(host.to_owned()).is_err()
    {
        return Err(ImapDiagnosticInputError::Host);
    }
    if port == 0 {
        return Err(ImapDiagnosticInputError::Port);
    }
    if login.is_empty() || login.len() > MAX_LOGIN_BYTES || login.trim() != login {
        return Err(ImapDiagnosticInputError::Login);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ImapDiagnosticStage {
    Connect,
    Tls,
    Greeting,
    Capability,
    Authenticate,
    Mailbox,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ImapDiagnosticFailureKind {
    Authentication,
    Permission,
    Certificate,
    Timeout,
    Offline,
    Protocol,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ImapDiagnosticFailure {
    pub(crate) stage: ImapDiagnosticStage,
    pub(crate) kind: ImapDiagnosticFailureKind,
}

impl fmt::Display for ImapDiagnosticFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self.kind {
            ImapDiagnosticFailureKind::Authentication => {
                "the IMAP server rejected the account credentials"
            }
            ImapDiagnosticFailureKind::Permission => "the account cannot access its IMAP inbox",
            ImapDiagnosticFailureKind::Certificate => {
                "the IMAP server certificate could not be verified"
            }
            ImapDiagnosticFailureKind::Timeout => "the IMAP connection check timed out",
            ImapDiagnosticFailureKind::Offline => "the IMAP server could not be reached",
            ImapDiagnosticFailureKind::Protocol => {
                "the server did not complete a valid IMAP connection check"
            }
        })
    }
}

impl std::error::Error for ImapDiagnosticFailure {}

pub(crate) async fn diagnose_app_password(
    request: ImapDiagnosticRequest,
) -> Result<(), ImapDiagnosticFailure> {
    let connector = platform_connector()?;
    diagnose_with_connector(request, connector, DiagnosticLimits::production()).await
}

fn platform_connector() -> Result<TlsConnector, ImapDiagnosticFailure> {
    if let Some(config) = PLATFORM_CONFIG.get() {
        return Ok(TlsConnector::from(config.clone()));
    }

    let provider = Arc::new(crypto::ring::default_provider());
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .and_then(|builder| builder.with_platform_verifier())
        .map_err(|_| {
            failure(
                ImapDiagnosticStage::Tls,
                ImapDiagnosticFailureKind::Certificate,
            )
        })?
        .with_no_client_auth();
    let config = Arc::new(config);
    let _ = PLATFORM_CONFIG.set(config.clone());
    Ok(TlsConnector::from(
        PLATFORM_CONFIG.get().cloned().unwrap_or(config),
    ))
}

async fn diagnose_with_connector(
    request: ImapDiagnosticRequest,
    connector: TlsConnector,
    limits: DiagnosticLimits,
) -> Result<(), ImapDiagnosticFailure> {
    let deadline = Instant::now() + limits.timeout;
    let (mut session, login_capabilities) = open_app_password_session(
        request.host.as_ref(),
        request.port,
        request.login.as_ref(),
        &request.secret,
        connector,
        deadline,
        limits.max_server_bytes,
        limits.max_client_bytes,
    )
    .await?;
    require_imap_capability(&mut session, login_capabilities, deadline).await?;

    match timeout_at(deadline, session.examine("INBOX")).await {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => {
            return Err(failure(
                ImapDiagnosticStage::Mailbox,
                imap_failure_kind(&error, ImapDiagnosticStage::Mailbox),
            ));
        }
        Err(_) => return Err(timeout_failure(ImapDiagnosticStage::Mailbox)),
    }

    let _ = tokio::time::timeout(LOGOUT_TIMEOUT, session.logout()).await;
    Ok(())
}

type AuthenticatedSession = Session<BoundedIo<TlsStream<TcpStream>>>;

struct CachedInboxSession {
    host: Box<str>,
    port: u16,
    login: Box<str>,
    uid_validity: NonZeroU32,
    cached_at: Instant,
    session: ProtocolSession,
}

thread_local! {
    static CACHED_INBOX_SESSIONS: RefCell<Vec<CachedInboxSession>> = const {
        RefCell::new(Vec::new())
    };
}

fn take_cached_inbox_session(
    host: &str,
    port: u16,
    login: &str,
    uid_validity: NonZeroU32,
) -> Option<ProtocolSession> {
    let now = Instant::now();
    CACHED_INBOX_SESSIONS.with_borrow_mut(|sessions| {
        sessions.retain(|entry| {
            now.saturating_duration_since(entry.cached_at) <= CACHED_INBOX_SESSION_TTL
        });
        let index = sessions.iter().position(|entry| {
            entry.host.as_ref() == host
                && entry.port == port
                && entry.login.as_ref() == login
                && entry.uid_validity == uid_validity
        })?;
        Some(sessions.swap_remove(index).session)
    })
}

fn take_most_recent_cached_inbox_session(
    host: &str,
    port: u16,
    login: &str,
    uid_validity: NonZeroU32,
) -> Option<ProtocolSession> {
    let now = Instant::now();
    CACHED_INBOX_SESSIONS.with_borrow_mut(|sessions| {
        sessions.retain(|entry| {
            now.saturating_duration_since(entry.cached_at) <= CACHED_INBOX_SESSION_TTL
        });
        let index = sessions.iter().rposition(|entry| {
            entry.host.as_ref() == host
                && entry.port == port
                && entry.login.as_ref() == login
                && entry.uid_validity == uid_validity
        })?;
        Some(sessions.swap_remove(index).session)
    })
}

fn cache_inbox_session(
    host: &str,
    port: u16,
    login: &str,
    uid_validity: NonZeroU32,
    session: ProtocolSession,
) {
    let now = Instant::now();
    CACHED_INBOX_SESSIONS.with_borrow_mut(|sessions| {
        sessions.retain(|entry| {
            now.saturating_duration_since(entry.cached_at) <= CACHED_INBOX_SESSION_TTL
        });
        let matching = |entry: &CachedInboxSession| {
            entry.host.as_ref() == host
                && entry.port == port
                && entry.login.as_ref() == login
                && entry.uid_validity == uid_validity
        };
        if sessions.iter().filter(|entry| matching(entry)).count()
            >= MAX_CACHED_INBOX_SESSIONS_PER_ACCOUNT
            && let Some(oldest) = sessions
                .iter()
                .enumerate()
                .filter(|(_, entry)| matching(entry))
                .min_by_key(|(_, entry)| entry.cached_at)
                .map(|(index, _)| index)
        {
            sessions.swap_remove(oldest);
        }
        if sessions.len() >= MAX_CACHED_INBOX_SESSIONS {
            let oldest = sessions
                .iter()
                .enumerate()
                .min_by_key(|(_, entry)| entry.cached_at)
                .map(|(index, _)| index)
                .expect("a full session cache contains an entry");
            sessions.swap_remove(oldest);
        }
        sessions.push(CachedInboxSession {
            host: host.into(),
            port,
            login: login.into(),
            uid_validity,
            cached_at: now,
            session,
        });
    });
}

#[allow(clippy::too_many_arguments)]
async fn open_app_password_session(
    host: &str,
    port: u16,
    login: &str,
    secret: &Secret,
    connector: TlsConnector,
    deadline: Instant,
    max_server_bytes: usize,
    max_client_bytes: usize,
) -> Result<
    (
        AuthenticatedSession,
        Option<async_imap::types::Capabilities>,
    ),
    ImapDiagnosticFailure,
> {
    let server_name = ServerName::try_from(host.to_owned()).map_err(|_| {
        failure(
            ImapDiagnosticStage::Tls,
            ImapDiagnosticFailureKind::Protocol,
        )
    })?;
    let tcp = match timeout_at(deadline, TcpStream::connect((host, port))).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(error)) => {
            return Err(failure(
                ImapDiagnosticStage::Connect,
                io_failure_kind(&error),
            ));
        }
        Err(_) => return Err(timeout_failure(ImapDiagnosticStage::Connect)),
    };
    let _ = tcp.set_nodelay(true);
    let tls = match timeout_at(deadline, connector.connect(server_name, tcp)).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(error)) => {
            return Err(failure(ImapDiagnosticStage::Tls, tls_failure_kind(&error)));
        }
        Err(_) => return Err(timeout_failure(ImapDiagnosticStage::Tls)),
    };
    let mut client = Client::new(BoundedIo::new(tls, max_server_bytes, max_client_bytes));
    let greeting = match timeout_at(deadline, client.read_response()).await {
        Ok(Ok(Some(greeting))) => greeting,
        Ok(Ok(None)) => {
            return Err(failure(
                ImapDiagnosticStage::Greeting,
                ImapDiagnosticFailureKind::Offline,
            ));
        }
        Ok(Err(error)) => {
            return Err(failure(
                ImapDiagnosticStage::Greeting,
                io_failure_kind(&error),
            ));
        }
        Err(_) => return Err(timeout_failure(ImapDiagnosticStage::Greeting)),
    };
    if !matches!(
        greeting.parsed(),
        Response::Data {
            status: Status::Ok,
            ..
        }
    ) {
        return Err(failure(
            ImapDiagnosticStage::Greeting,
            ImapDiagnosticFailureKind::Protocol,
        ));
    }
    let password = std::str::from_utf8(secret.expose()).map_err(|_| {
        failure(
            ImapDiagnosticStage::Authenticate,
            ImapDiagnosticFailureKind::Authentication,
        )
    })?;
    match timeout_at(deadline, client.login_with_capabilities(login, password)).await {
        Ok(Ok(session)) => Ok(session),
        Ok(Err((error, _client))) => Err(failure(
            ImapDiagnosticStage::Authenticate,
            imap_failure_kind(&error, ImapDiagnosticStage::Authenticate),
        )),
        Err(_) => Err(timeout_failure(ImapDiagnosticStage::Authenticate)),
    }
}

async fn require_imap_capability(
    session: &mut AuthenticatedSession,
    login_capabilities: Option<async_imap::types::Capabilities>,
    deadline: Instant,
) -> Result<(), ImapDiagnosticFailure> {
    let capabilities = match login_capabilities {
        Some(capabilities) => capabilities,
        None => match timeout_at(deadline, session.capabilities()).await {
            Ok(Ok(capabilities)) => capabilities,
            Ok(Err(error)) => {
                return Err(failure(
                    ImapDiagnosticStage::Capability,
                    imap_failure_kind(&error, ImapDiagnosticStage::Capability),
                ));
            }
            Err(_) => return Err(timeout_failure(ImapDiagnosticStage::Capability)),
        },
    };
    if capabilities.has_str("IMAP4rev1") || capabilities.has_str("IMAP4rev2") {
        Ok(())
    } else {
        Err(failure(
            ImapDiagnosticStage::Capability,
            ImapDiagnosticFailureKind::Protocol,
        ))
    }
}

pub(crate) async fn fetch_canonical_inbox(
    request: ImapInboxFetchRequest,
) -> Result<ImapInboxPage, ImapInboxFetchFailure> {
    let connector = platform_connector().map_err(map_diagnostic_failure)?;
    fetch_canonical_inbox_with_connector(request, connector, InboxFetchLimits::production(), None)
        .await
}

pub(crate) async fn fetch_canonical_inbox_metadata(
    request: ImapInboxFetchRequest,
) -> Result<ImapInboxPage, ImapInboxFetchFailure> {
    let connector = platform_connector().map_err(map_diagnostic_failure)?;
    fetch_canonical_inbox_with_mode(
        request,
        connector,
        InboxFetchLimits::production(),
        None,
        InboxFetchMode::Metadata,
        true,
    )
    .await
}

pub(crate) async fn fetch_imap_message_content(
    request: ImapMessageContentFetchRequest,
) -> Result<Box<[u8]>, ImapInboxFetchFailure> {
    let connector = platform_connector().map_err(map_diagnostic_failure)?;
    fetch_imap_message_content_inner(request, connector, on_demand_limits(), true).await
}

pub(crate) async fn fetch_cached_imap_message_content(
    host: &str,
    port: u16,
    login: &str,
    uid_validity: u32,
    uid: u32,
) -> Result<Option<Box<[u8]>>, ImapInboxFetchFailure> {
    validate_endpoint(host, port, login).map_err(|_| ImapInboxFetchFailure::Protocol)?;
    let uid_validity = NonZeroU32::new(uid_validity).ok_or(ImapInboxFetchFailure::Protocol)?;
    let uid = NonZeroU32::new(uid).ok_or(ImapInboxFetchFailure::Protocol)?;
    let Some(mut session) = take_most_recent_cached_inbox_session(host, port, login, uid_validity)
    else {
        #[cfg(feature = "bench-harness")]
        eprintln!("NIVALIS_PERF body_cache miss");
        return Ok(None);
    };
    #[cfg(feature = "bench-harness")]
    FOREGROUND_SESSION_REUSES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let deadline = Instant::now() + INBOX_FETCH_TIMEOUT;
    #[cfg(feature = "bench-harness")]
    let body_started = Instant::now();
    let result = fetch_body(&mut session, uid, deadline, on_demand_limits()).await;
    #[cfg(feature = "bench-harness")]
    eprintln!(
        "NIVALIS_PERF body_cache result={:?} elapsed_ms={}",
        result.as_ref().map(|body| body.len()),
        body_started.elapsed().as_millis()
    );
    match result {
        Ok(content) => {
            cache_inbox_session(host, port, login, uid_validity, session);
            Ok(Some(content))
        }
        Err(failure) if recoverable_cached_session_failure(failure) => Ok(None),
        Err(failure) => Err(failure),
    }
}

pub(crate) async fn wait_for_cached_inbox_change(
    request: ImapIdleRequest,
    mut cancellation: oneshot::Receiver<()>,
) -> ImapIdleOutcome {
    let Some(mut session) = take_cached_inbox_session(
        request.host.as_ref(),
        request.port,
        request.login.as_ref(),
        request.uid_validity,
    ) else {
        return ImapIdleOutcome::Unavailable;
    };
    if !session.supports_idle() {
        cache_inbox_session(
            request.host.as_ref(),
            request.port,
            request.login.as_ref(),
            request.uid_validity,
            session,
        );
        return ImapIdleOutcome::Unavailable;
    }
    let outcome = session
        .idle_until_cancellable(Instant::now() + IDLE_WAIT_TIMEOUT, Some(&mut cancellation))
        .await;
    match outcome {
        Ok(outcome) => {
            cache_inbox_session(
                request.host.as_ref(),
                request.port,
                request.login.as_ref(),
                request.uid_validity,
                session,
            );
            match outcome {
                ProtocolIdleOutcome::Changed => ImapIdleOutcome::Changed,
                ProtocolIdleOutcome::TimedOut => ImapIdleOutcome::TimedOut,
                ProtocolIdleOutcome::Cancelled => {
                    #[cfg(feature = "bench-harness")]
                    IDLE_CANCELLATIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    ImapIdleOutcome::Cancelled
                }
            }
        }
        Err(failure) => ImapIdleOutcome::Disconnected(failure),
    }
}

#[cfg(any(test, feature = "bench-harness"))]
pub(crate) fn cached_inbox_session_resources() -> (usize, usize) {
    CACHED_INBOX_SESSIONS.with_borrow(|sessions| {
        (
            sessions.len(),
            sessions
                .iter()
                .map(|entry| entry.session.retained_plaintext_buffer_bytes())
                .sum(),
        )
    })
}

fn on_demand_limits() -> InboxFetchLimits {
    InboxFetchLimits {
        timeout: INBOX_FETCH_TIMEOUT,
        max_server_bytes: MAX_ON_DEMAND_SERVER_BYTES,
        max_client_bytes: MAX_INBOX_CLIENT_BYTES,
        max_messages: 1,
        max_message_literal_bytes: MAX_ON_DEMAND_MESSAGE_BYTES,
        max_page_literal_bytes: MAX_ON_DEMAND_MESSAGE_BYTES,
        body_connections: 1,
    }
}

async fn fetch_imap_message_content_with_connector(
    request: ImapMessageContentFetchRequest,
    connector: TlsConnector,
    limits: InboxFetchLimits,
) -> Result<Box<[u8]>, ImapInboxFetchFailure> {
    fetch_imap_message_content_inner(request, connector, limits, false).await
}

async fn fetch_imap_message_content_inner(
    request: ImapMessageContentFetchRequest,
    connector: TlsConnector,
    limits: InboxFetchLimits,
    reuse_cached_session: bool,
) -> Result<Box<[u8]>, ImapInboxFetchFailure> {
    let deadline = Instant::now() + limits.timeout;
    if reuse_cached_session
        && let Some(mut session) = take_most_recent_cached_inbox_session(
            request.host.as_ref(),
            request.port,
            request.login.as_ref(),
            request.uid_validity,
        )
    {
        #[cfg(feature = "bench-harness")]
        FOREGROUND_SESSION_REUSES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        match fetch_body(&mut session, request.uid, deadline, limits).await {
            Ok(content) => {
                cache_inbox_session(
                    request.host.as_ref(),
                    request.port,
                    request.login.as_ref(),
                    request.uid_validity,
                    session,
                );
                return Ok(content);
            }
            Err(
                ImapInboxFetchFailure::Offline
                | ImapInboxFetchFailure::Timeout
                | ImapInboxFetchFailure::Protocol
                | ImapInboxFetchFailure::ResourceLimit,
            ) => {}
            Err(failure) => return Err(failure),
        }
    }
    let (mut session, mailbox) = open_protocol_inbox_session(
        request.host.as_ref(),
        request.port,
        request.login.as_ref(),
        &request.secret,
        connector,
        deadline,
        limits,
    )
    .await?;
    let actual_uid_validity = mailbox
        .uid_validity
        .and_then(NonZeroU32::new)
        .ok_or(ImapInboxFetchFailure::MissingUidValidity)?;
    if actual_uid_validity != request.uid_validity {
        return Err(ImapInboxFetchFailure::UidValidityChanged {
            expected: request.uid_validity.get(),
            actual: actual_uid_validity.get(),
        });
    }
    let content = fetch_body(&mut session, request.uid, deadline, limits).await?;
    if reuse_cached_session {
        cache_inbox_session(
            request.host.as_ref(),
            request.port,
            request.login.as_ref(),
            request.uid_validity,
            session,
        );
    } else {
        session.logout(Instant::now() + LOGOUT_TIMEOUT).await;
    }
    Ok(content)
}

pub(crate) async fn fetch_canonical_inbox_cancellable(
    request: ImapInboxFetchRequest,
    cancellation: ImapInboxFetchCancellation,
) -> Result<ImapInboxPage, ImapInboxFetchFailure> {
    let connector = platform_connector().map_err(map_diagnostic_failure)?;
    fetch_canonical_inbox_with_connector(
        request,
        connector,
        InboxFetchLimits::production(),
        Some(cancellation),
    )
    .await
}

async fn fetch_canonical_inbox_with_connector(
    request: ImapInboxFetchRequest,
    connector: TlsConnector,
    limits: InboxFetchLimits,
    cancellation: Option<ImapInboxFetchCancellation>,
) -> Result<ImapInboxPage, ImapInboxFetchFailure> {
    fetch_canonical_inbox_with_mode(
        request,
        connector,
        limits,
        cancellation,
        InboxFetchMode::Complete,
        false,
    )
    .await
}

async fn fetch_canonical_inbox_with_mode(
    request: ImapInboxFetchRequest,
    connector: TlsConnector,
    limits: InboxFetchLimits,
    cancellation: Option<ImapInboxFetchCancellation>,
    mode: InboxFetchMode,
    cache_primary_session: bool,
) -> Result<ImapInboxPage, ImapInboxFetchFailure> {
    let operation =
        fetch_canonical_inbox_inner(request, connector, limits, mode, cache_primary_session);
    let Some(ImapInboxFetchCancellation(receiver)) = cancellation else {
        return operation.await;
    };
    let mut operation = Box::pin(operation);
    let mut receiver = Some(Box::pin(receiver));
    poll_fn(move |context| {
        if let Some(signal) = receiver.as_mut() {
            match signal.as_mut().poll(context) {
                Poll::Ready(Ok(())) => {
                    return Poll::Ready(Err(ImapInboxFetchFailure::Cancelled));
                }
                Poll::Ready(Err(_)) => receiver = None,
                Poll::Pending => {}
            }
        }
        operation.as_mut().poll(context)
    })
    .await
}

#[allow(clippy::too_many_arguments)]
async fn open_protocol_inbox_pool(
    host: &str,
    port: u16,
    login: &str,
    secret: &Secret,
    connector: TlsConnector,
    deadline: Instant,
    limits: InboxFetchLimits,
) -> Result<
    (
        (ProtocolSession, ProtocolMailbox),
        Vec<(ProtocolSession, ProtocolMailbox)>,
    ),
    ImapInboxFetchFailure,
> {
    #[cfg(feature = "bench-harness")]
    let pool_started = Instant::now();
    let secondary_deadline = deadline.min(Instant::now() + PARALLEL_SESSION_TIMEOUT);
    let mut secondary_opens: Vec<
        LocalBoxFuture<'_, Result<(ProtocolSession, ProtocolMailbox), ImapInboxFetchFailure>>,
    > = Vec::with_capacity(MAX_METADATA_CONNECTIONS - 1);
    for _ in 1..MAX_METADATA_CONNECTIONS {
        secondary_opens.push(Box::pin(open_protocol_inbox_session(
            host,
            port,
            login,
            secret,
            connector.clone(),
            secondary_deadline,
            limits,
        )));
    }
    let (primary, secondary) = join_pair(
        open_protocol_inbox_session(host, port, login, secret, connector, deadline, limits),
        join_all_local(secondary_opens),
    )
    .await;
    let secondary = secondary
        .into_iter()
        .filter_map(Result::ok)
        .collect::<Vec<_>>();
    #[cfg(feature = "bench-harness")]
    eprintln!(
        "NIVALIS_PERF metadata_pool_open primary={:?} secondary_ready={}",
        primary.as_ref().map(|_| 1),
        secondary.len(),
    );
    #[cfg(feature = "bench-harness")]
    eprintln!(
        "NIVALIS_PERF metadata_pool_open_elapsed_ms={}",
        pool_started.elapsed().as_millis()
    );
    Ok((primary?, secondary))
}

async fn fetch_canonical_inbox_inner(
    request: ImapInboxFetchRequest,
    connector: TlsConnector,
    limits: InboxFetchLimits,
    mode: InboxFetchMode,
    cache_primary_session: bool,
) -> Result<ImapInboxPage, ImapInboxFetchFailure> {
    let deadline = Instant::now() + limits.timeout;
    let body_connector = connector.clone();
    let should_prewarm_metadata_pool = cache_primary_session
        && mode == InboxFetchMode::Metadata
        && request.first_uid.get() == 1
        && limits.max_messages >= MIN_PARALLEL_METADATA_MESSAGES;
    let cached_session = if cache_primary_session {
        request.expected_uid_validity.and_then(|uid_validity| {
            take_cached_inbox_session(
                request.host.as_ref(),
                request.port,
                request.login.as_ref(),
                uid_validity,
            )
        })
    } else {
        None
    };
    let (mut session, mailbox, prewarmed_sessions) = if let Some(mut cached) = cached_session {
        let probe_deadline = deadline.min(Instant::now() + CACHED_SESSION_PROBE_TIMEOUT);
        match cached.examine_inbox(probe_deadline).await {
            Ok(mailbox) => (cached, mailbox, Vec::new()),
            Err(failure) if recoverable_cached_session_failure(failure) => {
                if should_prewarm_metadata_pool {
                    let ((fresh, mailbox), sessions) = open_protocol_inbox_pool(
                        request.host.as_ref(),
                        request.port,
                        request.login.as_ref(),
                        &request.secret,
                        connector,
                        deadline,
                        limits,
                    )
                    .await?;
                    (fresh, mailbox, sessions)
                } else {
                    let (fresh, mailbox) = open_protocol_inbox_session(
                        request.host.as_ref(),
                        request.port,
                        request.login.as_ref(),
                        &request.secret,
                        connector,
                        deadline,
                        limits,
                    )
                    .await?;
                    (fresh, mailbox, Vec::new())
                }
            }
            Err(failure) => return Err(failure),
        }
    } else if should_prewarm_metadata_pool {
        let ((fresh, mailbox), sessions) = open_protocol_inbox_pool(
            request.host.as_ref(),
            request.port,
            request.login.as_ref(),
            &request.secret,
            connector,
            deadline,
            limits,
        )
        .await?;
        (fresh, mailbox, sessions)
    } else {
        let (fresh, mailbox) = open_protocol_inbox_session(
            request.host.as_ref(),
            request.port,
            request.login.as_ref(),
            &request.secret,
            connector,
            deadline,
            limits,
        )
        .await?;
        (fresh, mailbox, Vec::new())
    };
    let uid_validity = mailbox
        .uid_validity
        .and_then(NonZeroU32::new)
        .ok_or(ImapInboxFetchFailure::MissingUidValidity)?;
    if let Some(expected) = request.expected_uid_validity
        && expected != uid_validity
    {
        return Err(ImapInboxFetchFailure::UidValidityChanged {
            expected: expected.get(),
            actual: uid_validity.get(),
        });
    }
    for (secondary, secondary_mailbox) in prewarmed_sessions {
        if secondary_mailbox.uid_validity.and_then(NonZeroU32::new) == Some(uid_validity) {
            cache_inbox_session(
                request.host.as_ref(),
                request.port,
                request.login.as_ref(),
                uid_validity,
                secondary,
            );
        }
    }
    let uid_next = mailbox
        .uid_next
        .and_then(NonZeroU32::new)
        .ok_or(ImapInboxFetchFailure::MissingUidNext)?;
    let first_uid = request.first_uid.get();
    let initial_snapshot = first_uid == 1;
    let selection_limits = if initial_snapshot {
        InboxFetchLimits {
            max_messages: limits.max_messages.min(FIRST_SCREEN_MESSAGES),
            ..limits
        }
    } else {
        limits
    };
    if mode == InboxFetchMode::Metadata && initial_snapshot && mailbox.exists > 0 {
        let mut metadata = fetch_latest_metadata_parallel(
            &mut session,
            mailbox.exists,
            request.host.as_ref(),
            request.port,
            request.login.as_ref(),
            &request.secret,
            body_connector,
            uid_validity,
            deadline,
            selection_limits,
        )
        .await?;
        metadata.sort_unstable_by_key(|message| message.uid);
        let has_history = usize::try_from(mailbox.exists)
            .map_err(|_| ImapInboxFetchFailure::ResourceLimit)?
            > metadata.len();
        let next_cursor = has_history
            .then(|| {
                metadata
                    .first()
                    .and_then(|message| NonZeroU32::new(message.uid.get() - 1))
            })
            .flatten();
        let messages = metadata
            .into_iter()
            .map(|metadata| ImapInboxMessage {
                uid: metadata.uid,
                flags: metadata.flags,
                internal_date: metadata.internal_date,
                envelope: metadata.envelope,
                declared_bytes: metadata.declared_bytes,
                content: ImapInboxContent::NotFetched,
            })
            .collect::<Vec<_>>();
        if cache_primary_session {
            cache_inbox_session(
                request.host.as_ref(),
                request.port,
                request.login.as_ref(),
                uid_validity,
                session,
            );
        } else {
            session.logout(Instant::now() + LOGOUT_TIMEOUT).await;
        }
        return Ok(ImapInboxPage {
            uid_validity,
            uid_next,
            scanned_through_uid: NonZeroU32::new(uid_next.get().saturating_sub(1)),
            next_uid: None,
            bootstrap_history: Some(ImapBootstrapHistory {
                next_cursor,
                complete: !has_history,
            }),
            history_page: None,
            messages: messages.into_boxed_slice(),
        });
    }
    let mut selection = if mailbox.exists == 0 || uid_next.get() <= first_uid {
        let scanned_through = first_uid
            .saturating_sub(1)
            .max(uid_next.get().saturating_sub(1));
        UidSelection {
            uids: Vec::new(),
            allow_missing_uids: false,
            scanned_through_uid: NonZeroU32::new(scanned_through),
            next_uid: None,
            bootstrap_history: initial_snapshot.then_some(ImapBootstrapHistory {
                next_cursor: None,
                complete: true,
            }),
            history_page: None,
        }
    } else if initial_snapshot {
        // A missing cursor always maps to UID 1, including retries after metadata
        // staging has persisted UIDVALIDITY but content publication did not finish.
        search_uids(
            &mut session,
            first_uid,
            uid_next,
            initial_snapshot,
            deadline,
            selection_limits,
        )
        .await?
    } else if usize::try_from(uid_next.get().saturating_sub(first_uid))
        .is_ok_and(|span| span <= selection_limits.max_messages)
    {
        select_forward_uid_window(first_uid, uid_next, selection_limits)?
    } else {
        // A wide or very sparse UID range still needs SEARCH to return a full
        // bounded page without asking the server for an unbounded FETCH.
        search_uids(
            &mut session,
            first_uid,
            uid_next,
            false,
            deadline,
            selection_limits,
        )
        .await?
    };
    if selection.uids.is_empty()
        && !initial_snapshot
        && !request.history_complete
        && let Some(history_cursor) = request.history_cursor
    {
        let expected_cursor =
            NonZeroU32::new(first_uid - 1).expect("non-initial inbox fetch has a forward cursor");
        selection = search_history_uids(
            &mut session,
            expected_cursor,
            history_cursor,
            deadline,
            limits,
        )
        .await?;
    }
    if selection.uids.is_empty() {
        if cache_primary_session {
            cache_inbox_session(
                request.host.as_ref(),
                request.port,
                request.login.as_ref(),
                uid_validity,
                session,
            );
        } else {
            session.logout(Instant::now() + LOGOUT_TIMEOUT).await;
        }
        return Ok(ImapInboxPage {
            uid_validity,
            uid_next,
            scanned_through_uid: selection.scanned_through_uid,
            next_uid: selection.next_uid,
            bootstrap_history: selection.bootstrap_history,
            history_page: selection.history_page,
            messages: Box::new([]),
        });
    }

    if mode == InboxFetchMode::Metadata {
        // This provider emits metadata at a visible per-message cadence. Split
        // full 50-row pages across a small, bounded set of selected sessions;
        // smaller daily increments stay on the already-hot primary connection.
        let mut metadata = fetch_metadata_parallel(
            &mut session,
            &selection.uids,
            selection.allow_missing_uids,
            request.host.as_ref(),
            request.port,
            request.login.as_ref(),
            &request.secret,
            body_connector,
            uid_validity,
            deadline,
            limits,
        )
        .await?;
        metadata.sort_unstable_by_key(|message| message.uid);
        if selection.history_page.is_some() {
            metadata.reverse();
        }
        let messages = metadata
            .into_iter()
            .map(|metadata| ImapInboxMessage {
                uid: metadata.uid,
                flags: metadata.flags,
                internal_date: metadata.internal_date,
                envelope: metadata.envelope,
                declared_bytes: metadata.declared_bytes,
                content: ImapInboxContent::NotFetched,
            })
            .collect::<Vec<_>>();
        if cache_primary_session {
            cache_inbox_session(
                request.host.as_ref(),
                request.port,
                request.login.as_ref(),
                uid_validity,
                session,
            );
        } else {
            session.logout(Instant::now() + LOGOUT_TIMEOUT).await;
        }
        return Ok(ImapInboxPage {
            uid_validity,
            uid_next,
            scanned_through_uid: selection.scanned_through_uid,
            next_uid: selection.next_uid,
            bootstrap_history: selection.bootstrap_history,
            history_page: selection.history_page,
            messages: messages.into_boxed_slice(),
        });
    }

    let (mut metadata, mut secondary_session) =
        if limits.body_connections > 1 && selection.uids.len() >= MIN_PARALLEL_BODY_MESSAGES {
            let secondary_deadline = deadline.min(Instant::now() + PARALLEL_SESSION_TIMEOUT);
            let metadata_fetch = fetch_metadata(
                &mut session,
                &selection.uids,
                selection.allow_missing_uids,
                deadline,
                limits,
            );
            let secondary_open = open_inbox_body_session(
                request.host.as_ref(),
                request.port,
                request.login.as_ref(),
                &request.secret,
                body_connector,
                uid_validity,
                secondary_deadline,
                limits,
            );
            let (metadata, secondary) = join_pair(metadata_fetch, secondary_open).await;
            (metadata?, secondary.ok())
        } else {
            (
                fetch_metadata(
                    &mut session,
                    &selection.uids,
                    selection.allow_missing_uids,
                    deadline,
                    limits,
                )
                .await?,
                None,
            )
        };
    metadata.sort_unstable_by_key(|message| message.uid);
    if selection.history_page.is_some() {
        metadata.reverse();
    }

    let fetched = fetch_message_contents(
        &mut session,
        secondary_session.as_mut(),
        metadata,
        limits.max_page_literal_bytes,
        deadline,
        limits,
    )
    .await?;
    if let Some(secondary) = secondary_session {
        secondary.logout(Instant::now() + LOGOUT_TIMEOUT).await;
    }
    let messages = fetched.messages.into_vec();
    let deferred_uid = fetched.deferred_uid;
    let (scanned_through_uid, next_uid, history_page) =
        if let Some(mut history) = selection.history_page {
            if deferred_uid.is_some() {
                history.next_cursor = messages
                    .iter()
                    .map(|message| message.uid.get())
                    .min()
                    .and_then(|uid| NonZeroU32::new(uid - 1));
                history.complete = false;
            }
            (selection.scanned_through_uid, None, Some(history))
        } else {
            (
                deferred_uid
                    .and_then(|uid| uid.get().checked_sub(1))
                    .and_then(NonZeroU32::new)
                    .or(selection.scanned_through_uid),
                deferred_uid.or(selection.next_uid),
                None,
            )
        };
    session.logout(Instant::now() + LOGOUT_TIMEOUT).await;
    Ok(ImapInboxPage {
        uid_validity,
        uid_next,
        scanned_through_uid,
        next_uid,
        bootstrap_history: selection.bootstrap_history,
        history_page,
        messages: messages.into_boxed_slice(),
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InboxFetchMode {
    Metadata,
    Complete,
}

#[derive(Clone, Copy)]
struct InboxFetchLimits {
    timeout: Duration,
    max_server_bytes: usize,
    max_client_bytes: usize,
    max_messages: usize,
    max_message_literal_bytes: usize,
    max_page_literal_bytes: usize,
    body_connections: usize,
}

impl InboxFetchLimits {
    const fn production() -> Self {
        Self {
            timeout: INBOX_FETCH_TIMEOUT,
            max_server_bytes: MAX_INBOX_SERVER_BYTES,
            max_client_bytes: MAX_INBOX_CLIENT_BYTES,
            max_messages: MAX_INBOX_MESSAGES,
            max_message_literal_bytes: MAX_INBOX_LITERAL_BYTES,
            max_page_literal_bytes: MAX_INBOX_PAGE_LITERAL_BYTES,
            body_connections: MAX_BODY_CONNECTIONS,
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn open_inbox_body_session(
    host: &str,
    port: u16,
    login: &str,
    secret: &Secret,
    connector: TlsConnector,
    expected_uid_validity: NonZeroU32,
    deadline: Instant,
    limits: InboxFetchLimits,
) -> Result<ProtocolSession, ImapInboxFetchFailure> {
    let (session, mailbox) =
        open_protocol_inbox_session(host, port, login, secret, connector, deadline, limits).await?;
    let actual_uid_validity = mailbox
        .uid_validity
        .and_then(NonZeroU32::new)
        .ok_or(ImapInboxFetchFailure::MissingUidValidity)?;
    if actual_uid_validity != expected_uid_validity {
        return Err(ImapInboxFetchFailure::UidValidityChanged {
            expected: expected_uid_validity.get(),
            actual: actual_uid_validity.get(),
        });
    }
    Ok(session)
}

struct InboxMetadata {
    uid: NonZeroU32,
    flags: ImapInboxFlags,
    internal_date: Box<str>,
    envelope: ImapInboxEnvelope,
    declared_bytes: u32,
}

struct UidSelection {
    uids: Vec<NonZeroU32>,
    allow_missing_uids: bool,
    scanned_through_uid: Option<NonZeroU32>,
    next_uid: Option<NonZeroU32>,
    bootstrap_history: Option<ImapBootstrapHistory>,
    history_page: Option<ImapHistoryPage>,
}

async fn search_uids(
    session: &mut ProtocolSession,
    first_uid: u32,
    uid_next: NonZeroU32,
    initial_snapshot: bool,
    deadline: Instant,
    limits: InboxFetchLimits,
) -> Result<UidSelection, ImapInboxFetchFailure> {
    if limits.max_messages == 0 {
        return Err(ImapInboxFetchFailure::ResourceLimit);
    }
    let found = session
        .uid_search(format!("UID {first_uid}:*"), deadline)
        .await?;
    let mut found = found
        .into_iter()
        .filter(|uid| (first_uid..uid_next.get()).contains(uid))
        .filter_map(NonZeroU32::new)
        .collect::<Vec<_>>();
    found.sort_unstable();

    let initial_has_history = initial_snapshot && found.len() > limits.max_messages;
    if initial_has_history {
        found.drain(..found.len() - limits.max_messages);
    }
    let next_uid = (!initial_snapshot && found.len() > limits.max_messages)
        .then(|| found[limits.max_messages]);
    found.truncate(limits.max_messages);
    let scanned_through_uid = next_uid
        .and_then(|_| found.last().copied())
        .or_else(|| NonZeroU32::new(uid_next.get().saturating_sub(1)));
    let bootstrap_history = initial_snapshot.then(|| ImapBootstrapHistory {
        next_cursor: initial_has_history
            .then(|| found.first().and_then(|uid| NonZeroU32::new(uid.get() - 1)))
            .flatten(),
        complete: !initial_has_history,
    });
    Ok(UidSelection {
        uids: found,
        allow_missing_uids: false,
        scanned_through_uid,
        next_uid,
        bootstrap_history,
        history_page: None,
    })
}

fn select_forward_uid_window(
    first_uid: u32,
    uid_next: NonZeroU32,
    limits: InboxFetchLimits,
) -> Result<UidSelection, ImapInboxFetchFailure> {
    let maximum =
        u32::try_from(limits.max_messages).map_err(|_| ImapInboxFetchFailure::ResourceLimit)?;
    if maximum == 0 {
        return Err(ImapInboxFetchFailure::ResourceLimit);
    }
    let Some(last_assigned_uid) = uid_next.get().checked_sub(1) else {
        return Err(ImapInboxFetchFailure::Protocol);
    };
    if first_uid > last_assigned_uid {
        return Ok(UidSelection {
            uids: Vec::new(),
            allow_missing_uids: true,
            scanned_through_uid: NonZeroU32::new(last_assigned_uid),
            next_uid: None,
            bootstrap_history: None,
            history_page: None,
        });
    }
    let window_end = first_uid
        .saturating_add(maximum.saturating_sub(1))
        .min(last_assigned_uid);
    let uids = (first_uid..=window_end)
        .filter_map(NonZeroU32::new)
        .collect::<Vec<_>>();
    Ok(UidSelection {
        uids,
        // UIDNEXT makes this numeric window safe even when expunged messages left
        // gaps: the server will never allocate one of these skipped UIDs later.
        allow_missing_uids: true,
        scanned_through_uid: NonZeroU32::new(window_end),
        next_uid: (window_end < last_assigned_uid)
            .then(|| NonZeroU32::new(window_end.saturating_add(1)))
            .flatten(),
        bootstrap_history: None,
        history_page: None,
    })
}

async fn search_history_uids(
    session: &mut ProtocolSession,
    expected_cursor: NonZeroU32,
    history_cursor: NonZeroU32,
    deadline: Instant,
    limits: InboxFetchLimits,
) -> Result<UidSelection, ImapInboxFetchFailure> {
    if limits.max_messages == 0 {
        return Err(ImapInboxFetchFailure::ResourceLimit);
    }
    let found = session
        .uid_search(format!("UID 1:{}", history_cursor.get()), deadline)
        .await?;
    let mut found = found
        .into_iter()
        .filter(|uid| (1..=history_cursor.get()).contains(uid))
        .filter_map(NonZeroU32::new)
        .collect::<Vec<_>>();
    found.sort_unstable();
    let has_more = found.len() > limits.max_messages;
    if has_more {
        found.drain(..found.len() - limits.max_messages);
    }
    let next_cursor = has_more
        .then(|| found.first().and_then(|uid| NonZeroU32::new(uid.get() - 1)))
        .flatten();
    Ok(UidSelection {
        uids: found,
        allow_missing_uids: false,
        scanned_through_uid: Some(expected_cursor),
        next_uid: None,
        bootstrap_history: None,
        history_page: Some(ImapHistoryPage {
            expected_cursor: history_cursor,
            next_cursor,
            complete: !has_more,
        }),
    })
}

async fn fetch_metadata(
    session: &mut ProtocolSession,
    uids: &[NonZeroU32],
    allow_missing_uids: bool,
    deadline: Instant,
    limits: InboxFetchLimits,
) -> Result<Vec<InboxMetadata>, ImapInboxFetchFailure> {
    let uid_set = uids
        .iter()
        .map(|uid| uid.get())
        .map(|uid| uid.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let command = format!(
        "UID FETCH {uid_set} (UID FLAGS INTERNALDATE RFC822.SIZE BODY.PEEK[HEADER.FIELDS (DATE FROM SUBJECT MESSAGE-ID)])"
    );
    let id = session.run_command(command, deadline).await?;
    let mut messages = Vec::with_capacity(limits.max_messages);
    loop {
        let response = read_receive_response(session, deadline).await?;
        match response {
            ReceiveResponse::Done { tag, status } if tag == id => {
                require_ok_status(status)?;
                break;
            }
            ReceiveResponse::Done { .. } => return Err(ImapInboxFetchFailure::Protocol),
            ReceiveResponse::Fetch(fetch) => {
                if messages.len() == limits.max_messages {
                    return Err(ImapInboxFetchFailure::ResourceLimit);
                }
                let message = parse_metadata(fetch)?;
                if !uids.contains(&message.uid)
                    || messages
                        .iter()
                        .any(|existing: &InboxMetadata| existing.uid == message.uid)
                {
                    return Err(ImapInboxFetchFailure::Protocol);
                }
                messages.push(message);
            }
            ReceiveResponse::Bye => return Err(ImapInboxFetchFailure::Offline),
            ReceiveResponse::Other => {}
        }
    }
    if !allow_missing_uids && messages.len() != uids.len() {
        return Err(ImapInboxFetchFailure::Protocol);
    }
    Ok(messages)
}

#[allow(clippy::too_many_arguments)]
async fn fetch_metadata_parallel(
    primary: &mut ProtocolSession,
    uids: &[NonZeroU32],
    allow_missing_uids: bool,
    host: &str,
    port: u16,
    login: &str,
    secret: &Secret,
    connector: TlsConnector,
    uid_validity: NonZeroU32,
    deadline: Instant,
    limits: InboxFetchLimits,
) -> Result<Vec<InboxMetadata>, ImapInboxFetchFailure> {
    if uids.len() < MIN_PARALLEL_METADATA_MESSAGES {
        return fetch_metadata(primary, uids, allow_missing_uids, deadline, limits).await;
    }
    let chunk = uids.len().div_ceil(MAX_METADATA_CONNECTIONS);
    let partitions = uids.chunks(chunk).collect::<Vec<_>>();
    let primary_uids = partitions[0];
    let secondary_deadline = deadline.min(Instant::now() + PARALLEL_SESSION_TIMEOUT);
    #[cfg(feature = "bench-harness")]
    let fetch_started = Instant::now();
    let mut futures: Vec<LocalBoxFuture<'_, Result<Vec<InboxMetadata>, ImapInboxFetchFailure>>> =
        Vec::with_capacity(partitions.len());
    futures.push(Box::pin(fetch_metadata(
        primary,
        primary_uids,
        allow_missing_uids,
        deadline,
        limits,
    )));
    for partition in &partitions[1..] {
        futures.push(Box::pin(fetch_metadata_on_new_session(
            host,
            port,
            login,
            secret,
            connector.clone(),
            uid_validity,
            partition,
            allow_missing_uids,
            secondary_deadline,
            deadline,
            limits,
        )));
    }
    let mut results = join_all_local(futures).await.into_iter();
    let primary_result = results
        .next()
        .expect("a parallel metadata request includes its primary");
    let secondary_results = results.collect::<Vec<_>>();
    #[cfg(feature = "bench-harness")]
    eprintln!(
        "NIVALIS_PERF metadata_parallel mode=uid lanes={} primary={:?} secondary={:?}",
        partitions.len(),
        primary_result.as_ref().map(Vec::len),
        secondary_results
            .iter()
            .map(|result| result.as_ref().map(Vec::len))
            .collect::<Vec<_>>(),
    );
    #[cfg(feature = "bench-harness")]
    eprintln!(
        "NIVALIS_PERF metadata_parallel_elapsed_ms={}",
        fetch_started.elapsed().as_millis()
    );
    let mut metadata = primary_result?;
    for (partition, result) in partitions[1..].iter().zip(secondary_results) {
        match result {
            Ok(messages) => metadata.extend(messages),
            Err(_) => {
                let fallback =
                    fetch_metadata(primary, partition, allow_missing_uids, deadline, limits).await;
                #[cfg(feature = "bench-harness")]
                eprintln!(
                    "NIVALIS_PERF metadata_parallel_fallback mode=uid result={:?}",
                    fallback.as_ref().map(Vec::len)
                );
                metadata.extend(fallback?);
            }
        }
    }
    Ok(metadata)
}

#[allow(clippy::too_many_arguments)]
async fn fetch_metadata_on_new_session(
    host: &str,
    port: u16,
    login: &str,
    secret: &Secret,
    connector: TlsConnector,
    uid_validity: NonZeroU32,
    uids: &[NonZeroU32],
    allow_missing_uids: bool,
    open_deadline: Instant,
    operation_deadline: Instant,
    limits: InboxFetchLimits,
) -> Result<Vec<InboxMetadata>, ImapInboxFetchFailure> {
    let mut session = match take_cached_inbox_session(host, port, login, uid_validity) {
        Some(session) => session,
        None => {
            open_inbox_body_session(
                host,
                port,
                login,
                secret,
                connector,
                uid_validity,
                open_deadline,
                limits,
            )
            .await?
        }
    };
    let messages = fetch_metadata(
        &mut session,
        uids,
        allow_missing_uids,
        operation_deadline,
        limits,
    )
    .await?;
    cache_inbox_session(host, port, login, uid_validity, session);
    Ok(messages)
}

async fn fetch_latest_metadata_by_sequence(
    session: &mut ProtocolSession,
    exists: u32,
    deadline: Instant,
    limits: InboxFetchLimits,
) -> Result<Vec<InboxMetadata>, ImapInboxFetchFailure> {
    let maximum =
        u32::try_from(limits.max_messages).map_err(|_| ImapInboxFetchFailure::ResourceLimit)?;
    if maximum == 0 {
        return Err(ImapInboxFetchFailure::ResourceLimit);
    }
    let expected = exists.min(maximum);
    let first_sequence = exists - expected + 1;
    fetch_metadata_by_sequence_range(session, first_sequence, exists, deadline, limits).await
}

async fn fetch_metadata_by_sequence_range(
    session: &mut ProtocolSession,
    first_sequence: u32,
    last_sequence: u32,
    deadline: Instant,
    limits: InboxFetchLimits,
) -> Result<Vec<InboxMetadata>, ImapInboxFetchFailure> {
    let expected = last_sequence
        .checked_sub(first_sequence)
        .and_then(|difference| difference.checked_add(1))
        .ok_or(ImapInboxFetchFailure::Protocol)?;
    let command = format!(
        "FETCH {first_sequence}:{last_sequence} (UID FLAGS INTERNALDATE RFC822.SIZE BODY.PEEK[HEADER.FIELDS (DATE FROM SUBJECT MESSAGE-ID)])"
    );
    let id = session.run_command(command, deadline).await?;
    let mut messages = Vec::with_capacity(
        usize::try_from(expected).map_err(|_| ImapInboxFetchFailure::ResourceLimit)?,
    );
    loop {
        let response = read_receive_response(session, deadline).await?;
        match response {
            ReceiveResponse::Done { tag, status } if tag == id => {
                require_ok_status(status)?;
                break;
            }
            ReceiveResponse::Done { .. } => return Err(ImapInboxFetchFailure::Protocol),
            ReceiveResponse::Fetch(fetch) => {
                if messages.len() == limits.max_messages {
                    return Err(ImapInboxFetchFailure::ResourceLimit);
                }
                let message = parse_metadata(fetch)?;
                if messages
                    .iter()
                    .any(|existing: &InboxMetadata| existing.uid == message.uid)
                {
                    return Err(ImapInboxFetchFailure::Protocol);
                }
                messages.push(message);
            }
            ReceiveResponse::Bye => return Err(ImapInboxFetchFailure::Offline),
            ReceiveResponse::Other => {}
        }
    }
    if messages.len()
        != usize::try_from(expected).map_err(|_| ImapInboxFetchFailure::ResourceLimit)?
    {
        return Err(ImapInboxFetchFailure::Protocol);
    }
    Ok(messages)
}

#[derive(Clone, Copy)]
struct SequenceRange {
    first: u32,
    last: u32,
}

#[allow(clippy::too_many_arguments)]
async fn fetch_latest_metadata_parallel(
    primary: &mut ProtocolSession,
    exists: u32,
    host: &str,
    port: u16,
    login: &str,
    secret: &Secret,
    connector: TlsConnector,
    uid_validity: NonZeroU32,
    deadline: Instant,
    limits: InboxFetchLimits,
) -> Result<Vec<InboxMetadata>, ImapInboxFetchFailure> {
    let maximum =
        u32::try_from(limits.max_messages).map_err(|_| ImapInboxFetchFailure::ResourceLimit)?;
    let expected = exists.min(maximum);
    if usize::try_from(expected).map_err(|_| ImapInboxFetchFailure::ResourceLimit)?
        < MIN_PARALLEL_METADATA_MESSAGES
    {
        return fetch_latest_metadata_by_sequence(primary, exists, deadline, limits).await;
    }
    let first = exists - expected + 1;
    let ranges = split_sequence_ranges(first, exists, MAX_METADATA_CONNECTIONS);
    let secondary_deadline = deadline.min(Instant::now() + PARALLEL_SESSION_TIMEOUT);
    #[cfg(feature = "bench-harness")]
    let fetch_started = Instant::now();
    let mut futures: Vec<LocalBoxFuture<'_, Result<Vec<InboxMetadata>, ImapInboxFetchFailure>>> =
        Vec::with_capacity(ranges.len());
    futures.push(Box::pin(fetch_metadata_by_sequence_range(
        primary,
        ranges[0].first,
        ranges[0].last,
        deadline,
        limits,
    )));
    for range in &ranges[1..] {
        futures.push(Box::pin(fetch_sequence_metadata_on_new_session(
            host,
            port,
            login,
            secret,
            connector.clone(),
            uid_validity,
            *range,
            secondary_deadline,
            deadline,
            limits,
        )));
    }
    let mut results = join_all_local(futures).await.into_iter();
    let primary_result = results
        .next()
        .expect("a parallel metadata request includes its primary");
    let secondary_results = results.collect::<Vec<_>>();
    #[cfg(feature = "bench-harness")]
    eprintln!(
        "NIVALIS_PERF metadata_parallel mode=sequence lanes={} primary={:?} secondary={:?}",
        ranges.len(),
        primary_result.as_ref().map(Vec::len),
        secondary_results
            .iter()
            .map(|result| result.as_ref().map(Vec::len))
            .collect::<Vec<_>>(),
    );
    #[cfg(feature = "bench-harness")]
    eprintln!(
        "NIVALIS_PERF metadata_parallel_elapsed_ms={}",
        fetch_started.elapsed().as_millis()
    );
    let mut metadata = primary_result?;
    for (range, result) in ranges[1..].iter().zip(secondary_results) {
        match result {
            Ok(messages) => metadata.extend(messages),
            Err(_) => {
                let fallback = fetch_metadata_by_sequence_range(
                    primary,
                    range.first,
                    range.last,
                    deadline,
                    limits,
                )
                .await;
                #[cfg(feature = "bench-harness")]
                eprintln!(
                    "NIVALIS_PERF metadata_parallel_fallback mode=sequence range={}:{} result={:?}",
                    range.first,
                    range.last,
                    fallback.as_ref().map(Vec::len)
                );
                metadata.extend(fallback?);
            }
        }
    }
    Ok(metadata)
}

fn split_sequence_ranges(first: u32, last: u32, maximum: usize) -> Vec<SequenceRange> {
    let count = last - first + 1;
    let lanes = usize::try_from(count).unwrap_or(usize::MAX).min(maximum);
    let lanes_u32 = u32::try_from(lanes).expect("metadata lane count fits u32");
    let base = count / lanes_u32;
    let remainder = count % lanes_u32;
    let mut next = first;
    (0..lanes)
        .map(|index| {
            let length = base + u32::from(index < usize::try_from(remainder).unwrap_or(0));
            let range = SequenceRange {
                first: next,
                last: next + length - 1,
            };
            next += length;
            range
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
async fn fetch_sequence_metadata_on_new_session(
    host: &str,
    port: u16,
    login: &str,
    secret: &Secret,
    connector: TlsConnector,
    uid_validity: NonZeroU32,
    range: SequenceRange,
    open_deadline: Instant,
    operation_deadline: Instant,
    limits: InboxFetchLimits,
) -> Result<Vec<InboxMetadata>, ImapInboxFetchFailure> {
    let mut session = match take_cached_inbox_session(host, port, login, uid_validity) {
        Some(session) => session,
        None => {
            open_inbox_body_session(
                host,
                port,
                login,
                secret,
                connector,
                uid_validity,
                open_deadline,
                limits,
            )
            .await?
        }
    };
    let messages = fetch_metadata_by_sequence_range(
        &mut session,
        range.first,
        range.last,
        operation_deadline,
        limits,
    )
    .await?;
    cache_inbox_session(host, port, login, uid_validity, session);
    Ok(messages)
}

struct ContentFetchPage {
    messages: Box<[ImapInboxMessage]>,
    deferred_uid: Option<NonZeroU32>,
}

async fn fetch_message_contents(
    session: &mut ProtocolSession,
    secondary_session: Option<&mut ProtocolSession>,
    metadata: Vec<InboxMetadata>,
    maximum_literal_bytes: usize,
    deadline: Instant,
    limits: InboxFetchLimits,
) -> Result<ContentFetchPage, ImapInboxFetchFailure> {
    let mut included = Vec::with_capacity(metadata.len());
    let mut declared_total = 0_usize;
    let mut deferred_uid = None;
    for message in metadata {
        let declared_bytes = usize::try_from(message.declared_bytes)
            .map_err(|_| ImapInboxFetchFailure::ResourceLimit)?;
        if declared_bytes <= limits.max_message_literal_bytes {
            let next_total = declared_total
                .checked_add(declared_bytes)
                .ok_or(ImapInboxFetchFailure::ResourceLimit)?;
            if next_total > maximum_literal_bytes {
                deferred_uid = Some(message.uid);
                break;
            }
            declared_total = next_total;
        }
        included.push(message);
    }

    let mut body_work = Vec::with_capacity(included.len());
    for message in &included {
        let declared_bytes = usize::try_from(message.declared_bytes)
            .map_err(|_| ImapInboxFetchFailure::ResourceLimit)?;
        if declared_bytes <= limits.max_message_literal_bytes {
            body_work.push((message.uid, declared_bytes));
        }
    }
    let mut bodies = if body_work.is_empty() {
        Vec::new()
    } else if let Some(secondary) = secondary_session
        && body_work.len() >= MIN_PARALLEL_BODY_MESSAGES
    {
        fetch_bodies_parallel(session, secondary, &body_work, deadline, limits).await?
    } else {
        let uids = body_work.iter().map(|(uid, _)| *uid).collect::<Vec<_>>();
        fetch_bodies(session, &uids, deadline, limits).await?
    };

    let mut actual_total = 0_usize;
    let mut messages = Vec::with_capacity(included.len());
    for metadata in included {
        let declared_bytes = usize::try_from(metadata.declared_bytes)
            .map_err(|_| ImapInboxFetchFailure::ResourceLimit)?;
        let content = if declared_bytes > limits.max_message_literal_bytes {
            ImapInboxContent::Oversized {
                declared_bytes: metadata.declared_bytes,
            }
        } else {
            let index = bodies
                .iter()
                .position(|body| body.uid == metadata.uid)
                .ok_or(ImapInboxFetchFailure::Protocol)?;
            let body = bodies.swap_remove(index);
            actual_total = actual_total
                .checked_add(body.bytes.len())
                .ok_or(ImapInboxFetchFailure::ResourceLimit)?;
            if actual_total > maximum_literal_bytes {
                return Err(ImapInboxFetchFailure::ResourceLimit);
            }
            ImapInboxContent::Fetched(body.bytes)
        };
        messages.push(ImapInboxMessage {
            uid: metadata.uid,
            flags: metadata.flags,
            internal_date: metadata.internal_date,
            envelope: metadata.envelope,
            declared_bytes: metadata.declared_bytes,
            content,
        });
    }
    if !bodies.is_empty() {
        return Err(ImapInboxFetchFailure::Protocol);
    }
    Ok(ContentFetchPage {
        messages: messages.into_boxed_slice(),
        deferred_uid,
    })
}

async fn fetch_bodies_parallel(
    primary: &mut ProtocolSession,
    secondary: &mut ProtocolSession,
    work: &[(NonZeroU32, usize)],
    deadline: Instant,
    limits: InboxFetchLimits,
) -> Result<Vec<ProjectedBody>, ImapInboxFetchFailure> {
    let (primary_uids, secondary_uids) = balanced_body_partitions(work);
    let (primary_result, secondary_result) = join_pair(
        fetch_bodies(primary, &primary_uids, deadline, limits),
        fetch_bodies(secondary, &secondary_uids, deadline, limits),
    )
    .await;
    match (primary_result, secondary_result) {
        (Ok(mut primary_bodies), Ok(secondary_bodies)) => {
            primary_bodies.extend(secondary_bodies);
            Ok(primary_bodies)
        }
        (Ok(mut primary_bodies), Err(_)) => {
            primary_bodies.extend(fetch_bodies(primary, &secondary_uids, deadline, limits).await?);
            Ok(primary_bodies)
        }
        (Err(_), Ok(mut secondary_bodies)) => {
            secondary_bodies
                .extend(fetch_bodies(secondary, &primary_uids, deadline, limits).await?);
            Ok(secondary_bodies)
        }
        (Err(primary_failure), Err(_)) => Err(primary_failure),
    }
}

fn balanced_body_partitions(work: &[(NonZeroU32, usize)]) -> (Vec<NonZeroU32>, Vec<NonZeroU32>) {
    let mut sorted = work.to_vec();
    sorted.sort_unstable_by(|(left_uid, left_bytes), (right_uid, right_bytes)| {
        right_bytes
            .cmp(left_bytes)
            .then_with(|| left_uid.cmp(right_uid))
    });
    let mut primary = Vec::with_capacity(sorted.len().div_ceil(2));
    let mut secondary = Vec::with_capacity(sorted.len() / 2);
    let mut primary_bytes = 0_usize;
    let mut secondary_bytes = 0_usize;
    for (uid, bytes) in sorted {
        if primary_bytes <= secondary_bytes {
            primary.push(uid);
            primary_bytes = primary_bytes.saturating_add(bytes);
        } else {
            secondary.push(uid);
            secondary_bytes = secondary_bytes.saturating_add(bytes);
        }
    }
    (primary, secondary)
}

async fn join_pair<A, B>(left: A, right: B) -> (A::Output, B::Output)
where
    A: Future,
    B: Future,
{
    let mut left = std::pin::pin!(left);
    let mut right = std::pin::pin!(right);
    let mut left_output = None;
    let mut right_output = None;
    poll_fn(move |context| {
        if left_output.is_none()
            && let Poll::Ready(output) = left.as_mut().poll(context)
        {
            left_output = Some(output);
        }
        if right_output.is_none()
            && let Poll::Ready(output) = right.as_mut().poll(context)
        {
            right_output = Some(output);
        }
        match (left_output.take(), right_output.take()) {
            (Some(left), Some(right)) => Poll::Ready((left, right)),
            (left, right) => {
                left_output = left;
                right_output = right;
                Poll::Pending
            }
        }
    })
    .await
}

type LocalBoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

async fn join_all_local<T>(futures: Vec<LocalBoxFuture<'_, T>>) -> Vec<T> {
    let mut futures = futures.into_iter().map(Some).collect::<Vec<_>>();
    let mut outputs = (0..futures.len()).map(|_| None).collect::<Vec<_>>();
    let mut remaining = futures.len();
    poll_fn(move |context| {
        for (index, future) in futures.iter_mut().enumerate() {
            let Some(future) = future.as_mut() else {
                continue;
            };
            if let Poll::Ready(output) = future.as_mut().poll(context) {
                outputs[index] = Some(output);
                *future = Box::pin(std::future::pending());
                remaining -= 1;
            }
        }
        if remaining == 0 {
            Poll::Ready(
                outputs
                    .iter_mut()
                    .map(|output| output.take().expect("completed future has an output"))
                    .collect(),
            )
        } else {
            Poll::Pending
        }
    })
    .await
}

async fn fetch_bodies(
    session: &mut ProtocolSession,
    uids: &[NonZeroU32],
    deadline: Instant,
    limits: InboxFetchLimits,
) -> Result<Vec<ProjectedBody>, ImapInboxFetchFailure> {
    if uids.is_empty() {
        return Ok(Vec::new());
    }
    let uid_set = uids
        .iter()
        .map(|uid| uid.get().to_string())
        .collect::<Vec<_>>()
        .join(",");
    let id = session
        .run_command(format!("UID FETCH {uid_set} (UID BODY.PEEK[])"), deadline)
        .await?;
    let mut bodies = Vec::with_capacity(uids.len());
    loop {
        let response = read_receive_response(session, deadline).await?;
        match response {
            ReceiveResponse::Done { tag, status } if tag == id => {
                require_ok_status(status)?;
                break;
            }
            ReceiveResponse::Done { .. } => return Err(ImapInboxFetchFailure::Protocol),
            ReceiveResponse::Fetch(fetch) => {
                let Some(bytes) = fetch.body else {
                    continue;
                };
                let uid = match fetch.uid {
                    Some(uid) => NonZeroU32::new(uid).ok_or(ImapInboxFetchFailure::Protocol)?,
                    None if uids.len() == 1 => uids[0],
                    None => return Err(ImapInboxFetchFailure::Protocol),
                };
                if !uids.contains(&uid) || bodies.iter().any(|body: &ProjectedBody| body.uid == uid)
                {
                    return Err(ImapInboxFetchFailure::Protocol);
                }
                if bytes.len() > limits.max_message_literal_bytes {
                    return Err(ImapInboxFetchFailure::ResourceLimit);
                }
                bodies.push(ProjectedBody { uid, bytes });
            }
            ReceiveResponse::Bye => return Err(ImapInboxFetchFailure::Offline),
            ReceiveResponse::Other => {}
        }
    }
    if bodies.len() != uids.len() {
        return Err(ImapInboxFetchFailure::Protocol);
    }
    Ok(bodies)
}

async fn fetch_body(
    session: &mut ProtocolSession,
    uid: NonZeroU32,
    deadline: Instant,
    limits: InboxFetchLimits,
) -> Result<Box<[u8]>, ImapInboxFetchFailure> {
    let id = session
        .run_command(
            format!("UID FETCH {} (UID BODY.PEEK[])", uid.get()),
            deadline,
        )
        .await?;
    let mut body = None;
    loop {
        let response = read_receive_response(session, deadline).await?;
        match response {
            ReceiveResponse::Done { tag, status } if tag == id => {
                require_ok_status(status)?;
                break;
            }
            ReceiveResponse::Done { .. } => return Err(ImapInboxFetchFailure::Protocol),
            ReceiveResponse::Fetch(fetch) => {
                let Some(body_response) = parse_body(fetch, uid)? else {
                    continue;
                };
                if body_response.uid != uid || body.is_some() {
                    return Err(ImapInboxFetchFailure::Protocol);
                }
                if body_response.bytes.len() > limits.max_message_literal_bytes {
                    return Err(ImapInboxFetchFailure::ResourceLimit);
                }
                body = Some(body_response.bytes);
            }
            ReceiveResponse::Bye => return Err(ImapInboxFetchFailure::Offline),
            ReceiveResponse::Other => {}
        }
    }
    body.ok_or(ImapInboxFetchFailure::Protocol)
}

async fn read_receive_response(
    session: &mut ProtocolSession,
    deadline: Instant,
) -> Result<ReceiveResponse, ImapInboxFetchFailure> {
    project_protocol_response(session.read_response(deadline).await?)
}

enum ReceiveResponse {
    Done {
        tag: ProtocolTag,
        status: ReceiveStatus,
    },
    Fetch(ProjectedFetch),
    Bye,
    Other,
}

#[derive(Clone, Copy)]
enum ReceiveStatus {
    Ok,
    No,
    Bad,
}

#[derive(Default)]
struct ProjectedFetch {
    uid: Option<u32>,
    flags: Option<ImapInboxFlags>,
    internal_date: Option<Box<str>>,
    declared_bytes: Option<u32>,
    envelope: Option<ImapInboxEnvelope>,
    metadata_headers: Option<Box<[u8]>>,
    body: Option<Box<[u8]>>,
}

fn project_protocol_response(
    response: ProtocolResponse,
) -> Result<ReceiveResponse, ImapInboxFetchFailure> {
    if let ProtocolResponse::Tagged { tag, status, .. } = response {
        return Ok(ReceiveResponse::Done {
            tag: ProtocolTag::from_bytes(tag),
            status: match status {
                ProtocolStatus::Ok => ReceiveStatus::Ok,
                ProtocolStatus::No => ReceiveStatus::No,
                ProtocolStatus::Bad => ReceiveStatus::Bad,
                _ => return Err(ImapInboxFetchFailure::Protocol),
            },
        });
    }
    match parse_untagged(&response).map_err(map_protocol_error)? {
        Some(UntaggedData::Fetch { data, .. }) => {
            let mut fetch = ProjectedFetch::default();
            for item in data.items() {
                project_protocol_fetch_item(&mut fetch, item)?;
            }
            Ok(ReceiveResponse::Fetch(fetch))
        }
        Some(UntaggedData::Status(status)) if status.kind == StatusKind::Bye => {
            Ok(ReceiveResponse::Bye)
        }
        _ => Ok(ReceiveResponse::Other),
    }
}

fn project_protocol_fetch_item(
    fetch: &mut ProjectedFetch,
    item: FetchResponseItem<'_>,
) -> Result<(), ImapInboxFetchFailure> {
    match item {
        FetchResponseItem::Uid(value) => set_once(&mut fetch.uid, value)?,
        FetchResponseItem::Flags(values) => {
            let mut flags = ImapInboxFlags::default();
            for flag in values.iter() {
                match flag {
                    FetchFlag::Seen => flags.seen = true,
                    FetchFlag::Flagged => flags.flagged = true,
                    FetchFlag::Answered => flags.answered = true,
                    FetchFlag::Draft => flags.draft = true,
                    FetchFlag::Deleted => flags.deleted = true,
                    _ => {}
                }
            }
            set_once(&mut fetch.flags, flags)?;
        }
        FetchResponseItem::InternalDate(value) => {
            let value = value
                .strip_prefix(b"\"")
                .and_then(|value| value.strip_suffix(b"\""))
                .ok_or(ImapInboxFetchFailure::Protocol)?;
            if value.len() > MAX_INTERNAL_DATE_BYTES {
                return Err(ImapInboxFetchFailure::ResourceLimit);
            }
            let value = std::str::from_utf8(value)
                .map_err(|_| ImapInboxFetchFailure::Protocol)?
                .to_owned()
                .into_boxed_str();
            set_once(&mut fetch.internal_date, value)?;
        }
        FetchResponseItem::Rfc822Size(value) => set_once(
            &mut fetch.declared_bytes,
            u32::try_from(value).map_err(|_| ImapInboxFetchFailure::ResourceLimit)?,
        )?,
        FetchResponseItem::Envelope(value) => {
            set_once(&mut fetch.envelope, project_protocol_envelope(value)?)?;
        }
        FetchResponseItem::BodySection { section, data, .. }
            if section.parts().next().is_none()
                && matches!(
                    section.text(),
                    Some(FetchSectionText::Header | FetchSectionText::HeaderFields(_))
                ) =>
        {
            if let Some(bytes) = data.decoded() {
                if bytes.len() > MAX_METADATA_HEADER_BYTES {
                    return Err(ImapInboxFetchFailure::ResourceLimit);
                }
                set_once(&mut fetch.metadata_headers, bytes.as_ref().into())?;
            }
        }
        FetchResponseItem::BodySection { section, data, .. } if section.is_entire_message() => {
            if let Some(bytes) = data.decoded() {
                set_once(&mut fetch.body, bytes.as_ref().into())?;
            }
        }
        FetchResponseItem::Rfc822(data) => {
            if let Some(bytes) = data.decoded() {
                set_once(&mut fetch.body, bytes.as_ref().into())?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn parse_metadata(fetch: ProjectedFetch) -> Result<InboxMetadata, ImapInboxFetchFailure> {
    if fetch.body.is_some() {
        return Err(ImapInboxFetchFailure::Protocol);
    }
    let envelope = match (fetch.envelope, fetch.metadata_headers) {
        (Some(envelope), None) => envelope,
        (None, Some(headers)) => project_metadata_headers(&headers)?,
        _ => return Err(ImapInboxFetchFailure::Protocol),
    };
    Ok(InboxMetadata {
        uid: fetch
            .uid
            .and_then(NonZeroU32::new)
            .ok_or(ImapInboxFetchFailure::Protocol)?,
        flags: fetch.flags.ok_or(ImapInboxFetchFailure::Protocol)?,
        internal_date: fetch.internal_date.ok_or(ImapInboxFetchFailure::Protocol)?,
        envelope,
        declared_bytes: fetch
            .declared_bytes
            .ok_or(ImapInboxFetchFailure::Protocol)?,
    })
}

fn project_metadata_headers(headers: &[u8]) -> Result<ImapInboxEnvelope, ImapInboxFetchFailure> {
    let message = MessageParser::new()
        .header_date(HeaderName::Date)
        .header_text(HeaderName::Subject)
        .header_address(HeaderName::From)
        .header_id(HeaderName::MessageId)
        .default_header_ignore()
        .parse_headers(headers)
        .ok_or(ImapInboxFetchFailure::Protocol)?;
    let from = message.from().and_then(|addresses| addresses.first());
    let address = from.and_then(|address| address.address());
    let (mailbox, host) = address
        .and_then(|address| address.rsplit_once('@'))
        .unwrap_or((address.unwrap_or_default(), ""));
    let date = message.date().map(|date| date.to_rfc822());
    let message_id = message
        .message_id()
        .map(|message_id| format!("<{message_id}>"));

    Ok(ImapInboxEnvelope {
        date: bounded_envelope_bytes(date.as_deref().map(str::as_bytes), MAX_ENVELOPE_DATE_BYTES)?,
        subject: bounded_envelope_bytes(
            message.subject().map(str::as_bytes),
            MAX_ENVELOPE_SUBJECT_BYTES,
        )?,
        from_name: bounded_envelope_bytes(
            from.and_then(|address| address.name()).map(str::as_bytes),
            MAX_ENVELOPE_NAME_BYTES,
        )?,
        from_mailbox: bounded_envelope_bytes(Some(mailbox.as_bytes()), MAX_ENVELOPE_MAILBOX_BYTES)?,
        from_host: bounded_envelope_bytes(Some(host.as_bytes()), MAX_ENVELOPE_HOST_BYTES)?,
        message_id: bounded_envelope_bytes(
            message_id.as_deref().map(str::as_bytes),
            MAX_ENVELOPE_MESSAGE_ID_BYTES,
        )?,
    })
}

struct ProjectedBody {
    uid: NonZeroU32,
    bytes: Box<[u8]>,
}

fn parse_body(
    fetch: ProjectedFetch,
    requested_uid: NonZeroU32,
) -> Result<Option<ProjectedBody>, ImapInboxFetchFailure> {
    let Some(body) = fetch.body else {
        return Ok(None);
    };
    // Some IMAP4rev1 servers omit the implicit UID item on a single-UID
    // UID FETCH. The command still identifies the body unambiguously.
    let uid = match fetch.uid {
        Some(uid) => NonZeroU32::new(uid).ok_or(ImapInboxFetchFailure::Protocol)?,
        None => requested_uid,
    };
    if uid != requested_uid {
        return Err(ImapInboxFetchFailure::Protocol);
    }
    Ok(Some(ProjectedBody { uid, bytes: body }))
}

fn set_once<T>(slot: &mut Option<T>, value: T) -> Result<(), ImapInboxFetchFailure> {
    if slot.replace(value).is_some() {
        Err(ImapInboxFetchFailure::Protocol)
    } else {
        Ok(())
    }
}

fn project_protocol_envelope(
    envelope: ProtocolEnvelope<'_>,
) -> Result<ImapInboxEnvelope, ImapInboxFetchFailure> {
    let from = envelope.from().iter().next();
    Ok(ImapInboxEnvelope {
        date: bounded_protocol_nstring(envelope.date(), MAX_ENVELOPE_DATE_BYTES)?,
        subject: bounded_protocol_nstring(envelope.subject(), MAX_ENVELOPE_SUBJECT_BYTES)?,
        from_name: bounded_protocol_nstring(
            from.map(|address| address.name)
                .unwrap_or(mail_protocol_imap::FetchNString::Nil),
            MAX_ENVELOPE_NAME_BYTES,
        )?,
        from_mailbox: bounded_protocol_nstring(
            from.map(|address| address.mailbox)
                .unwrap_or(mail_protocol_imap::FetchNString::Nil),
            MAX_ENVELOPE_MAILBOX_BYTES,
        )?,
        from_host: bounded_protocol_nstring(
            from.map(|address| address.host)
                .unwrap_or(mail_protocol_imap::FetchNString::Nil),
            MAX_ENVELOPE_HOST_BYTES,
        )?,
        message_id: bounded_protocol_nstring(envelope.message_id(), MAX_ENVELOPE_MESSAGE_ID_BYTES)?,
    })
}

fn bounded_protocol_nstring(
    value: mail_protocol_imap::FetchNString<'_>,
    maximum: usize,
) -> Result<Box<[u8]>, ImapInboxFetchFailure> {
    let value = value.decoded();
    bounded_envelope_bytes(value.as_deref(), maximum)
}

fn bounded_envelope_bytes(
    value: Option<&[u8]>,
    maximum: usize,
) -> Result<Box<[u8]>, ImapInboxFetchFailure> {
    let value = value.unwrap_or_default();
    if value.len() > maximum {
        Err(ImapInboxFetchFailure::ResourceLimit)
    } else {
        Ok(value.into())
    }
}

fn require_ok_status(status: ReceiveStatus) -> Result<(), ImapInboxFetchFailure> {
    match status {
        ReceiveStatus::Ok => Ok(()),
        ReceiveStatus::No => Err(ImapInboxFetchFailure::Permission),
        ReceiveStatus::Bad => Err(ImapInboxFetchFailure::Protocol),
    }
}

fn recoverable_cached_session_failure(failure: ImapInboxFetchFailure) -> bool {
    matches!(
        failure,
        ImapInboxFetchFailure::Offline
            | ImapInboxFetchFailure::Timeout
            | ImapInboxFetchFailure::Protocol
            | ImapInboxFetchFailure::ResourceLimit
    )
}

fn map_diagnostic_failure(failure: ImapDiagnosticFailure) -> ImapInboxFetchFailure {
    match failure.kind {
        ImapDiagnosticFailureKind::Authentication => ImapInboxFetchFailure::Authentication,
        ImapDiagnosticFailureKind::Permission => ImapInboxFetchFailure::Permission,
        ImapDiagnosticFailureKind::Certificate => ImapInboxFetchFailure::Certificate,
        ImapDiagnosticFailureKind::Timeout => ImapInboxFetchFailure::Timeout,
        ImapDiagnosticFailureKind::Offline => ImapInboxFetchFailure::Offline,
        ImapDiagnosticFailureKind::Protocol => ImapInboxFetchFailure::Protocol,
    }
}

fn map_receive_imap_error(error: &ImapError, mailbox: bool) -> ImapInboxFetchFailure {
    match error {
        ImapError::No(_) if mailbox => ImapInboxFetchFailure::Permission,
        ImapError::No(_) | ImapError::Bad(_) => ImapInboxFetchFailure::Protocol,
        ImapError::Io(error) => map_receive_io_error(error),
        ImapError::ConnectionLost => ImapInboxFetchFailure::Offline,
        _ => ImapInboxFetchFailure::Protocol,
    }
}

fn map_receive_io_error(error: &io::Error) -> ImapInboxFetchFailure {
    match error.kind() {
        io::ErrorKind::FileTooLarge => ImapInboxFetchFailure::ResourceLimit,
        io::ErrorKind::TimedOut => ImapInboxFetchFailure::Timeout,
        io::ErrorKind::InvalidData | io::ErrorKind::InvalidInput => ImapInboxFetchFailure::Protocol,
        _ => ImapInboxFetchFailure::Offline,
    }
}

#[derive(Clone, Copy)]
struct DiagnosticLimits {
    timeout: Duration,
    max_server_bytes: usize,
    max_client_bytes: usize,
}

impl DiagnosticLimits {
    const fn production() -> Self {
        Self {
            timeout: DIAGNOSTIC_TIMEOUT,
            max_server_bytes: MAX_SERVER_BYTES,
            max_client_bytes: MAX_CLIENT_BYTES,
        }
    }
}

fn failure(stage: ImapDiagnosticStage, kind: ImapDiagnosticFailureKind) -> ImapDiagnosticFailure {
    ImapDiagnosticFailure { stage, kind }
}

fn timeout_failure(stage: ImapDiagnosticStage) -> ImapDiagnosticFailure {
    failure(stage, ImapDiagnosticFailureKind::Timeout)
}

fn tls_failure_kind(error: &io::Error) -> ImapDiagnosticFailureKind {
    if error
        .get_ref()
        .and_then(|source| source.downcast_ref::<RustlsError>())
        .is_some_and(|error| matches!(error, RustlsError::InvalidCertificate(_)))
    {
        ImapDiagnosticFailureKind::Certificate
    } else {
        match error.kind() {
            io::ErrorKind::InvalidData | io::ErrorKind::FileTooLarge => {
                ImapDiagnosticFailureKind::Protocol
            }
            io::ErrorKind::PermissionDenied => ImapDiagnosticFailureKind::Permission,
            io::ErrorKind::TimedOut => ImapDiagnosticFailureKind::Timeout,
            _ => ImapDiagnosticFailureKind::Offline,
        }
    }
}

fn io_failure_kind(error: &io::Error) -> ImapDiagnosticFailureKind {
    match error.kind() {
        io::ErrorKind::InvalidData | io::ErrorKind::InvalidInput | io::ErrorKind::FileTooLarge => {
            ImapDiagnosticFailureKind::Protocol
        }
        io::ErrorKind::PermissionDenied => ImapDiagnosticFailureKind::Permission,
        io::ErrorKind::TimedOut => ImapDiagnosticFailureKind::Timeout,
        _ => ImapDiagnosticFailureKind::Offline,
    }
}

fn imap_failure_kind(error: &ImapError, stage: ImapDiagnosticStage) -> ImapDiagnosticFailureKind {
    match error {
        ImapError::No(_) if stage == ImapDiagnosticStage::Authenticate => {
            ImapDiagnosticFailureKind::Authentication
        }
        ImapError::No(_) if stage == ImapDiagnosticStage::Mailbox => {
            ImapDiagnosticFailureKind::Permission
        }
        ImapError::Io(error) => io_failure_kind(error),
        ImapError::ConnectionLost => ImapDiagnosticFailureKind::Offline,
        ImapError::Bad(_)
        | ImapError::No(_)
        | ImapError::Parse(_)
        | ImapError::Validate(_)
        | ImapError::Append => ImapDiagnosticFailureKind::Protocol,
        _ => ImapDiagnosticFailureKind::Protocol,
    }
}

struct BoundedIo<T> {
    inner: T,
    read_remaining: usize,
    write_remaining: usize,
}

impl<T> BoundedIo<T> {
    fn new(inner: T, read_limit: usize, write_limit: usize) -> Self {
        Self {
            inner,
            read_remaining: read_limit,
            write_remaining: write_limit,
        }
    }
}

impl<T> fmt::Debug for BoundedIo<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("BoundedIo(..)")
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for BoundedIo<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.read_remaining == 0 {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "IMAP response byte limit exceeded",
            )));
        }
        let allowed = buffer.remaining().min(this.read_remaining);
        let target = buffer.initialize_unfilled_to(allowed);
        let mut limited = ReadBuf::new(target);
        match Pin::new(&mut this.inner).poll_read(context, &mut limited) {
            Poll::Ready(Ok(())) => {
                let read = limited.filled().len();
                this.read_remaining -= read;
                buffer.advance(read);
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for BoundedIo<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.write_remaining == 0 {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "IMAP request byte limit exceeded",
            )));
        }
        let allowed = bytes.len().min(this.write_remaining);
        match Pin::new(&mut this.inner).poll_write(context, &bytes[..allowed]) {
            Poll::Ready(Ok(written)) => {
                this.write_remaining -= written;
                Poll::Ready(Ok(written))
            }
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(context)
    }

    fn poll_shutdown(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(context)
    }
}

#[cfg(test)]
mod smoke_tests {
    use std::sync::Arc;

    use rcgen::{CertifiedKey, generate_simple_self_signed};
    use rustls::{
        ClientConfig, RootCertStore, ServerConfig,
        pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
    };
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::TcpListener,
        task::JoinHandle,
    };
    use tokio_rustls::{TlsAcceptor, TlsConnector};

    use super::*;

    #[derive(Clone, Copy)]
    enum ServerBehavior {
        Success,
        RejectAuthentication,
        HangBeforeGreeting,
        OversizedGreeting,
    }

    struct TestServer {
        port: u16,
        certificate: CertificateDer<'static>,
        task: JoinHandle<()>,
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .expect("test runtime")
    }

    async fn start_server(behavior: ServerBehavior) -> TestServer {
        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(vec!["127.0.0.1".into()]).expect("test certificate");
        let certificate = cert.der().clone();
        let private_key =
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));
        let server_config =
            ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .expect("test protocol versions")
                .with_no_client_auth()
                .with_single_cert(vec![certificate.clone()], private_key)
                .expect("test server configuration");
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind test IMAP server");
        let port = listener.local_addr().expect("test server address").port();
        let acceptor = TlsAcceptor::from(Arc::new(server_config));
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept test connection");
            let mut stream = match acceptor.accept(stream).await {
                Ok(stream) => stream,
                Err(_) => return,
            };
            match behavior {
                ServerBehavior::HangBeforeGreeting => {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    return;
                }
                ServerBehavior::OversizedGreeting => {
                    let greeting = format!("* OK {}\r\n", "x".repeat(256));
                    let _ = stream.write_all(greeting.as_bytes()).await;
                    return;
                }
                ServerBehavior::Success | ServerBehavior::RejectAuthentication => {}
            }

            stream
                .write_all(b"* OK local test server ready\r\n")
                .await
                .expect("write greeting");
            let mut stream = BufReader::new(stream);
            let login = read_command(&mut stream).await;
            let login_tag = command_tag(&login);
            if matches!(behavior, ServerBehavior::RejectAuthentication) {
                stream
                    .get_mut()
                    .write_all(format!("{login_tag} NO invalid credentials\r\n").as_bytes())
                    .await
                    .expect("write login rejection");
                return;
            }
            stream
                .get_mut()
                .write_all(
                    format!("{login_tag} OK [CAPABILITY IMAP4rev1] authenticated\r\n").as_bytes(),
                )
                .await
                .expect("write login response");

            let examine = read_command(&mut stream).await;
            let examine_tag = command_tag(&examine);
            stream
                .get_mut()
                .write_all(
                    format!(
                        "* 0 EXISTS\r\n* 0 RECENT\r\n{examine_tag} OK [READ-ONLY] selected\r\n"
                    )
                    .as_bytes(),
                )
                .await
                .expect("write examine response");

            let logout = read_command(&mut stream).await;
            let logout_tag = command_tag(&logout);
            stream
                .get_mut()
                .write_all(format!("* BYE closing\r\n{logout_tag} OK logged out\r\n").as_bytes())
                .await
                .expect("write logout response");
        });

        TestServer {
            port,
            certificate,
            task,
        }
    }

    async fn read_command<T: AsyncRead + Unpin>(stream: &mut BufReader<T>) -> String {
        let mut command = String::new();
        stream
            .read_line(&mut command)
            .await
            .expect("read IMAP command");
        assert!(command.ends_with("\r\n"));
        command
    }

    fn command_tag(command: &str) -> &str {
        command
            .split_ascii_whitespace()
            .next()
            .expect("IMAP command tag")
    }

    fn trusted_connector(certificate: CertificateDer<'static>) -> TlsConnector {
        let mut roots = RootCertStore::empty();
        roots.add(certificate).expect("add test root");
        let config =
            ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .expect("test protocol versions")
                .with_root_certificates(roots)
                .with_no_client_auth();
        TlsConnector::from(Arc::new(config))
    }

    fn request(port: u16) -> ImapDiagnosticRequest {
        ImapDiagnosticRequest::new(
            "127.0.0.1",
            port,
            "user@example.test",
            Secret::new(b"app-password".to_vec()).expect("test secret"),
        )
        .expect("test request")
    }

    fn limits(timeout: Duration, max_server_bytes: usize) -> DiagnosticLimits {
        DiagnosticLimits {
            timeout,
            max_server_bytes,
            max_client_bytes: MAX_CLIENT_BYTES,
        }
    }

    #[test]
    fn completes_a_bounded_tls_imap_diagnostic() {
        runtime().block_on(async {
            let server = start_server(ServerBehavior::Success).await;
            let result = diagnose_with_connector(
                request(server.port),
                trusted_connector(server.certificate.clone()),
                limits(Duration::from_secs(2), MAX_SERVER_BYTES),
            )
            .await;
            assert_eq!(result, Ok(()));
            server.task.await.expect("test server task");
        });
    }

    #[test]
    fn maps_login_rejection_to_authentication() {
        runtime().block_on(async {
            let server = start_server(ServerBehavior::RejectAuthentication).await;
            let failure = diagnose_with_connector(
                request(server.port),
                trusted_connector(server.certificate.clone()),
                limits(Duration::from_secs(2), MAX_SERVER_BYTES),
            )
            .await
            .expect_err("authentication must fail");
            assert_eq!(failure.stage, ImapDiagnosticStage::Authenticate);
            assert_eq!(failure.kind, ImapDiagnosticFailureKind::Authentication);
            server.task.await.expect("test server task");
        });
    }

    #[test]
    fn rejects_a_certificate_outside_platform_trust() {
        runtime().block_on(async {
            let server = start_server(ServerBehavior::Success).await;
            let failure = diagnose_with_connector(
                request(server.port),
                platform_connector().expect("platform connector"),
                limits(Duration::from_secs(2), MAX_SERVER_BYTES),
            )
            .await
            .expect_err("self-signed certificate must fail");
            assert_eq!(failure.stage, ImapDiagnosticStage::Tls);
            assert_eq!(failure.kind, ImapDiagnosticFailureKind::Certificate);
            server.task.await.expect("test server task");
        });
    }

    #[test]
    fn applies_one_deadline_to_the_diagnostic() {
        runtime().block_on(async {
            let server = start_server(ServerBehavior::HangBeforeGreeting).await;
            let failure = diagnose_with_connector(
                request(server.port),
                trusted_connector(server.certificate.clone()),
                limits(Duration::from_millis(50), MAX_SERVER_BYTES),
            )
            .await
            .expect_err("hung greeting must time out");
            assert_eq!(failure.stage, ImapDiagnosticStage::Greeting);
            assert_eq!(failure.kind, ImapDiagnosticFailureKind::Timeout);
            server.task.abort();
            let _ = server.task.await;
        });
    }

    #[test]
    fn rejects_responses_over_the_byte_budget() {
        runtime().block_on(async {
            let server = start_server(ServerBehavior::OversizedGreeting).await;
            let failure = diagnose_with_connector(
                request(server.port),
                trusted_connector(server.certificate.clone()),
                limits(Duration::from_secs(2), 32),
            )
            .await
            .expect_err("oversized greeting must fail");
            assert_eq!(failure.stage, ImapDiagnosticStage::Greeting);
            assert_eq!(failure.kind, ImapDiagnosticFailureKind::Protocol);
            server.task.await.expect("test server task");
        });
    }
}

#[cfg(test)]
mod tests {
    use std::{future::Future, net::SocketAddr};

    use rcgen::{CertifiedKey, generate_simple_self_signed};
    use rustls::{
        RootCertStore, ServerConfig,
        pki_types::{CertificateDer, PrivateKeyDer},
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
        sync::oneshot,
        task::JoinHandle,
    };
    use tokio_rustls::{TlsAcceptor, server::TlsStream};

    use super::*;

    const HOST: &str = "127.0.0.1";
    const LOGIN: &str = "alice@example.test";
    const PASSWORD: &[u8] = b"local-app-password";
    const TEST_TIMEOUT: Duration = Duration::from_secs(3);

    enum Script {
        Success,
        AuthenticationDenied,
        MailboxDenied,
        RawGreeting(Vec<u8>),
        StallAfterLogin(Option<oneshot::Sender<()>>),
    }

    struct ScriptedServer {
        address: SocketAddr,
        connector: TlsConnector,
        task: JoinHandle<Vec<Box<str>>>,
    }

    fn run_async<F: Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .unwrap()
            .block_on(future)
    }

    fn request(port: u16) -> ImapDiagnosticRequest {
        ImapDiagnosticRequest::new(HOST, port, LOGIN, Secret::new(PASSWORD.to_vec()).unwrap())
            .unwrap()
    }

    fn test_tls() -> (TlsAcceptor, TlsConnector, CertificateDer<'static>) {
        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(vec![HOST.to_owned()]).unwrap();
        let certificate = cert.der().clone();
        let private_key: PrivateKeyDer<'static> = signing_key.into();
        let provider = Arc::new(crypto::ring::default_provider());

        let server = ServerConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![certificate.clone()], private_key)
            .unwrap();

        let mut roots = RootCertStore::empty();
        roots.add(certificate.clone()).unwrap();
        let client = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();

        (
            TlsAcceptor::from(Arc::new(server)),
            TlsConnector::from(Arc::new(client)),
            certificate,
        )
    }

    async fn spawn_server(script: Script) -> ScriptedServer {
        let (acceptor, connector, _) = test_tls();
        let listener = TcpListener::bind((HOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut stream = acceptor.accept(tcp).await.unwrap();
            run_script(&mut stream, script).await
        });
        ScriptedServer {
            address,
            connector,
            task,
        }
    }

    async fn read_command(stream: &mut TlsStream<TcpStream>) -> io::Result<Option<Box<str>>> {
        const MAX_COMMAND_BYTES: usize = 32 * 1024;

        let mut command = Vec::with_capacity(128);
        loop {
            let mut byte = [0_u8; 1];
            let read = stream.read(&mut byte).await?;
            if read == 0 {
                return if command.is_empty() {
                    Ok(None)
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "partial IMAP command",
                    ))
                };
            }
            command.push(byte[0]);
            if command.len() > MAX_COMMAND_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "IMAP test command exceeded its bound",
                ));
            }
            if command.ends_with(b"\r\n") {
                command.truncate(command.len() - 2);
                return String::from_utf8(command)
                    .map(String::into_boxed_str)
                    .map(Some)
                    .map_err(|_| {
                        io::Error::new(io::ErrorKind::InvalidData, "non-UTF-8 IMAP command")
                    });
            }
        }
    }

    async fn required_command(stream: &mut TlsStream<TcpStream>) -> Box<str> {
        read_command(stream)
            .await
            .unwrap()
            .expect("client closed before the required command")
    }

    fn tagged_response(command: &str, response: &str) -> Vec<u8> {
        let tag = command
            .split_once(' ')
            .expect("tagged command must contain a space")
            .0;
        format!("{tag} {response}\r\n").into_bytes()
    }

    async fn write_login_ok(stream: &mut TlsStream<TcpStream>, command: &str) {
        stream
            .write_all(&tagged_response(command, "OK authenticated"))
            .await
            .unwrap();
    }

    async fn write_capabilities(stream: &mut TlsStream<TcpStream>, command: &str) {
        stream
            .write_all(b"* CAPABILITY IMAP4rev1 IDLE\r\n")
            .await
            .unwrap();
        stream
            .write_all(&tagged_response(command, "OK capabilities complete"))
            .await
            .unwrap();
    }

    async fn run_script(stream: &mut TlsStream<TcpStream>, script: Script) -> Vec<Box<str>> {
        if let Script::RawGreeting(bytes) = script {
            stream.write_all(&bytes).await.unwrap();
            stream.flush().await.unwrap();
            return Vec::new();
        }

        stream
            .write_all(b"* OK Nivalis test IMAP ready\r\n")
            .await
            .unwrap();
        stream.flush().await.unwrap();

        let login = required_command(stream).await;
        if command_name(&login) != Some("LOGIN") {
            panic!("expected LOGIN command");
        }
        let mut commands = vec![login];

        match script {
            Script::AuthenticationDenied => {
                let response = tagged_response(
                    &commands[0],
                    "NO [AUTHENTICATIONFAILED] credentials rejected",
                );
                stream.write_all(&response).await.unwrap();
                stream.flush().await.unwrap();
                return commands;
            }
            Script::StallAfterLogin(reached) => {
                if let Some(reached) = reached {
                    let _ = reached.send(());
                }
                let mut byte = [0_u8; 1];
                let _ = stream.read(&mut byte).await;
                return commands;
            }
            Script::Success | Script::MailboxDenied => {}
            Script::RawGreeting(_) => unreachable!(),
        }

        write_login_ok(stream, &commands[0]).await;
        let capability = required_command(stream).await;
        if command_name(&capability) != Some("CAPABILITY") {
            panic!("expected CAPABILITY command");
        }
        write_capabilities(stream, &capability).await;
        commands.push(capability);

        let examine = required_command(stream).await;
        if command_name(&examine) != Some("EXAMINE") {
            panic!("expected EXAMINE command");
        }
        if matches!(script, Script::MailboxDenied) {
            let response = tagged_response(&examine, "NO [NOPERM] mailbox access denied");
            stream.write_all(&response).await.unwrap();
            stream.flush().await.unwrap();
            commands.push(examine);
            return commands;
        }
        stream
            .write_all(b"* FLAGS (\\Answered \\Flagged \\Deleted \\Seen \\Draft)\r\n")
            .await
            .unwrap();
        stream
            .write_all(b"* 0 EXISTS\r\n* 0 RECENT\r\n")
            .await
            .unwrap();
        stream
            .write_all(&tagged_response(
                &examine,
                "OK [READ-ONLY] mailbox examined",
            ))
            .await
            .unwrap();
        commands.push(examine);

        let logout = required_command(stream).await;
        if command_name(&logout) != Some("LOGOUT") {
            panic!("expected LOGOUT command");
        }
        stream.write_all(b"* BYE logging out\r\n").await.unwrap();
        stream
            .write_all(&tagged_response(&logout, "OK logout complete"))
            .await
            .unwrap();
        stream.flush().await.unwrap();
        commands.push(logout);
        commands
    }

    fn command_name(command: &str) -> Option<&str> {
        command.split_whitespace().nth(1)
    }

    fn assert_failure(
        result: Result<(), ImapDiagnosticFailure>,
        stage: ImapDiagnosticStage,
        kind: ImapDiagnosticFailureKind,
    ) {
        assert_eq!(result.unwrap_err(), failure(stage, kind));
    }

    #[test]
    fn completes_greeting_login_capability_examine_and_logout() {
        run_async(async {
            let server = spawn_server(Script::Success).await;
            let result = diagnose_with_connector(
                request(server.address.port()),
                server.connector,
                DiagnosticLimits::production(),
            )
            .await;
            assert_eq!(result, Ok(()));

            let commands = tokio::time::timeout(TEST_TIMEOUT, server.task)
                .await
                .unwrap()
                .unwrap();
            let names: Vec<_> = commands
                .iter()
                .filter_map(|command| command_name(command))
                .collect();
            assert_eq!(names, ["LOGIN", "CAPABILITY", "EXAMINE", "LOGOUT"]);
        });
    }

    #[test]
    fn classifies_authentication_and_mailbox_denials() {
        run_async(async {
            let auth_server = spawn_server(Script::AuthenticationDenied).await;
            let auth_result = diagnose_with_connector(
                request(auth_server.address.port()),
                auth_server.connector,
                DiagnosticLimits::production(),
            )
            .await;
            assert_failure(
                auth_result,
                ImapDiagnosticStage::Authenticate,
                ImapDiagnosticFailureKind::Authentication,
            );
            assert_eq!(
                tokio::time::timeout(TEST_TIMEOUT, auth_server.task)
                    .await
                    .unwrap()
                    .unwrap()
                    .len(),
                1
            );

            let mailbox_server = spawn_server(Script::MailboxDenied).await;
            let mailbox_result = diagnose_with_connector(
                request(mailbox_server.address.port()),
                mailbox_server.connector,
                DiagnosticLimits::production(),
            )
            .await;
            assert_failure(
                mailbox_result,
                ImapDiagnosticStage::Mailbox,
                ImapDiagnosticFailureKind::Permission,
            );
            assert_eq!(
                tokio::time::timeout(TEST_TIMEOUT, mailbox_server.task)
                    .await
                    .unwrap()
                    .unwrap()
                    .len(),
                3
            );
        });
    }

    #[test]
    fn platform_verifier_rejects_the_self_signed_server() {
        run_async(async {
            platform_connector().expect("platform verifier configuration must be available");
            let (acceptor, _, _) = test_tls();
            let listener = TcpListener::bind((HOST, 0)).await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let server = tokio::spawn(async move {
                let (tcp, _) = listener.accept().await.unwrap();
                acceptor.accept(tcp).await
            });

            let result = diagnose_app_password(request(port)).await;
            assert_failure(
                result,
                ImapDiagnosticStage::Tls,
                ImapDiagnosticFailureKind::Certificate,
            );
            assert!(
                tokio::time::timeout(TEST_TIMEOUT, server)
                    .await
                    .unwrap()
                    .unwrap()
                    .is_err()
            );
        });
    }

    #[test]
    fn enforces_the_shared_deadline_and_is_cancellation_safe() {
        run_async(async {
            let timeout_server = spawn_server(Script::StallAfterLogin(None)).await;
            let result = diagnose_with_connector(
                request(timeout_server.address.port()),
                timeout_server.connector,
                DiagnosticLimits {
                    timeout: Duration::from_millis(500),
                    ..DiagnosticLimits::production()
                },
            )
            .await;
            assert_failure(
                result,
                ImapDiagnosticStage::Authenticate,
                ImapDiagnosticFailureKind::Timeout,
            );
            tokio::time::timeout(TEST_TIMEOUT, timeout_server.task)
                .await
                .unwrap()
                .unwrap();

            let (reached_tx, reached_rx) = oneshot::channel();
            let cancel_server = spawn_server(Script::StallAfterLogin(Some(reached_tx))).await;
            let diagnostic = tokio::spawn(diagnose_with_connector(
                request(cancel_server.address.port()),
                cancel_server.connector,
                DiagnosticLimits {
                    timeout: Duration::from_secs(10),
                    ..DiagnosticLimits::production()
                },
            ));
            tokio::time::timeout(TEST_TIMEOUT, reached_rx)
                .await
                .unwrap()
                .unwrap();
            diagnostic.abort();
            assert!(diagnostic.await.unwrap_err().is_cancelled());
            tokio::time::timeout(TEST_TIMEOUT, cancel_server.task)
                .await
                .unwrap()
                .unwrap();
        });
    }

    #[test]
    fn rejects_a_response_beyond_the_configured_byte_budget() {
        run_async(async {
            let mut greeting = b"* OK ".to_vec();
            greeting.extend(std::iter::repeat_n(b'x', 512));
            greeting.extend_from_slice(b"\r\n");
            let server = spawn_server(Script::RawGreeting(greeting)).await;
            let result = diagnose_with_connector(
                request(server.address.port()),
                server.connector,
                DiagnosticLimits {
                    max_server_bytes: 32,
                    ..DiagnosticLimits::production()
                },
            )
            .await;
            assert_failure(
                result,
                ImapDiagnosticStage::Greeting,
                ImapDiagnosticFailureKind::Protocol,
            );
            tokio::time::timeout(TEST_TIMEOUT, server.task)
                .await
                .unwrap()
                .unwrap();
        });
    }

    #[test]
    fn rejects_an_oversized_literal_before_the_parser_can_reserve_it() {
        run_async(async {
            let (mut server, client) = tokio::io::duplex(256);
            server
                .write_all(
                    b"* OK status text may end in {64}\r\n\
                      * 1 FETCH (BODY[] {268435456}\r\n",
                )
                .await
                .unwrap();
            drop(server);

            let mut client = Client::new(BoundedIo::new(client, 1024, 1024));
            let status = client.read_response().await.unwrap().unwrap();
            assert!(matches!(
                status.parsed(),
                Response::Data {
                    status: Status::Ok,
                    ..
                }
            ));
            let error = client.read_response().await.unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::Other);
        });
    }

    #[test]
    fn request_input_bounds_are_exact_and_debug_is_redacted() {
        let host = format!(
            "{}.{}.{}.{}",
            "a".repeat(63),
            "b".repeat(63),
            "c".repeat(63),
            "d".repeat(61)
        );
        assert_eq!(host.len(), MAX_HOST_BYTES);
        let login = "l".repeat(MAX_LOGIN_BYTES);
        let request = ImapDiagnosticRequest::new(
            &host,
            u16::MAX,
            &login,
            Secret::new(PASSWORD.to_vec()).unwrap(),
        )
        .unwrap();
        let debug = format!("{request:?}");
        assert_eq!(debug, "ImapDiagnosticRequest([REDACTED])");
        assert!(!debug.contains(&host));
        assert!(!debug.contains(&login));
        assert!(!debug.contains(std::str::from_utf8(PASSWORD).unwrap()));

        assert_eq!(
            ImapDiagnosticRequest::new(
                &"a".repeat(MAX_HOST_BYTES + 1),
                993,
                LOGIN,
                Secret::new(PASSWORD.to_vec()).unwrap(),
            )
            .unwrap_err(),
            ImapDiagnosticInputError::Host
        );
        assert_eq!(
            ImapDiagnosticRequest::new(HOST, 0, LOGIN, Secret::new(PASSWORD.to_vec()).unwrap(),)
                .unwrap_err(),
            ImapDiagnosticInputError::Port
        );
        assert_eq!(
            ImapDiagnosticRequest::new(
                HOST,
                993,
                &"l".repeat(MAX_LOGIN_BYTES + 1),
                Secret::new(PASSWORD.to_vec()).unwrap(),
            )
            .unwrap_err(),
            ImapDiagnosticInputError::Login
        );
        assert_eq!(
            ImapDiagnosticRequest::new(HOST, 993, LOGIN, Secret::new(vec![0xff]).unwrap(),)
                .unwrap_err(),
            ImapDiagnosticInputError::SecretEncoding
        );
    }
}

#[cfg(test)]
mod inbox_fetch_tests {
    use std::{future::Future, net::SocketAddr, sync::Arc};

    use rcgen::{CertifiedKey, generate_simple_self_signed};
    use rustls::{ClientConfig, RootCertStore, ServerConfig, pki_types::PrivateKeyDer};
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::{TcpListener, TcpStream},
        sync::oneshot,
        task::JoinHandle,
    };
    use tokio_rustls::{TlsAcceptor, TlsConnector, server::TlsStream as ServerTlsStream};

    use super::*;

    const HOST: &str = "127.0.0.1";
    const LOGIN: &str = "receive@example.test";
    const PASSWORD: &[u8] = b"receive-password";
    const UID_VALIDITY: u32 = 777;
    const TEST_TIMEOUT: Duration = Duration::from_secs(3);
    const RAW_ONE: &[u8] =
        b"From: Alice <alice@example.test>\r\nSubject: First\r\nMessage-ID: <one@example.test>\r\n\r\nHello one.\r\n";
    const RAW_TWO: &[u8] =
        b"From: Bob <bob@example.test>\r\nSubject: Second\r\nMessage-ID: <two@example.test>\r\n\r\nHello two.\r\n";

    #[derive(Clone)]
    struct FixtureMessage {
        uid: u32,
        declared_bytes: u32,
        raw: Box<[u8]>,
    }

    struct ServerPlan {
        uid_validity: Option<u32>,
        omit_uid_next: bool,
        uid_next_override: Option<u32>,
        messages: Vec<FixtureMessage>,
        metadata_status: Option<&'static str>,
        extra_metadata: usize,
        omit_body_uid: bool,
        stall_body: Option<oneshot::Sender<()>>,
    }

    impl ServerPlan {
        fn messages(messages: Vec<FixtureMessage>) -> Self {
            Self {
                uid_validity: Some(UID_VALIDITY),
                omit_uid_next: false,
                uid_next_override: None,
                messages,
                metadata_status: None,
                extra_metadata: 0,
                omit_body_uid: false,
                stall_body: None,
            }
        }
    }

    struct TestServer {
        address: SocketAddr,
        connector: TlsConnector,
        task: JoinHandle<Vec<Box<str>>>,
    }

    type ParallelServerTranscripts = (Vec<Box<str>>, Vec<Box<str>>);

    struct ParallelTestServer {
        address: SocketAddr,
        connector: TlsConnector,
        task: JoinHandle<ParallelServerTranscripts>,
    }

    fn run_async<F: Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .unwrap()
            .block_on(future)
    }

    fn fixture(uid: u32, raw: &[u8]) -> FixtureMessage {
        FixtureMessage {
            uid,
            declared_bytes: u32::try_from(raw.len()).unwrap(),
            raw: raw.into(),
        }
    }

    fn limits() -> InboxFetchLimits {
        InboxFetchLimits {
            timeout: TEST_TIMEOUT,
            max_server_bytes: 64 * 1024,
            max_client_bytes: 16 * 1024,
            max_messages: 2,
            max_message_literal_bytes: 1024,
            max_page_literal_bytes: 2048,
            body_connections: 1,
        }
    }

    #[test]
    fn body_partitions_balance_declared_bytes_across_two_connections() {
        let work = [(1, 8), (2, 1), (3, 7), (4, 2)]
            .into_iter()
            .map(|(uid, bytes)| (NonZeroU32::new(uid).unwrap(), bytes))
            .collect::<Vec<_>>();
        let (primary, secondary) = balanced_body_partitions(&work);
        let total = |uids: &[NonZeroU32]| {
            uids.iter()
                .map(|uid| {
                    work.iter()
                        .find(|(candidate, _)| candidate == uid)
                        .unwrap()
                        .1
                })
                .sum::<usize>()
        };

        assert_eq!(primary.len() + secondary.len(), work.len());
        assert!(!primary.is_empty());
        assert!(!secondary.is_empty());
        assert_eq!(total(&primary), total(&secondary));
    }

    #[test]
    fn metadata_headers_decode_encoded_words_and_preserve_message_date() {
        let headers = b"Date: Tue, 21 Jul 2026 08:15:30 +0800\r\nSubject: =?UTF-8?B?5ZCM5q2l5a6M5oiQ?=\r\nFrom: =?UTF-8?B?5rWL6K+V?= <sender@example.test>\r\nMessage-ID: <encoded@example.test>\r\n\r\n";
        let envelope = project_metadata_headers(headers).unwrap();

        assert_eq!(envelope.subject.as_ref(), "同步完成".as_bytes());
        assert_eq!(envelope.from_name.as_ref(), "测试".as_bytes());
        assert_eq!(envelope.from_mailbox.as_ref(), b"sender");
        assert_eq!(envelope.from_host.as_ref(), b"example.test");
        assert_eq!(envelope.message_id.as_ref(), b"<encoded@example.test>");
        assert_eq!(envelope.date.as_ref(), b"Tue, 21 Jul 2026 08:15:30 +0800");
    }

    fn request(port: u16, expected_uid_validity: Option<u32>) -> ImapInboxFetchRequest {
        request_from(port, 1, expected_uid_validity)
    }

    fn request_from(
        port: u16,
        first_uid: u32,
        expected_uid_validity: Option<u32>,
    ) -> ImapInboxFetchRequest {
        request_with_history(port, first_uid, expected_uid_validity, None, false)
    }

    fn request_with_history(
        port: u16,
        first_uid: u32,
        expected_uid_validity: Option<u32>,
        history_cursor: Option<u32>,
        history_complete: bool,
    ) -> ImapInboxFetchRequest {
        ImapInboxFetchRequest::new(
            HOST,
            port,
            LOGIN,
            Secret::new(PASSWORD.to_vec()).unwrap(),
            first_uid,
            expected_uid_validity,
            history_cursor,
            history_complete,
        )
        .unwrap()
    }

    fn content_request(port: u16, uid: u32) -> ImapMessageContentFetchRequest {
        ImapMessageContentFetchRequest::new(
            HOST,
            port,
            LOGIN,
            Secret::new(PASSWORD.to_vec()).unwrap(),
            UID_VALIDITY,
            uid,
        )
        .unwrap()
    }

    fn test_tls() -> (TlsAcceptor, TlsConnector) {
        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(vec![HOST.to_owned()]).unwrap();
        let certificate = cert.der().clone();
        let private_key: PrivateKeyDer<'static> = signing_key.into();
        let provider = Arc::new(crypto::ring::default_provider());
        let server = ServerConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![certificate.clone()], private_key)
            .unwrap();
        let mut roots = RootCertStore::empty();
        roots.add(certificate).unwrap();
        let client = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        (
            TlsAcceptor::from(Arc::new(server)),
            TlsConnector::from(Arc::new(client)),
        )
    }

    async fn spawn_server(plan: ServerPlan) -> TestServer {
        let (acceptor, connector) = test_tls();
        let listener = TcpListener::bind((HOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let stream = acceptor.accept(tcp).await.unwrap();
            run_server(stream, plan).await
        });
        TestServer {
            address,
            connector,
            task,
        }
    }

    async fn spawn_parallel_server(
        messages: Vec<FixtureMessage>,
        fail_secondary_body: bool,
    ) -> ParallelTestServer {
        let (acceptor, connector) = test_tls();
        let listener = TcpListener::bind((HOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (primary_tcp, _) = listener.accept().await.unwrap();
            let primary_stream = acceptor.accept(primary_tcp).await.unwrap();
            let primary_messages = messages.clone();
            let primary = tokio::spawn(async move {
                run_server(primary_stream, ServerPlan::messages(primary_messages)).await
            });

            let (secondary_tcp, _) = listener.accept().await.unwrap();
            let secondary_stream = acceptor.accept(secondary_tcp).await.unwrap();
            let secondary =
                run_body_only_server(secondary_stream, messages, fail_secondary_body).await;
            (primary.await.unwrap(), secondary)
        });
        ParallelTestServer {
            address,
            connector,
            task,
        }
    }

    async fn spawn_parallel_metadata_server(messages: Vec<FixtureMessage>) -> ParallelTestServer {
        let (acceptor, connector) = test_tls();
        let listener = TcpListener::bind((HOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (primary_tcp, _) = listener.accept().await.unwrap();
            let primary_stream = acceptor.accept(primary_tcp).await.unwrap();
            let primary_messages = messages.clone();
            let primary = tokio::spawn(async move {
                run_server(primary_stream, ServerPlan::messages(primary_messages)).await
            });

            let (secondary_tcp, _) = listener.accept().await.unwrap();
            let secondary_stream = acceptor.accept(secondary_tcp).await.unwrap();
            let secondary = run_metadata_only_server(secondary_stream, messages).await;
            (primary.await.unwrap(), secondary)
        });
        ParallelTestServer {
            address,
            connector,
            task,
        }
    }

    async fn spawn_body_server(messages: Vec<FixtureMessage>) -> TestServer {
        let (acceptor, connector) = test_tls();
        let listener = TcpListener::bind((HOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let stream = acceptor.accept(tcp).await.unwrap();
            run_body_only_server(stream, messages, false).await
        });
        TestServer {
            address,
            connector,
            task,
        }
    }

    async fn spawn_idle_server() -> TestServer {
        let (acceptor, connector) = test_tls();
        let listener = TcpListener::bind((HOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let stream = acceptor.accept(tcp).await.unwrap();
            let mut stream = BufReader::new(stream);
            stream
                .get_mut()
                .write_all(b"* OK idle fixture ready\r\n")
                .await
                .unwrap();
            let login = read_command(&mut stream).await.unwrap();
            tagged(
                &mut stream,
                &login,
                "OK [CAPABILITY IMAP4rev1 IDLE] authenticated",
            )
            .await;
            let examine = read_command(&mut stream).await.unwrap();
            stream
                .get_mut()
                .write_all(
                    b"* 1 EXISTS\r\n* OK [UIDVALIDITY 777] epoch\r\n* OK [UIDNEXT 2] next\r\n",
                )
                .await
                .unwrap();
            tagged(&mut stream, &examine, "OK [READ-ONLY] selected").await;
            let idle = read_command(&mut stream).await.unwrap();
            stream
                .get_mut()
                .write_all(b"+ idling\r\n* 2 EXISTS\r\n")
                .await
                .unwrap();
            let done = read_command(&mut stream).await.unwrap();
            assert_eq!(done.as_ref(), "DONE");
            tagged(&mut stream, &idle, "OK idle complete").await;
            let logout = read_command(&mut stream).await.unwrap();
            tagged(&mut stream, &logout, "OK logout complete").await;
            vec![login, examine, idle, done, logout]
        });
        TestServer {
            address,
            connector,
            task,
        }
    }

    async fn spawn_idle_disconnect_server() -> TestServer {
        let (acceptor, connector) = test_tls();
        let listener = TcpListener::bind((HOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let stream = acceptor.accept(tcp).await.unwrap();
            let mut stream = BufReader::new(stream);
            stream
                .get_mut()
                .write_all(b"* OK idle disconnect fixture ready\r\n")
                .await
                .unwrap();
            let login = read_command(&mut stream).await.unwrap();
            tagged(
                &mut stream,
                &login,
                "OK [CAPABILITY IMAP4rev1 IDLE] authenticated",
            )
            .await;
            let examine = read_command(&mut stream).await.unwrap();
            stream
                .get_mut()
                .write_all(
                    b"* 1 EXISTS\r\n* OK [UIDVALIDITY 777] epoch\r\n* OK [UIDNEXT 2] next\r\n",
                )
                .await
                .unwrap();
            tagged(&mut stream, &examine, "OK [READ-ONLY] selected").await;
            let idle = read_command(&mut stream).await.unwrap();
            stream.get_mut().write_all(b"+ idling\r\n").await.unwrap();
            stream.get_mut().shutdown().await.unwrap();
            vec![login, examine, idle]
        });
        TestServer {
            address,
            connector,
            task,
        }
    }

    async fn spawn_cancellable_idle_server() -> (TestServer, oneshot::Receiver<()>) {
        let (acceptor, connector) = test_tls();
        let listener = TcpListener::bind((HOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let (idle_started, started) = oneshot::channel();
        let task = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let stream = acceptor.accept(tcp).await.unwrap();
            let mut stream = BufReader::new(stream);
            stream
                .get_mut()
                .write_all(b"* OK cancellable idle fixture ready\r\n")
                .await
                .unwrap();
            let login = read_command(&mut stream).await.unwrap();
            tagged(
                &mut stream,
                &login,
                "OK [CAPABILITY IMAP4rev1 IDLE] authenticated",
            )
            .await;
            let examine = read_command(&mut stream).await.unwrap();
            stream
                .get_mut()
                .write_all(
                    b"* 1 EXISTS\r\n* OK [UIDVALIDITY 777] epoch\r\n* OK [UIDNEXT 2] next\r\n",
                )
                .await
                .unwrap();
            tagged(&mut stream, &examine, "OK [READ-ONLY] selected").await;
            let idle = read_command(&mut stream).await.unwrap();
            stream.get_mut().write_all(b"+ idling\r\n").await.unwrap();
            let _ = idle_started.send(());
            let done = read_command(&mut stream).await.unwrap();
            assert_eq!(done.as_ref(), "DONE");
            tagged(&mut stream, &idle, "OK idle cancelled").await;
            let logout = read_command(&mut stream).await.unwrap();
            tagged(&mut stream, &logout, "OK logout complete").await;
            vec![login, examine, idle, done, logout]
        });
        (
            TestServer {
                address,
                connector,
                task,
            },
            started,
        )
    }

    async fn read_command(stream: &mut BufReader<ServerTlsStream<TcpStream>>) -> Option<Box<str>> {
        let mut command = String::new();
        match stream.read_line(&mut command).await {
            Ok(0) | Err(_) => None,
            Ok(_) => {
                assert!(command.ends_with("\r\n"));
                command.truncate(command.len() - 2);
                Some(command.into_boxed_str())
            }
        }
    }

    fn command_tag(command: &str) -> &str {
        command.split_whitespace().next().unwrap()
    }

    fn command_name(command: &str) -> Option<&str> {
        command.split_whitespace().nth(1)
    }

    async fn tagged(
        stream: &mut BufReader<ServerTlsStream<TcpStream>>,
        command: &str,
        response: &str,
    ) {
        stream
            .get_mut()
            .write_all(format!("{} {response}\r\n", command_tag(command)).as_bytes())
            .await
            .unwrap();
    }

    async fn write_metadata(
        stream: &mut BufReader<ServerTlsStream<TcpStream>>,
        sequence: usize,
        message: &FixtureMessage,
    ) {
        let headers = format!(
            "Date: Mon, 20 Jul 2026 12:00:00 +0000\r\nSubject: Message {}\r\nFrom: Sender {} <sender@example.test>\r\nMessage-ID: <{}@example.test>\r\n\r\n",
            message.uid, message.uid, message.uid,
        );
        let response = format!(
            "* {sequence} FETCH (UID {} FLAGS (\\Seen \\Flagged) INTERNALDATE \"20-Jul-2026 12:00:00 +0000\" RFC822.SIZE {} BODY[HEADER.FIELDS (DATE FROM SUBJECT MESSAGE-ID)] {{{}}}\r\n{})\r\n",
            message.uid,
            message.declared_bytes,
            headers.len(),
            headers,
        );
        stream
            .get_mut()
            .write_all(response.as_bytes())
            .await
            .unwrap();
    }

    async fn run_server(stream: ServerTlsStream<TcpStream>, mut plan: ServerPlan) -> Vec<Box<str>> {
        let mut stream = BufReader::new(stream);
        stream
            .get_mut()
            .write_all(b"* OK receive fixture ready\r\n")
            .await
            .unwrap();
        let Some(login) = read_command(&mut stream).await else {
            return Vec::new();
        };
        assert_eq!(command_name(&login), Some("LOGIN"));
        tagged(
            &mut stream,
            &login,
            "OK [CAPABILITY IMAP4rev1] authenticated",
        )
        .await;
        let Some(examine) = read_command(&mut stream).await else {
            return vec![login];
        };
        assert_eq!(command_name(&examine), Some("EXAMINE"));
        let exists = plan.messages.len() + plan.extra_metadata;
        stream
            .get_mut()
            .write_all(format!("* {exists} EXISTS\r\n").as_bytes())
            .await
            .unwrap();
        if let Some(uid_validity) = plan.uid_validity {
            stream
                .get_mut()
                .write_all(format!("* OK [UIDVALIDITY {uid_validity}] epoch\r\n").as_bytes())
                .await
                .unwrap();
        }
        let uid_next = plan.uid_next_override.unwrap_or_else(|| {
            plan.messages
                .last()
                .map_or(1, |message| message.uid.saturating_add(1))
        });
        if !plan.omit_uid_next {
            stream
                .get_mut()
                .write_all(format!("* OK [UIDNEXT {uid_next}] next\r\n").as_bytes())
                .await
                .unwrap();
        }
        tagged(&mut stream, &examine, "OK [READ-ONLY] selected").await;
        let mut commands = vec![login, examine];

        let mut pending_metadata = None;
        if !plan.messages.is_empty() {
            let Some(command) = read_command(&mut stream).await else {
                return commands;
            };
            if command.contains("UID SEARCH UID ") {
                commands.push(command.clone());
                let first_uid = command
                    .split_whitespace()
                    .nth(4)
                    .unwrap()
                    .split(':')
                    .next()
                    .unwrap()
                    .parse::<u32>()
                    .unwrap();
                let uids = plan
                    .messages
                    .iter()
                    .filter(|message| message.uid >= first_uid)
                    .map(|message| message.uid.to_string())
                    .collect::<Vec<_>>()
                    .join(" ");
                stream
                    .get_mut()
                    .write_all(format!("* SEARCH {uids}\r\n").as_bytes())
                    .await
                    .unwrap();
                tagged(&mut stream, &command, "OK search complete").await;
            } else {
                pending_metadata = Some(command);
            }
        }

        let metadata = match pending_metadata {
            Some(metadata) => metadata,
            None => {
                let Some(metadata) = read_command(&mut stream).await else {
                    return commands;
                };
                metadata
            }
        };
        commands.push(metadata.clone());
        if command_name(&metadata) == Some("LOGOUT") {
            tagged(&mut stream, &metadata, "OK logout complete").await;
            return commands;
        }
        assert!(metadata.contains("FETCH "));
        if let Some(status) = plan.metadata_status {
            tagged(&mut stream, &metadata, status).await;
            let _ = read_command(&mut stream).await;
            return commands;
        }
        let requested_messages = if metadata.contains("UID FETCH") {
            let requested_uids = metadata
                .split_whitespace()
                .nth(3)
                .unwrap()
                .split(',')
                .map(|uid| uid.parse::<u32>().unwrap())
                .collect::<Vec<_>>();
            plan.messages
                .iter()
                .filter(|message| requested_uids.contains(&message.uid))
                .collect::<Vec<_>>()
        } else {
            let first_sequence = metadata
                .split_whitespace()
                .nth(2)
                .unwrap()
                .split(':')
                .next()
                .unwrap()
                .parse::<usize>()
                .unwrap();
            plan.messages.iter().skip(first_sequence - 1).collect()
        };
        for message in requested_messages {
            let sequence = plan
                .messages
                .iter()
                .position(|candidate| candidate.uid == message.uid)
                .unwrap()
                + 1;
            write_metadata(&mut stream, sequence, message).await;
        }
        for extra in 0..plan.extra_metadata {
            write_metadata(
                &mut stream,
                plan.messages.len() + extra + 1,
                &fixture(
                    u32::try_from(plan.messages.len() + extra + 1).unwrap(),
                    b"x",
                ),
            )
            .await;
        }
        tagged(&mut stream, &metadata, "OK metadata complete").await;

        while let Some(command) = read_command(&mut stream).await {
            commands.push(command.clone());
            if command_name(&command) == Some("LOGOUT") {
                tagged(&mut stream, &command, "OK logout complete").await;
                break;
            }
            if command_name(&command) == Some("EXAMINE") {
                stream
                    .get_mut()
                    .write_all(format!("* {exists} EXISTS\r\n").as_bytes())
                    .await
                    .unwrap();
                if let Some(uid_validity) = plan.uid_validity {
                    stream
                        .get_mut()
                        .write_all(
                            format!("* OK [UIDVALIDITY {uid_validity}] epoch\r\n").as_bytes(),
                        )
                        .await
                        .unwrap();
                }
                if !plan.omit_uid_next {
                    stream
                        .get_mut()
                        .write_all(format!("* OK [UIDNEXT {uid_next}] next\r\n").as_bytes())
                        .await
                        .unwrap();
                }
                tagged(&mut stream, &command, "OK [READ-ONLY] selected").await;
                continue;
            }
            assert!(command.contains("UID FETCH"));
            if let Some(reached) = plan.stall_body.take() {
                let _ = reached.send(());
                let _ = read_command(&mut stream).await;
                break;
            }
            let requested_body_uids = command
                .split_whitespace()
                .nth(3)
                .unwrap()
                .split(',')
                .map(|uid| uid.parse::<u32>().unwrap())
                .collect::<Vec<_>>();
            for uid in requested_body_uids {
                let message = plan
                    .messages
                    .iter()
                    .find(|message| message.uid == uid)
                    .unwrap();
                let sequence = plan
                    .messages
                    .iter()
                    .position(|message| message.uid == uid)
                    .unwrap()
                    + 1;
                let uid_attribute = if plan.omit_body_uid {
                    String::new()
                } else {
                    format!("UID {uid} ")
                };
                let response = format!(
                    "* {sequence} FETCH ({uid_attribute}BODY[] {{{}}}\r\n",
                    message.raw.len()
                );
                stream
                    .get_mut()
                    .write_all(response.as_bytes())
                    .await
                    .unwrap();
                stream.get_mut().write_all(&message.raw).await.unwrap();
                stream.get_mut().write_all(b")\r\n").await.unwrap();
            }
            tagged(&mut stream, &command, "OK body complete").await;
        }
        commands
    }

    async fn run_body_only_server(
        stream: ServerTlsStream<TcpStream>,
        messages: Vec<FixtureMessage>,
        fail_body: bool,
    ) -> Vec<Box<str>> {
        let mut stream = BufReader::new(stream);
        stream
            .get_mut()
            .write_all(b"* OK parallel body fixture ready\r\n")
            .await
            .unwrap();
        let login = read_command(&mut stream).await.unwrap();
        assert_eq!(command_name(&login), Some("LOGIN"));
        tagged(
            &mut stream,
            &login,
            "OK [CAPABILITY IMAP4rev1] authenticated",
        )
        .await;
        let examine = read_command(&mut stream).await.unwrap();
        assert_eq!(command_name(&examine), Some("EXAMINE"));
        stream
            .get_mut()
            .write_all(format!("* {} EXISTS\r\n", messages.len()).as_bytes())
            .await
            .unwrap();
        stream
            .get_mut()
            .write_all(format!("* OK [UIDVALIDITY {UID_VALIDITY}] epoch\r\n").as_bytes())
            .await
            .unwrap();
        let uid_next = messages.last().map_or(1, |message| message.uid + 1);
        stream
            .get_mut()
            .write_all(format!("* OK [UIDNEXT {uid_next}] next\r\n").as_bytes())
            .await
            .unwrap();
        tagged(&mut stream, &examine, "OK [READ-ONLY] selected").await;
        let mut commands = vec![login, examine];

        while let Some(command) = read_command(&mut stream).await {
            commands.push(command.clone());
            if command_name(&command) == Some("LOGOUT") {
                tagged(&mut stream, &command, "OK logout complete").await;
                break;
            }
            assert!(command.contains("UID FETCH"));
            if fail_body {
                break;
            }
            let requested_uids = command
                .split_whitespace()
                .nth(3)
                .unwrap()
                .split(',')
                .map(|uid| uid.parse::<u32>().unwrap());
            for uid in requested_uids {
                let message = messages.iter().find(|message| message.uid == uid).unwrap();
                let response = format!(
                    "* {uid} FETCH (UID {uid} BODY[] {{{}}}\r\n",
                    message.raw.len()
                );
                stream
                    .get_mut()
                    .write_all(response.as_bytes())
                    .await
                    .unwrap();
                stream.get_mut().write_all(&message.raw).await.unwrap();
                stream.get_mut().write_all(b")\r\n").await.unwrap();
            }
            tagged(&mut stream, &command, "OK body complete").await;
        }
        commands
    }

    async fn run_metadata_only_server(
        stream: ServerTlsStream<TcpStream>,
        messages: Vec<FixtureMessage>,
    ) -> Vec<Box<str>> {
        let mut stream = BufReader::new(stream);
        stream
            .get_mut()
            .write_all(b"* OK parallel metadata fixture ready\r\n")
            .await
            .unwrap();
        let login = read_command(&mut stream).await.unwrap();
        assert_eq!(command_name(&login), Some("LOGIN"));
        tagged(
            &mut stream,
            &login,
            "OK [CAPABILITY IMAP4rev1] authenticated",
        )
        .await;
        let examine = read_command(&mut stream).await.unwrap();
        assert_eq!(command_name(&examine), Some("EXAMINE"));
        stream
            .get_mut()
            .write_all(format!("* {} EXISTS\r\n", messages.len()).as_bytes())
            .await
            .unwrap();
        stream
            .get_mut()
            .write_all(format!("* OK [UIDVALIDITY {UID_VALIDITY}] epoch\r\n").as_bytes())
            .await
            .unwrap();
        let uid_next = messages.last().map_or(1, |message| message.uid + 1);
        stream
            .get_mut()
            .write_all(format!("* OK [UIDNEXT {uid_next}] next\r\n").as_bytes())
            .await
            .unwrap();
        tagged(&mut stream, &examine, "OK [READ-ONLY] selected").await;
        let mut commands = vec![login, examine];

        while let Some(command) = read_command(&mut stream).await {
            commands.push(command.clone());
            if command_name(&command) == Some("LOGOUT") {
                tagged(&mut stream, &command, "OK logout complete").await;
                break;
            }
            assert!(command.contains("UID FETCH"));
            assert!(command.contains("HEADER.FIELDS"));
            assert!(!command.contains("BODY.PEEK[]"));
            let requested_uids = command
                .split_whitespace()
                .nth(3)
                .unwrap()
                .split(',')
                .map(|uid| uid.parse::<u32>().unwrap())
                .collect::<Vec<_>>();
            for message in messages
                .iter()
                .filter(|message| requested_uids.contains(&message.uid))
            {
                let sequence = messages
                    .iter()
                    .position(|candidate| candidate.uid == message.uid)
                    .unwrap()
                    + 1;
                write_metadata(&mut stream, sequence, message).await;
            }
            tagged(&mut stream, &command, "OK metadata complete").await;
        }
        commands
    }

    async fn finish(server: TestServer) -> Vec<Box<str>> {
        tokio::time::timeout(TEST_TIMEOUT, server.task)
            .await
            .unwrap()
            .unwrap()
    }

    async fn finish_parallel(server: ParallelTestServer) -> (Vec<Box<str>>, Vec<Box<str>>) {
        tokio::time::timeout(TEST_TIMEOUT, server.task)
            .await
            .unwrap()
            .unwrap()
    }

    fn parallel_messages() -> Vec<FixtureMessage> {
        (1..=8)
            .map(|uid| {
                let raw = vec![b'a'; usize::try_from(uid * 10).unwrap()];
                fixture(uid, &raw)
            })
            .collect()
    }

    fn parallel_limits() -> InboxFetchLimits {
        InboxFetchLimits {
            max_messages: 8,
            max_page_literal_bytes: 8 * 1024,
            body_connections: 2,
            ..limits()
        }
    }

    #[test]
    fn metadata_sync_does_not_issue_body_fetches() {
        run_async(async {
            let server = spawn_server(ServerPlan::messages(vec![
                fixture(1, RAW_ONE),
                fixture(2, RAW_TWO),
            ]))
            .await;
            let page = fetch_canonical_inbox_with_mode(
                request(server.address.port(), None),
                server.connector.clone(),
                limits(),
                None,
                InboxFetchMode::Metadata,
                false,
            )
            .await
            .unwrap();
            assert_eq!(page.messages.len(), 2);
            assert!(
                page.messages
                    .iter()
                    .all(|message| message.content == ImapInboxContent::NotFetched)
            );
            let commands = finish(server).await;
            assert!(
                commands
                    .iter()
                    .all(|command| !command.contains("BODY.PEEK[]"))
            );
        });
    }

    #[test]
    fn idle_notification_reuses_one_selected_tls_session() {
        run_async(async {
            let server = spawn_idle_server().await;
            let deadline = Instant::now() + TEST_TIMEOUT;
            let mut session = open_protocol_session(
                HOST,
                server.address.port(),
                LOGIN,
                &Secret::new(PASSWORD.to_vec()).unwrap(),
                server.connector.clone(),
                deadline,
                limits(),
            )
            .await
            .unwrap();
            let mailbox = session.examine_inbox(deadline).await.unwrap();
            assert_eq!(mailbox.uid_validity, Some(UID_VALIDITY));
            assert!(session.supports_idle());
            assert!(session.idle_until(deadline).await.unwrap());
            session.logout(deadline).await;

            let commands = finish(server).await;
            assert_eq!(
                commands
                    .iter()
                    .filter(|command| command_name(command) == Some("LOGIN"))
                    .count(),
                1
            );
            assert_eq!(command_name(&commands[2]), Some("IDLE"));
            assert_eq!(commands[3].as_ref(), "DONE");
        });
    }

    #[test]
    fn idle_watch_returns_its_session_to_the_bounded_cache() {
        run_async(async {
            let server = spawn_idle_server().await;
            let deadline = Instant::now() + TEST_TIMEOUT;
            let mut session = open_protocol_session(
                HOST,
                server.address.port(),
                LOGIN,
                &Secret::new(PASSWORD.to_vec()).unwrap(),
                server.connector.clone(),
                deadline,
                limits(),
            )
            .await
            .unwrap();
            session.examine_inbox(deadline).await.unwrap();
            let validity = NonZeroU32::new(UID_VALIDITY).unwrap();
            cache_inbox_session(HOST, server.address.port(), LOGIN, validity, session);

            let (cancel, cancelled) = oneshot::channel();
            let outcome = wait_for_cached_inbox_change(
                ImapIdleRequest::new(HOST, server.address.port(), LOGIN, validity).unwrap(),
                cancelled,
            )
            .await;
            drop(cancel);
            assert_eq!(outcome, ImapIdleOutcome::Changed);
            let (sessions, plaintext_buffers) = cached_inbox_session_resources();
            assert_eq!(sessions, 1);
            assert!(plaintext_buffers >= 32 * 1024);

            let session =
                take_cached_inbox_session(HOST, server.address.port(), LOGIN, validity).unwrap();
            session.logout(deadline).await;
            assert_eq!(cached_inbox_session_resources(), (0, 0));
            finish(server).await;
        });
    }

    #[test]
    fn idle_disconnect_drops_the_broken_session_instead_of_leaking_it() {
        run_async(async {
            let server = spawn_idle_disconnect_server().await;
            let deadline = Instant::now() + TEST_TIMEOUT;
            let mut session = open_protocol_session(
                HOST,
                server.address.port(),
                LOGIN,
                &Secret::new(PASSWORD.to_vec()).unwrap(),
                server.connector.clone(),
                deadline,
                limits(),
            )
            .await
            .unwrap();
            session.examine_inbox(deadline).await.unwrap();
            let validity = NonZeroU32::new(UID_VALIDITY).unwrap();
            cache_inbox_session(HOST, server.address.port(), LOGIN, validity, session);

            let (cancel, cancelled) = oneshot::channel();
            let outcome = wait_for_cached_inbox_change(
                ImapIdleRequest::new(HOST, server.address.port(), LOGIN, validity).unwrap(),
                cancelled,
            )
            .await;
            drop(cancel);
            assert_eq!(
                outcome,
                ImapIdleOutcome::Disconnected(ImapInboxFetchFailure::Offline)
            );
            assert_eq!(cached_inbox_session_resources(), (0, 0));
            let commands = finish(server).await;
            assert_eq!(commands.len(), 3);
        });
    }

    #[test]
    fn foreground_cancellation_returns_the_idling_session_to_the_hot_cache() {
        run_async(async {
            let (server, idle_started) = spawn_cancellable_idle_server().await;
            let deadline = Instant::now() + TEST_TIMEOUT;
            let mut session = open_protocol_session(
                HOST,
                server.address.port(),
                LOGIN,
                &Secret::new(PASSWORD.to_vec()).unwrap(),
                server.connector.clone(),
                deadline,
                limits(),
            )
            .await
            .unwrap();
            session.examine_inbox(deadline).await.unwrap();
            let validity = NonZeroU32::new(UID_VALIDITY).unwrap();
            cache_inbox_session(HOST, server.address.port(), LOGIN, validity, session);
            let (cancel, cancelled) = oneshot::channel();
            let request =
                ImapIdleRequest::new(HOST, server.address.port(), LOGIN, validity).unwrap();
            let idle_task = tokio::spawn(wait_for_cached_inbox_change(request, cancelled));

            idle_started.await.unwrap();
            cancel.send(()).unwrap();
            assert_eq!(idle_task.await.unwrap(), ImapIdleOutcome::Cancelled);
            assert_eq!(cached_inbox_session_resources().0, 1);

            let session =
                take_cached_inbox_session(HOST, server.address.port(), LOGIN, validity).unwrap();
            session.logout(deadline).await;
            let commands = finish(server).await;
            assert_eq!(
                commands
                    .iter()
                    .filter(|command| command_name(command) == Some("LOGIN"))
                    .count(),
                1
            );
        });
    }

    #[test]
    fn metadata_sync_keeps_small_headers_on_one_selected_session() {
        run_async(async {
            let mut messages = parallel_messages();
            messages.push(fixture(9, b"nine"));
            let server = spawn_server(ServerPlan::messages(messages)).await;
            let page = fetch_canonical_inbox_with_mode(
                request_from(server.address.port(), 2, Some(UID_VALIDITY)),
                server.connector.clone(),
                parallel_limits(),
                None,
                InboxFetchMode::Metadata,
                false,
            )
            .await
            .unwrap();
            assert_eq!(page.messages.len(), 8);
            assert!(
                page.messages
                    .iter()
                    .all(|message| message.content == ImapInboxContent::NotFetched)
            );
            let commands = finish(server).await;
            assert_eq!(
                commands
                    .iter()
                    .filter(|command| command.contains("HEADER.FIELDS"))
                    .count(),
                1
            );
            assert!(
                commands
                    .iter()
                    .all(|command| !command.contains("BODY.PEEK[]"))
            );
        });
    }

    #[test]
    fn on_demand_content_fetch_requests_only_the_selected_uid() {
        run_async(async {
            let server = spawn_body_server(vec![fixture(7, RAW_TWO)]).await;
            let body = fetch_imap_message_content_with_connector(
                content_request(server.address.port(), 7),
                server.connector.clone(),
                limits(),
            )
            .await
            .unwrap();
            assert_eq!(body.as_ref(), RAW_TWO);
            let commands = finish(server).await;
            let body_commands = commands
                .iter()
                .filter(|command| command.contains("BODY.PEEK[]"))
                .collect::<Vec<_>>();
            assert_eq!(body_commands.len(), 1);
            assert!(body_commands[0].contains("UID FETCH 7 (UID BODY.PEEK[])"));
        });
    }

    #[test]
    fn on_demand_content_reuses_the_selected_metadata_session() {
        run_async(async {
            let server = spawn_server(ServerPlan::messages(vec![fixture(2, RAW_TWO)])).await;
            let page = fetch_canonical_inbox_with_mode(
                request(server.address.port(), None),
                server.connector.clone(),
                limits(),
                None,
                InboxFetchMode::Metadata,
                true,
            )
            .await
            .unwrap();
            assert_eq!(page.messages.len(), 1);

            let body = fetch_imap_message_content_inner(
                content_request(server.address.port(), 2),
                server.connector.clone(),
                limits(),
                true,
            )
            .await
            .unwrap();
            assert_eq!(body.as_ref(), RAW_TWO);

            let session = take_cached_inbox_session(
                HOST,
                server.address.port(),
                LOGIN,
                NonZeroU32::new(UID_VALIDITY).unwrap(),
            )
            .unwrap();
            session.logout(Instant::now() + LOGOUT_TIMEOUT).await;
            let commands = finish(server).await;
            assert_eq!(
                commands
                    .iter()
                    .filter(|command| command_name(command) == Some("LOGIN"))
                    .count(),
                1
            );
            assert_eq!(
                commands
                    .iter()
                    .filter(|command| command.contains("BODY.PEEK[]"))
                    .count(),
                1
            );
        });
    }

    #[test]
    fn incremental_metadata_sync_reuses_the_selected_session() {
        run_async(async {
            let server = spawn_server(ServerPlan::messages(vec![fixture(2, RAW_TWO)])).await;
            let first = fetch_canonical_inbox_with_mode(
                request(server.address.port(), None),
                server.connector.clone(),
                limits(),
                None,
                InboxFetchMode::Metadata,
                true,
            )
            .await
            .unwrap();
            assert_eq!(first.messages.len(), 1);

            let incremental = fetch_canonical_inbox_with_mode(
                request_from(server.address.port(), 3, Some(UID_VALIDITY)),
                server.connector.clone(),
                limits(),
                None,
                InboxFetchMode::Metadata,
                true,
            )
            .await
            .unwrap();
            assert!(incremental.messages.is_empty());

            let session = take_cached_inbox_session(
                HOST,
                server.address.port(),
                LOGIN,
                NonZeroU32::new(UID_VALIDITY).unwrap(),
            )
            .unwrap();
            session.logout(Instant::now() + LOGOUT_TIMEOUT).await;
            let commands = finish(server).await;
            assert_eq!(
                commands
                    .iter()
                    .filter(|command| command_name(command) == Some("LOGIN"))
                    .count(),
                1
            );
            assert_eq!(
                commands
                    .iter()
                    .filter(|command| command_name(command) == Some("EXAMINE"))
                    .count(),
                2
            );
        });
    }

    #[test]
    fn parallel_body_fetch_uses_two_bounded_selected_sessions() {
        run_async(async {
            let server = spawn_parallel_server(parallel_messages(), false).await;
            let page = fetch_canonical_inbox_with_connector(
                request(server.address.port(), None),
                server.connector.clone(),
                parallel_limits(),
                None,
            )
            .await
            .unwrap();
            assert_eq!(page.messages.len(), 8);
            assert!(
                page.messages
                    .iter()
                    .all(|message| matches!(message.content, ImapInboxContent::Fetched(_)))
            );
            let (primary, secondary) = finish_parallel(server).await;
            assert_eq!(
                primary
                    .iter()
                    .filter(|command| command.contains("BODY.PEEK[]"))
                    .count(),
                1
            );
            assert_eq!(
                secondary
                    .iter()
                    .filter(|command| command.contains("BODY.PEEK[]"))
                    .count(),
                1
            );
        });
    }

    #[test]
    fn parallel_body_failure_retries_the_unfinished_partition_on_primary() {
        run_async(async {
            let server = spawn_parallel_server(parallel_messages(), true).await;
            let page = fetch_canonical_inbox_with_connector(
                request(server.address.port(), None),
                server.connector.clone(),
                parallel_limits(),
                None,
            )
            .await
            .unwrap();
            assert_eq!(page.messages.len(), 8);
            let (primary, secondary) = finish_parallel(server).await;
            assert_eq!(
                primary
                    .iter()
                    .filter(|command| command.contains("BODY.PEEK[]"))
                    .count(),
                2
            );
            assert_eq!(
                secondary
                    .iter()
                    .filter(|command| command.contains("BODY.PEEK[]"))
                    .count(),
                1
            );
        });
    }

    #[test]
    fn fetches_a_bounded_page_and_handles_an_empty_inbox() {
        run_async(async {
            let server = spawn_server(ServerPlan::messages(vec![
                fixture(1, RAW_ONE),
                fixture(2, RAW_TWO),
            ]))
            .await;
            let page = fetch_canonical_inbox_with_connector(
                request(server.address.port(), Some(UID_VALIDITY)),
                server.connector.clone(),
                limits(),
                None,
            )
            .await
            .unwrap();
            assert_eq!(page.uid_validity.get(), UID_VALIDITY);
            assert_eq!(page.uid_next.get(), 3);
            assert_eq!(page.scanned_through_uid.unwrap().get(), 2);
            assert_eq!(page.next_uid, None);
            assert_eq!(page.messages.len(), 2);
            assert_eq!(page.messages[0].uid.get(), 1);
            assert!(page.messages[0].flags.seen);
            assert!(page.messages[0].flags.flagged);
            assert_eq!(page.messages[0].envelope.subject.as_ref(), b"Message 1");
            assert_eq!(
                page.messages[0].content,
                ImapInboxContent::Fetched(RAW_ONE.into())
            );
            let commands = finish(server).await;
            assert_eq!(
                commands
                    .iter()
                    .filter_map(|command| command_name(command))
                    .collect::<Vec<_>>(),
                ["LOGIN", "EXAMINE", "UID", "UID", "UID", "LOGOUT"]
            );
            assert!(
                commands
                    .iter()
                    .any(|command| { command.contains("UID FETCH 1,2 (UID BODY.PEEK[])") })
            );

            let empty = spawn_server(ServerPlan::messages(Vec::new())).await;
            let page = fetch_canonical_inbox_with_connector(
                request(empty.address.port(), None),
                empty.connector.clone(),
                limits(),
                None,
            )
            .await
            .unwrap();
            assert!(page.messages.is_empty());
            assert_eq!(page.scanned_through_uid, None);
            assert_eq!(page.next_uid, None);
            assert_eq!(finish(empty).await.len(), 3);

            let mut historical_empty_plan = ServerPlan::messages(Vec::new());
            historical_empty_plan.uid_next_override = Some(1000);
            let historical_empty = spawn_server(historical_empty_plan).await;
            let page = fetch_canonical_inbox_with_connector(
                request_from(historical_empty.address.port(), 101, Some(UID_VALIDITY)),
                historical_empty.connector.clone(),
                limits(),
                None,
            )
            .await
            .unwrap();
            assert_eq!(page.uid_next.get(), 1000);
            assert_eq!(page.scanned_through_uid.unwrap().get(), 999);
            assert_eq!(page.next_uid, None);
            assert_eq!(finish(historical_empty).await.len(), 3);
        });
    }

    #[test]
    fn accepts_single_uid_body_without_uid_and_with_a_canonical_size_difference() {
        run_async(async {
            let mut message = fixture(1, RAW_ONE);
            message.declared_bytes = message.declared_bytes.saturating_add(12);
            let mut plan = ServerPlan::messages(vec![message]);
            plan.omit_body_uid = true;
            let server = spawn_server(plan).await;

            let page = fetch_canonical_inbox_with_connector(
                request(server.address.port(), None),
                server.connector.clone(),
                limits(),
                None,
            )
            .await
            .unwrap();

            assert_eq!(page.messages.len(), 1);
            assert_eq!(page.messages[0].uid.get(), 1);
            assert_eq!(page.messages[0].declared_bytes, RAW_ONE.len() as u32 + 12);
            assert_eq!(
                page.messages[0].content,
                ImapInboxContent::Fetched(RAW_ONE.into())
            );
            finish(server).await;

            let wrong_uid = ProjectedFetch {
                uid: Some(2),
                body: Some(RAW_ONE.into()),
                ..ProjectedFetch::default()
            };
            assert!(matches!(
                parse_body(wrong_uid, NonZeroU32::new(1).unwrap()),
                Err(ImapInboxFetchFailure::Protocol)
            ));
        });
    }

    #[test]
    fn discovers_sparse_uids_and_starts_a_new_account_from_recent_mail() {
        run_async(async {
            let messages = || {
                vec![
                    fixture(21, RAW_ONE),
                    fixture(105, RAW_TWO),
                    fixture(400, RAW_ONE),
                    fixture(900, RAW_TWO),
                ]
            };
            let initial = spawn_server(ServerPlan::messages(messages())).await;
            let page = fetch_canonical_inbox_with_connector(
                request(initial.address.port(), None),
                initial.connector.clone(),
                limits(),
                None,
            )
            .await
            .unwrap();
            assert_eq!(
                page.messages
                    .iter()
                    .map(|message| message.uid.get())
                    .collect::<Vec<_>>(),
                [400, 900]
            );
            assert_eq!(page.scanned_through_uid.unwrap().get(), 900);
            assert_eq!(page.next_uid, None);
            finish(initial).await;

            let staged_retry = spawn_server(ServerPlan::messages(messages())).await;
            let page = fetch_canonical_inbox_with_connector(
                request(staged_retry.address.port(), Some(UID_VALIDITY)),
                staged_retry.connector.clone(),
                limits(),
                None,
            )
            .await
            .unwrap();
            assert_eq!(
                page.messages
                    .iter()
                    .map(|message| message.uid.get())
                    .collect::<Vec<_>>(),
                [400, 900]
            );
            assert_eq!(page.scanned_through_uid.unwrap().get(), 900);
            finish(staged_retry).await;

            let incremental = spawn_server(ServerPlan::messages(messages())).await;
            let page = fetch_canonical_inbox_with_connector(
                request_from(incremental.address.port(), 105, Some(UID_VALIDITY)),
                incremental.connector.clone(),
                limits(),
                None,
            )
            .await
            .unwrap();
            assert_eq!(
                page.messages
                    .iter()
                    .map(|message| message.uid.get())
                    .collect::<Vec<_>>(),
                [105, 400]
            );
            assert_eq!(page.scanned_through_uid.unwrap().get(), 400);
            assert_eq!(page.next_uid.unwrap().get(), 900);
            finish(incremental).await;
        });
    }

    #[test]
    fn recent_first_bootstrap_continues_backwards_until_history_is_complete() {
        run_async(async {
            let messages = || {
                (1..=6)
                    .map(|uid| fixture(uid, if uid % 2 == 0 { RAW_TWO } else { RAW_ONE }))
                    .collect::<Vec<_>>()
            };

            let initial = spawn_server(ServerPlan::messages(messages())).await;
            let page = fetch_canonical_inbox_with_connector(
                request(initial.address.port(), None),
                initial.connector.clone(),
                limits(),
                None,
            )
            .await
            .unwrap();
            assert_eq!(
                page.messages
                    .iter()
                    .map(|message| message.uid.get())
                    .collect::<Vec<_>>(),
                [5, 6]
            );
            assert_eq!(page.scanned_through_uid.unwrap().get(), 6);
            assert_eq!(
                page.bootstrap_history,
                Some(ImapBootstrapHistory {
                    next_cursor: NonZeroU32::new(4),
                    complete: false,
                })
            );
            finish(initial).await;

            let middle = spawn_server(ServerPlan::messages(messages())).await;
            let page = fetch_canonical_inbox_with_connector(
                request_with_history(middle.address.port(), 7, Some(UID_VALIDITY), Some(4), false),
                middle.connector.clone(),
                limits(),
                None,
            )
            .await
            .unwrap();
            assert_eq!(
                page.messages
                    .iter()
                    .map(|message| message.uid.get())
                    .collect::<Vec<_>>(),
                [4, 3]
            );
            assert_eq!(
                page.history_page,
                Some(ImapHistoryPage {
                    expected_cursor: NonZeroU32::new(4).unwrap(),
                    next_cursor: NonZeroU32::new(2),
                    complete: false,
                })
            );
            finish(middle).await;

            let oldest = spawn_server(ServerPlan::messages(messages())).await;
            let page = fetch_canonical_inbox_with_connector(
                request_with_history(oldest.address.port(), 7, Some(UID_VALIDITY), Some(2), false),
                oldest.connector.clone(),
                limits(),
                None,
            )
            .await
            .unwrap();
            assert_eq!(
                page.messages
                    .iter()
                    .map(|message| message.uid.get())
                    .collect::<Vec<_>>(),
                [2, 1]
            );
            assert_eq!(
                page.history_page,
                Some(ImapHistoryPage {
                    expected_cursor: NonZeroU32::new(2).unwrap(),
                    next_cursor: None,
                    complete: true,
                })
            );
            finish(oldest).await;
        });
    }

    #[test]
    fn rejects_missing_uidvalidity_uidnext_or_changed_epoch() {
        run_async(async {
            let mut missing_plan = ServerPlan::messages(vec![fixture(1, RAW_ONE)]);
            missing_plan.uid_validity = None;
            let missing = spawn_server(missing_plan).await;
            assert_eq!(
                fetch_canonical_inbox_with_connector(
                    request(missing.address.port(), None),
                    missing.connector.clone(),
                    limits(),
                    None,
                )
                .await,
                Err(ImapInboxFetchFailure::MissingUidValidity)
            );
            finish(missing).await;

            let mut missing_next_plan = ServerPlan::messages(vec![fixture(1, RAW_ONE)]);
            missing_next_plan.omit_uid_next = true;
            let missing_next = spawn_server(missing_next_plan).await;
            assert_eq!(
                fetch_canonical_inbox_with_connector(
                    request(missing_next.address.port(), None),
                    missing_next.connector.clone(),
                    limits(),
                    None,
                )
                .await,
                Err(ImapInboxFetchFailure::MissingUidNext)
            );
            finish(missing_next).await;

            let changed = spawn_server(ServerPlan::messages(vec![fixture(1, RAW_ONE)])).await;
            assert_eq!(
                fetch_canonical_inbox_with_connector(
                    request(changed.address.port(), Some(UID_VALIDITY + 1)),
                    changed.connector.clone(),
                    limits(),
                    None,
                )
                .await,
                Err(ImapInboxFetchFailure::UidValidityChanged {
                    expected: UID_VALIDITY + 1,
                    actual: UID_VALIDITY,
                })
            );
            finish(changed).await;
        });
    }

    #[test]
    fn reports_tagged_failures_and_response_count_limits() {
        run_async(async {
            for (status, expected) in [
                ("NO fetch denied", ImapInboxFetchFailure::Permission),
                ("BAD invalid fetch", ImapInboxFetchFailure::Protocol),
            ] {
                let mut plan = ServerPlan::messages(vec![fixture(1, RAW_ONE)]);
                plan.metadata_status = Some(status);
                let server = spawn_server(plan).await;
                assert_eq!(
                    fetch_canonical_inbox_with_connector(
                        request(server.address.port(), None),
                        server.connector.clone(),
                        limits(),
                        None,
                    )
                    .await,
                    Err(expected)
                );
                finish(server).await;
            }

            let mut plan = ServerPlan::messages(vec![fixture(1, RAW_ONE), fixture(2, RAW_TWO)]);
            plan.extra_metadata = 1;
            let server = spawn_server(plan).await;
            assert_eq!(
                fetch_canonical_inbox_with_connector(
                    request(server.address.port(), None),
                    server.connector.clone(),
                    limits(),
                    None,
                )
                .await,
                Err(ImapInboxFetchFailure::ResourceLimit)
            );
            finish(server).await;
        });
    }

    #[test]
    fn marks_single_message_oversize_and_defers_page_overflow() {
        run_async(async {
            let mut oversized_message = fixture(1, RAW_ONE);
            oversized_message.declared_bytes = 1025;
            let oversized = spawn_server(ServerPlan::messages(vec![oversized_message])).await;
            let page = fetch_canonical_inbox_with_connector(
                request(oversized.address.port(), None),
                oversized.connector.clone(),
                limits(),
                None,
            )
            .await
            .unwrap();
            assert_eq!(
                page.messages[0].content,
                ImapInboxContent::Oversized {
                    declared_bytes: 1025
                }
            );
            assert_eq!(page.scanned_through_uid.unwrap().get(), 1);
            assert_eq!(page.next_uid, None);
            assert_eq!(finish(oversized).await.len(), 5);

            let raw = vec![b'x'; 1025];
            let mut understated_message = fixture(1, &raw);
            understated_message.declared_bytes = 1;
            let understated = spawn_server(ServerPlan::messages(vec![understated_message])).await;
            assert_eq!(
                fetch_canonical_inbox_with_connector(
                    request(understated.address.port(), None),
                    understated.connector.clone(),
                    limits(),
                    None,
                )
                .await,
                Err(ImapInboxFetchFailure::ResourceLimit)
            );
            finish(understated).await;

            let server = spawn_server(ServerPlan::messages(vec![
                fixture(1, RAW_ONE),
                fixture(2, RAW_TWO),
            ]))
            .await;
            let mut page_limits = limits();
            page_limits.max_page_literal_bytes = RAW_ONE.len();
            let page = fetch_canonical_inbox_with_connector(
                request(server.address.port(), None),
                server.connector.clone(),
                page_limits,
                None,
            )
            .await
            .unwrap();
            assert_eq!(page.messages.len(), 1);
            assert_eq!(page.scanned_through_uid.unwrap().get(), 1);
            assert_eq!(page.next_uid.unwrap().get(), 2);
            let commands = finish(server).await;
            assert_eq!(
                commands
                    .iter()
                    .filter(|command| command.contains("BODY.PEEK[]"))
                    .count(),
                1
            );
        });
    }

    #[test]
    fn returns_a_fixed_error_when_cancelled_during_a_body() {
        run_async(async {
            let (reached_tx, reached_rx) = oneshot::channel();
            let mut plan = ServerPlan::messages(vec![fixture(1, RAW_ONE)]);
            plan.stall_body = Some(reached_tx);
            let server = spawn_server(plan).await;
            let (cancel, cancellation) = imap_inbox_fetch_cancellation_pair();
            let fetch = tokio::spawn(fetch_canonical_inbox_with_connector(
                request(server.address.port(), None),
                server.connector.clone(),
                limits(),
                Some(cancellation),
            ));
            tokio::time::timeout(TEST_TIMEOUT, reached_rx)
                .await
                .unwrap()
                .unwrap();
            cancel.cancel();
            assert_eq!(fetch.await.unwrap(), Err(ImapInboxFetchFailure::Cancelled));
            finish(server).await;
        });
    }
}
