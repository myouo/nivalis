use std::{
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
    imap_proto::types::{AttributeValue, Response, Status},
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

const MAX_HOST_BYTES: usize = 253;
const MAX_LOGIN_BYTES: usize = 320;
const MAX_SERVER_BYTES: usize = 256 * 1024;
const MAX_CLIENT_BYTES: usize = 64 * 1024;
const DIAGNOSTIC_TIMEOUT: Duration = Duration::from_secs(30);
const LOGOUT_TIMEOUT: Duration = Duration::from_secs(1);
const INBOX_FETCH_TIMEOUT: Duration = Duration::from_secs(45);
const MAX_INBOX_MESSAGES: usize = 32;
const MAX_INBOX_LITERAL_BYTES: usize = 1024 * 1024;
const MAX_INBOX_PAGE_LITERAL_BYTES: usize = 4 * 1024 * 1024;
const MAX_INBOX_SERVER_BYTES: usize = MAX_INBOX_PAGE_LITERAL_BYTES + 512 * 1024;
const MAX_INBOX_CLIENT_BYTES: usize = 64 * 1024;
const MAX_INTERNAL_DATE_BYTES: usize = 64;
const MAX_ENVELOPE_SUBJECT_BYTES: usize = 998;
const MAX_ENVELOPE_NAME_BYTES: usize = 320;
const MAX_ENVELOPE_MAILBOX_BYTES: usize = 320;
const MAX_ENVELOPE_HOST_BYTES: usize = 253;
const MAX_ENVELOPE_MESSAGE_ID_BYTES: usize = 998;

static PLATFORM_CONFIG: OnceLock<Arc<ClientConfig>> = OnceLock::new();

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
    pub(crate) subject: Box<[u8]>,
    pub(crate) from_name: Box<[u8]>,
    pub(crate) from_mailbox: Box<[u8]>,
    pub(crate) from_host: Box<[u8]>,
    pub(crate) message_id: Box<[u8]>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ImapInboxContent {
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
    if std::str::from_utf8(secret.expose()).is_err() {
        return Err(ImapDiagnosticInputError::SecretEncoding);
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
    let operation = fetch_canonical_inbox_inner(request, connector, limits);
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

async fn fetch_canonical_inbox_inner(
    request: ImapInboxFetchRequest,
    connector: TlsConnector,
    limits: InboxFetchLimits,
) -> Result<ImapInboxPage, ImapInboxFetchFailure> {
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
    .await
    .map_err(map_diagnostic_failure)?;
    require_imap_capability(&mut session, login_capabilities, deadline)
        .await
        .map_err(map_diagnostic_failure)?;

    let mailbox = match timeout_at(deadline, session.examine("INBOX")).await {
        Ok(Ok(mailbox)) => mailbox,
        Ok(Err(error)) => return Err(map_receive_imap_error(&error, true)),
        Err(_) => return Err(ImapInboxFetchFailure::Timeout),
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
    let uid_next = mailbox
        .uid_next
        .and_then(NonZeroU32::new)
        .ok_or(ImapInboxFetchFailure::MissingUidNext)?;
    let first_uid = request.first_uid.get();
    let initial_snapshot = first_uid == 1;
    let mut selection = if mailbox.exists == 0 || uid_next.get() <= first_uid {
        let scanned_through = first_uid
            .saturating_sub(1)
            .max(uid_next.get().saturating_sub(1));
        UidSelection {
            uids: Vec::new(),
            scanned_through_uid: NonZeroU32::new(scanned_through),
            next_uid: None,
            bootstrap_history: initial_snapshot.then_some(ImapBootstrapHistory {
                next_cursor: None,
                complete: true,
            }),
            history_page: None,
        }
    } else {
        // A missing cursor always maps to UID 1, including retries after metadata
        // staging has persisted UIDVALIDITY but content publication did not finish.
        search_uids(
            &mut session,
            first_uid,
            uid_next,
            initial_snapshot,
            deadline,
            limits,
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
        let _ = tokio::time::timeout(LOGOUT_TIMEOUT, session.logout()).await;
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

    let mut metadata = fetch_metadata(&mut session, &selection.uids, deadline, limits).await?;
    metadata.sort_unstable_by_key(|message| message.uid);
    if selection.history_page.is_some() {
        metadata.reverse();
    }

    let mut messages = Vec::with_capacity(metadata.len());
    let mut literal_bytes = 0_usize;
    let mut deferred_uid = None;
    for metadata in metadata {
        let declared_bytes = usize::try_from(metadata.declared_bytes)
            .map_err(|_| ImapInboxFetchFailure::ResourceLimit)?;
        let content = if declared_bytes > limits.max_message_literal_bytes {
            ImapInboxContent::Oversized {
                declared_bytes: metadata.declared_bytes,
            }
        } else {
            let next_total = literal_bytes
                .checked_add(declared_bytes)
                .ok_or(ImapInboxFetchFailure::ResourceLimit)?;
            if next_total > limits.max_page_literal_bytes {
                deferred_uid = Some(metadata.uid);
                break;
            }
            let raw = fetch_body(&mut session, metadata.uid, deadline, limits).await?;
            literal_bytes = literal_bytes
                .checked_add(raw.len())
                .ok_or(ImapInboxFetchFailure::ResourceLimit)?;
            if literal_bytes > limits.max_page_literal_bytes {
                return Err(ImapInboxFetchFailure::ResourceLimit);
            }
            ImapInboxContent::Fetched(raw)
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
    let _ = tokio::time::timeout(LOGOUT_TIMEOUT, session.logout()).await;
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

#[derive(Clone, Copy)]
struct InboxFetchLimits {
    timeout: Duration,
    max_server_bytes: usize,
    max_client_bytes: usize,
    max_messages: usize,
    max_message_literal_bytes: usize,
    max_page_literal_bytes: usize,
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
        }
    }
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
    scanned_through_uid: Option<NonZeroU32>,
    next_uid: Option<NonZeroU32>,
    bootstrap_history: Option<ImapBootstrapHistory>,
    history_page: Option<ImapHistoryPage>,
}

async fn search_uids(
    session: &mut AuthenticatedSession,
    first_uid: u32,
    uid_next: NonZeroU32,
    initial_snapshot: bool,
    deadline: Instant,
    limits: InboxFetchLimits,
) -> Result<UidSelection, ImapInboxFetchFailure> {
    if limits.max_messages == 0 {
        return Err(ImapInboxFetchFailure::ResourceLimit);
    }
    let found = match timeout_at(deadline, session.uid_search(format!("UID {first_uid}:*"))).await {
        Ok(Ok(found)) => found,
        Ok(Err(error)) => return Err(map_receive_imap_error(&error, false)),
        Err(_) => return Err(ImapInboxFetchFailure::Timeout),
    };
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
        scanned_through_uid,
        next_uid,
        bootstrap_history,
        history_page: None,
    })
}

async fn search_history_uids(
    session: &mut AuthenticatedSession,
    expected_cursor: NonZeroU32,
    history_cursor: NonZeroU32,
    deadline: Instant,
    limits: InboxFetchLimits,
) -> Result<UidSelection, ImapInboxFetchFailure> {
    if limits.max_messages == 0 {
        return Err(ImapInboxFetchFailure::ResourceLimit);
    }
    let found = match timeout_at(
        deadline,
        session.uid_search(format!("UID 1:{}", history_cursor.get())),
    )
    .await
    {
        Ok(Ok(found)) => found,
        Ok(Err(error)) => return Err(map_receive_imap_error(&error, false)),
        Err(_) => return Err(ImapInboxFetchFailure::Timeout),
    };
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
    session: &mut AuthenticatedSession,
    uids: &[NonZeroU32],
    deadline: Instant,
    limits: InboxFetchLimits,
) -> Result<Vec<InboxMetadata>, ImapInboxFetchFailure> {
    let uid_set = uids
        .iter()
        .map(|uid| uid.get())
        .map(|uid| uid.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let command = format!("UID FETCH {uid_set} (UID FLAGS INTERNALDATE RFC822.SIZE ENVELOPE)");
    let id = match timeout_at(deadline, session.run_command(command)).await {
        Ok(Ok(id)) => id,
        Ok(Err(error)) => return Err(map_receive_imap_error(&error, false)),
        Err(_) => return Err(ImapInboxFetchFailure::Timeout),
    };
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
    Ok(messages)
}

async fn fetch_body(
    session: &mut AuthenticatedSession,
    uid: NonZeroU32,
    deadline: Instant,
    limits: InboxFetchLimits,
) -> Result<Box<[u8]>, ImapInboxFetchFailure> {
    let id = match timeout_at(
        deadline,
        session.run_command(format!("UID FETCH {} (UID BODY.PEEK[])", uid.get())),
    )
    .await
    {
        Ok(Ok(id)) => id,
        Ok(Err(error)) => return Err(map_receive_imap_error(&error, false)),
        Err(_) => return Err(ImapInboxFetchFailure::Timeout),
    };
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
    session: &mut AuthenticatedSession,
    deadline: Instant,
) -> Result<ReceiveResponse, ImapInboxFetchFailure> {
    match timeout_at(deadline, session.read_response()).await {
        Ok(Ok(Some(response))) => project_receive_response(response.parsed()),
        Ok(Ok(None)) => Err(ImapInboxFetchFailure::Offline),
        Ok(Err(error)) => Err(map_receive_io_error(&error)),
        Err(_) => Err(ImapInboxFetchFailure::Timeout),
    }
}

enum ReceiveResponse {
    Done {
        tag: async_imap::imap_proto::RequestId,
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
    PreAuth,
    Bye,
}

#[derive(Default)]
struct ProjectedFetch {
    uid: Option<u32>,
    flags: Option<ImapInboxFlags>,
    internal_date: Option<Box<str>>,
    declared_bytes: Option<u32>,
    envelope: Option<ImapInboxEnvelope>,
    body: Option<Box<[u8]>>,
}

fn project_receive_response(
    response: &Response<'_>,
) -> Result<ReceiveResponse, ImapInboxFetchFailure> {
    match response {
        Response::Done { tag, status, .. } => Ok(ReceiveResponse::Done {
            tag: tag.clone(),
            status: project_status(status),
        }),
        Response::Fetch(_, attributes) => {
            let mut fetch = ProjectedFetch::default();
            for attribute in attributes {
                project_fetch_attribute(&mut fetch, attribute)?;
            }
            Ok(ReceiveResponse::Fetch(fetch))
        }
        Response::Data {
            status: Status::Bye,
            ..
        } => Ok(ReceiveResponse::Bye),
        _ => Ok(ReceiveResponse::Other),
    }
}

fn project_status(status: &Status) -> ReceiveStatus {
    match status {
        Status::Ok => ReceiveStatus::Ok,
        Status::No => ReceiveStatus::No,
        Status::Bad => ReceiveStatus::Bad,
        Status::PreAuth => ReceiveStatus::PreAuth,
        Status::Bye => ReceiveStatus::Bye,
    }
}

fn project_fetch_attribute(
    fetch: &mut ProjectedFetch,
    attribute: &AttributeValue<'_>,
) -> Result<(), ImapInboxFetchFailure> {
    match attribute {
        AttributeValue::Uid(value) => set_once(&mut fetch.uid, *value)?,
        AttributeValue::Flags(values) => {
            let parsed = ImapInboxFlags {
                seen: has_flag(values, "\\Seen"),
                flagged: has_flag(values, "\\Flagged"),
                answered: has_flag(values, "\\Answered"),
                draft: has_flag(values, "\\Draft"),
                deleted: has_flag(values, "\\Deleted"),
            };
            set_once(&mut fetch.flags, parsed)?;
        }
        AttributeValue::InternalDate(value) => {
            if value.len() > MAX_INTERNAL_DATE_BYTES {
                return Err(ImapInboxFetchFailure::ResourceLimit);
            }
            set_once(&mut fetch.internal_date, value.to_string().into_boxed_str())?;
        }
        AttributeValue::Rfc822Size(value) => set_once(&mut fetch.declared_bytes, *value)?,
        AttributeValue::Envelope(value) => {
            set_once(&mut fetch.envelope, project_envelope(value)?)?;
        }
        AttributeValue::BodySection {
            section: None,
            data: Some(bytes),
            ..
        }
        | AttributeValue::Rfc822(Some(bytes)) => {
            set_once(&mut fetch.body, bytes.as_ref().into())?;
        }
        _ => {}
    }
    Ok(())
}

fn parse_metadata(fetch: ProjectedFetch) -> Result<InboxMetadata, ImapInboxFetchFailure> {
    if fetch.body.is_some() {
        return Err(ImapInboxFetchFailure::Protocol);
    }
    Ok(InboxMetadata {
        uid: fetch
            .uid
            .and_then(NonZeroU32::new)
            .ok_or(ImapInboxFetchFailure::Protocol)?,
        flags: fetch.flags.ok_or(ImapInboxFetchFailure::Protocol)?,
        internal_date: fetch.internal_date.ok_or(ImapInboxFetchFailure::Protocol)?,
        envelope: fetch.envelope.ok_or(ImapInboxFetchFailure::Protocol)?,
        declared_bytes: fetch
            .declared_bytes
            .ok_or(ImapInboxFetchFailure::Protocol)?,
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

fn has_flag(flags: &[std::borrow::Cow<'_, str>], expected: &str) -> bool {
    flags
        .iter()
        .any(|flag| flag.as_ref().eq_ignore_ascii_case(expected))
}

fn project_envelope(
    envelope: &async_imap::imap_proto::types::Envelope<'_>,
) -> Result<ImapInboxEnvelope, ImapInboxFetchFailure> {
    let from = envelope
        .from
        .as_ref()
        .and_then(|addresses| addresses.first());
    Ok(ImapInboxEnvelope {
        subject: bounded_envelope_bytes(envelope.subject.as_deref(), MAX_ENVELOPE_SUBJECT_BYTES)?,
        from_name: bounded_envelope_bytes(
            from.and_then(|address| address.name.as_deref()),
            MAX_ENVELOPE_NAME_BYTES,
        )?,
        from_mailbox: bounded_envelope_bytes(
            from.and_then(|address| address.mailbox.as_deref()),
            MAX_ENVELOPE_MAILBOX_BYTES,
        )?,
        from_host: bounded_envelope_bytes(
            from.and_then(|address| address.host.as_deref()),
            MAX_ENVELOPE_HOST_BYTES,
        )?,
        message_id: bounded_envelope_bytes(
            envelope.message_id.as_deref(),
            MAX_ENVELOPE_MESSAGE_ID_BYTES,
        )?,
    })
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
        ReceiveStatus::Bad | ReceiveStatus::PreAuth => Err(ImapInboxFetchFailure::Protocol),
        ReceiveStatus::Bye => Err(ImapInboxFetchFailure::Offline),
    }
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
        }
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
        let response = format!(
            "* {sequence} FETCH (UID {} FLAGS (\\Seen \\Flagged) INTERNALDATE \"20-Jul-2026 12:00:00 +0000\" RFC822.SIZE {} ENVELOPE (\"20 Jul 2026 12:00:00 +0000\" \"Message {}\" ((\"Sender {}\" NIL \"sender\" \"example.test\")) NIL NIL NIL NIL NIL NIL \"<{}@example.test>\"))\r\n",
            message.uid, message.declared_bytes, message.uid, message.uid, message.uid,
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

        if !plan.messages.is_empty() {
            let Some(search) = read_command(&mut stream).await else {
                return commands;
            };
            assert!(search.contains("UID SEARCH UID "));
            commands.push(search.clone());
            let first_uid = search
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
            tagged(&mut stream, &search, "OK search complete").await;
        }

        let Some(metadata) = read_command(&mut stream).await else {
            return commands;
        };
        commands.push(metadata.clone());
        if command_name(&metadata) == Some("LOGOUT") {
            tagged(&mut stream, &metadata, "OK logout complete").await;
            return commands;
        }
        assert!(metadata.contains("UID FETCH "));
        if let Some(status) = plan.metadata_status {
            tagged(&mut stream, &metadata, status).await;
            let _ = read_command(&mut stream).await;
            return commands;
        }
        let requested_uids = metadata
            .split_whitespace()
            .nth(3)
            .unwrap()
            .split(',')
            .map(|uid| uid.parse::<u32>().unwrap())
            .collect::<Vec<_>>();
        for message in plan
            .messages
            .iter()
            .filter(|message| requested_uids.contains(&message.uid))
        {
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
            assert!(command.contains("UID FETCH"));
            if let Some(reached) = plan.stall_body.take() {
                let _ = reached.send(());
                let _ = read_command(&mut stream).await;
                break;
            }
            let uid: u32 = command.split_whitespace().nth(3).unwrap().parse().unwrap();
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
            tagged(&mut stream, &command, "OK body complete").await;
        }
        commands
    }

    async fn finish(server: TestServer) -> Vec<Box<str>> {
        tokio::time::timeout(TEST_TIMEOUT, server.task)
            .await
            .unwrap()
            .unwrap()
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
                ["LOGIN", "EXAMINE", "UID", "UID", "UID", "UID", "LOGOUT"]
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
                    .filter(|command| command.contains("BODY.PEEK"))
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
