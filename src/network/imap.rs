use std::{
    fmt, io,
    pin::Pin,
    sync::{Arc, OnceLock},
    task::{Context, Poll},
    time::Duration,
};

use async_imap::{
    Client,
    error::Error as ImapError,
    imap_proto::types::{Response, Status},
};
use rustls::{ClientConfig, Error as RustlsError, crypto, pki_types::ServerName};
use rustls_platform_verifier::BuilderVerifierExt;
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::TcpStream,
    time::{Instant, timeout_at},
};
use tokio_rustls::TlsConnector;

use crate::credentials::Secret;

const MAX_HOST_BYTES: usize = 253;
const MAX_LOGIN_BYTES: usize = 320;
const MAX_SERVER_BYTES: usize = 256 * 1024;
const MAX_CLIENT_BYTES: usize = 64 * 1024;
const DIAGNOSTIC_TIMEOUT: Duration = Duration::from_secs(30);
const LOGOUT_TIMEOUT: Duration = Duration::from_secs(1);

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
    let server_name = ServerName::try_from(request.host.to_string()).map_err(|_| {
        failure(
            ImapDiagnosticStage::Tls,
            ImapDiagnosticFailureKind::Protocol,
        )
    })?;

    let tcp = match timeout_at(
        deadline,
        TcpStream::connect((request.host.as_ref(), request.port)),
    )
    .await
    {
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
    let stream = BoundedIo::new(tls, limits.max_server_bytes, limits.max_client_bytes);
    let mut client = Client::new(stream);

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

    let password = std::str::from_utf8(request.secret.expose()).map_err(|_| {
        failure(
            ImapDiagnosticStage::Authenticate,
            ImapDiagnosticFailureKind::Authentication,
        )
    })?;
    let (mut session, login_capabilities) = match timeout_at(
        deadline,
        client.login_with_capabilities(request.login.as_ref(), password),
    )
    .await
    {
        Ok(Ok(session)) => session,
        Ok(Err((error, _client))) => {
            return Err(failure(
                ImapDiagnosticStage::Authenticate,
                imap_failure_kind(&error, ImapDiagnosticStage::Authenticate),
            ));
        }
        Err(_) => return Err(timeout_failure(ImapDiagnosticStage::Authenticate)),
    };

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
    if !capabilities.has_str("IMAP4rev1") && !capabilities.has_str("IMAP4rev2") {
        return Err(failure(
            ImapDiagnosticStage::Capability,
            ImapDiagnosticFailureKind::Protocol,
        ));
    }

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
            io::ErrorKind::InvalidData => ImapDiagnosticFailureKind::Protocol,
            io::ErrorKind::PermissionDenied => ImapDiagnosticFailureKind::Permission,
            io::ErrorKind::TimedOut => ImapDiagnosticFailureKind::Timeout,
            _ => ImapDiagnosticFailureKind::Offline,
        }
    }
}

fn io_failure_kind(error: &io::Error) -> ImapDiagnosticFailureKind {
    match error.kind() {
        io::ErrorKind::InvalidData | io::ErrorKind::InvalidInput => {
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
                io::ErrorKind::InvalidData,
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
                io::ErrorKind::InvalidData,
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
