use std::{
    fmt,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    thread,
};

use crossbeam_channel::{Receiver, Sender, TrySendError};
use keyring_core::{CredentialStore, Entry, Error as KeyringError};
use tokio::sync::oneshot;
use zeroize::{Zeroize, Zeroizing};

const SERVICE: &str = "io.github.myouo.nivalis.mail";
const REQUEST_CAPACITY: usize = 8;
const MAX_SECRET_BYTES: usize = 16 * 1024;

type StoreFactory =
    Arc<dyn Fn() -> Result<Arc<CredentialStore>, CredentialFailure> + Send + Sync + 'static>;
type CredentialReply = Result<CredentialOutcome, CredentialFailure>;

#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct CredentialLocator(Box<str>);

impl CredentialLocator {
    pub(crate) fn generate() -> Result<Self, CredentialFailure> {
        const HEX: &[u8; 16] = b"0123456789abcdef";

        let mut entropy = Zeroizing::new([0_u8; 16]);
        getrandom::fill(&mut *entropy)
            .map_err(|_| CredentialFailure::new(CredentialFailureKind::RandomUnavailable))?;
        let mut encoded = String::with_capacity(32);
        for byte in entropy.iter().copied() {
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        Ok(Self(encoded.into_boxed_str()))
    }

    pub(crate) fn parse(value: &str) -> Result<Self, CredentialFailure> {
        if value.len() == 32
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            Ok(Self(value.into()))
        } else {
            Err(CredentialFailure::new(CredentialFailureKind::InvalidInput))
        }
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for CredentialLocator {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CredentialLocator([OPAQUE])")
    }
}

pub(crate) struct Secret(Zeroizing<Vec<u8>>);

impl Secret {
    pub(crate) fn new(bytes: Vec<u8>) -> Result<Self, CredentialFailure> {
        Self::with_failure(bytes, CredentialFailureKind::InvalidInput)
    }

    fn from_store(bytes: Vec<u8>) -> Result<Self, CredentialFailure> {
        Self::with_failure(bytes, CredentialFailureKind::CorruptData)
    }

    fn with_failure(
        bytes: Vec<u8>,
        failure_kind: CredentialFailureKind,
    ) -> Result<Self, CredentialFailure> {
        let bytes = Zeroizing::new(bytes);
        if bytes.is_empty() || bytes.len() > MAX_SECRET_BYTES {
            Err(CredentialFailure::new(failure_kind))
        } else {
            Ok(Self(bytes))
        }
    }

    pub(crate) fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Secret([REDACTED])")
    }
}

pub(crate) enum CredentialOperation {
    Store {
        locator: CredentialLocator,
        secret: Secret,
    },
    Load {
        locator: CredentialLocator,
    },
    Delete {
        locator: CredentialLocator,
    },
}

impl CredentialOperation {
    fn is_read(&self) -> bool {
        matches!(self, Self::Load { .. })
    }
}

impl fmt::Debug for CredentialOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store { locator, .. } => formatter
                .debug_struct("Store")
                .field("locator", locator)
                .field("secret", &"[REDACTED]")
                .finish(),
            Self::Load { locator } => formatter
                .debug_struct("Load")
                .field("locator", locator)
                .finish(),
            Self::Delete { locator } => formatter
                .debug_struct("Delete")
                .field("locator", locator)
                .finish(),
        }
    }
}

pub(crate) enum CredentialOutcome {
    Stored,
    Loaded(Secret),
    Deleted(CredentialDeleteOutcome),
}

impl fmt::Debug for CredentialOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stored => formatter.write_str("Stored"),
            Self::Loaded(_) => formatter.write_str("Loaded([REDACTED])"),
            Self::Deleted(outcome) => formatter.debug_tuple("Deleted").field(outcome).finish(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CredentialDeleteOutcome {
    Deleted,
    AlreadyMissing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CredentialFailureKind {
    Missing,
    LockedOrDenied,
    Unavailable,
    InvalidInput,
    Ambiguous,
    CorruptData,
    Unsupported,
    RandomUnavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CredentialFailure {
    pub(crate) kind: CredentialFailureKind,
}

impl CredentialFailure {
    const fn new(kind: CredentialFailureKind) -> Self {
        Self { kind }
    }
}

impl fmt::Display for CredentialFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self.kind {
            CredentialFailureKind::Missing => "the account credential is missing",
            CredentialFailureKind::LockedOrDenied => {
                "the credential service is locked or denied access"
            }
            CredentialFailureKind::Unavailable => "the credential service is unavailable",
            CredentialFailureKind::InvalidInput => "the credential input is invalid",
            CredentialFailureKind::Ambiguous => "multiple account credentials matched",
            CredentialFailureKind::CorruptData => "the stored credential is malformed",
            CredentialFailureKind::Unsupported => {
                "secure credential storage is unsupported on this platform"
            }
            CredentialFailureKind::RandomUnavailable => {
                "the operating-system random source is unavailable"
            }
        })
    }
}

impl std::error::Error for CredentialFailure {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CredentialSubmitError {
    Busy,
    Closed,
    WorkerUnavailable,
}

pub(crate) struct CredentialSubmitFailure {
    reason: CredentialSubmitError,
    operation: CredentialOperation,
}

impl CredentialSubmitFailure {
    pub(crate) fn reason(&self) -> CredentialSubmitError {
        self.reason
    }

    pub(crate) fn into_parts(self) -> (CredentialSubmitError, CredentialOperation) {
        (self.reason, self.operation)
    }
}

impl fmt::Debug for CredentialSubmitFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CredentialSubmitFailure")
            .field("reason", &self.reason)
            .field("operation", &self.operation)
            .finish()
    }
}

#[derive(Clone)]
pub(crate) struct CredentialClient {
    control: Arc<Control>,
}

impl CredentialClient {
    pub(crate) fn try_submit(
        &self,
        operation: CredentialOperation,
    ) -> Result<CredentialResponse, CredentialSubmitFailure> {
        let mut state = self
            .control
            .state
            .lock()
            .expect("credential actor state mutex poisoned");
        if state.closed {
            return Err(CredentialSubmitFailure {
                reason: CredentialSubmitError::Closed,
                operation,
            });
        }
        if state.outstanding >= REQUEST_CAPACITY {
            return Err(CredentialSubmitFailure {
                reason: CredentialSubmitError::Busy,
                operation,
            });
        }
        if state.worker.is_none() {
            let receiver = state
                .receiver
                .take()
                .expect("unstarted credential actor owns its receiver");
            let factory = self.control.factory.clone();
            let worker = thread::Builder::new()
                .name("nivalis-credential".into())
                .spawn(move || run_worker(receiver, factory));
            match worker {
                Ok(worker) => state.worker = Some(worker),
                Err(_) => {
                    state.closed = true;
                    state.sender.take();
                    return Err(CredentialSubmitFailure {
                        reason: CredentialSubmitError::WorkerUnavailable,
                        operation,
                    });
                }
            }
        }

        let (reply, receiver) = oneshot::channel();
        let request = Request { operation, reply };
        let sender = state
            .sender
            .as_ref()
            .expect("open credential actor owns its sender")
            .clone();
        match sender.try_send(request) {
            Ok(()) => {
                state.outstanding += 1;
                Ok(CredentialResponse {
                    receiver,
                    permit: OutstandingPermit {
                        control: self.control.clone(),
                    },
                })
            }
            Err(TrySendError::Full(request)) => Err(CredentialSubmitFailure {
                reason: CredentialSubmitError::Busy,
                operation: request.operation,
            }),
            Err(TrySendError::Disconnected(request)) => {
                state.closed = true;
                state.sender.take();
                Err(CredentialSubmitFailure {
                    reason: CredentialSubmitError::Closed,
                    operation: request.operation,
                })
            }
        }
    }
}

pub(crate) struct CredentialResponse {
    receiver: oneshot::Receiver<CredentialReply>,
    permit: OutstandingPermit,
}

impl fmt::Debug for CredentialResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CredentialResponse([REDACTED])")
    }
}

impl Future for CredentialResponse {
    type Output = Result<CredentialReply, oneshot::error::RecvError>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.receiver).poll(context)
    }
}

struct OutstandingPermit {
    control: Arc<Control>,
}

impl Drop for OutstandingPermit {
    fn drop(&mut self) {
        self.control.release_outstanding();
    }
}

pub(crate) struct CredentialRuntime {
    control: Arc<Control>,
    closed: bool,
}

impl CredentialRuntime {
    pub(crate) fn shutdown(mut self) -> Result<(), CredentialRuntimeError> {
        let result = self.control.close_and_join();
        self.closed = true;
        result
    }
}

impl Drop for CredentialRuntime {
    fn drop(&mut self) {
        if !self.closed {
            let _ = self.control.close_and_join();
            self.closed = true;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CredentialRuntimeError {
    WorkerPanicked,
}

impl fmt::Display for CredentialRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("credential actor worker panicked")
    }
}

impl std::error::Error for CredentialRuntimeError {}

struct Request {
    operation: CredentialOperation,
    reply: oneshot::Sender<CredentialReply>,
}

struct Control {
    state: Mutex<ControlState>,
    factory: StoreFactory,
}

impl Control {
    fn release_outstanding(&self) {
        let mut state = self
            .state
            .lock()
            .expect("credential actor state mutex poisoned");
        state.outstanding = state
            .outstanding
            .checked_sub(1)
            .expect("credential actor outstanding permit underflow");
    }

    fn close_and_join(&self) -> Result<(), CredentialRuntimeError> {
        let worker = {
            let mut state = self
                .state
                .lock()
                .expect("credential actor state mutex poisoned");
            state.closed = true;
            state.sender.take();
            state.receiver.take();
            state.worker.take()
        };
        if worker.is_some_and(|worker| worker.join().is_err()) {
            Err(CredentialRuntimeError::WorkerPanicked)
        } else {
            Ok(())
        }
    }
}

struct ControlState {
    sender: Option<Sender<Request>>,
    receiver: Option<Receiver<Request>>,
    worker: Option<thread::JoinHandle<()>>,
    outstanding: usize,
    closed: bool,
}

pub(crate) fn spawn() -> (CredentialClient, CredentialRuntime) {
    spawn_with_factory(Arc::new(open_platform_store))
}

#[cfg(test)]
pub(crate) fn spawn_with_test_factory<F>(factory: F) -> (CredentialClient, CredentialRuntime)
where
    F: Fn() -> Result<Arc<CredentialStore>, CredentialFailure> + Send + Sync + 'static,
{
    spawn_with_factory(Arc::new(factory))
}

fn spawn_with_factory(factory: StoreFactory) -> (CredentialClient, CredentialRuntime) {
    let (sender, receiver) = crossbeam_channel::bounded(REQUEST_CAPACITY);
    let control = Arc::new(Control {
        state: Mutex::new(ControlState {
            sender: Some(sender),
            receiver: Some(receiver),
            worker: None,
            outstanding: 0,
            closed: false,
        }),
        factory,
    });
    (
        CredentialClient {
            control: control.clone(),
        },
        CredentialRuntime {
            control,
            closed: false,
        },
    )
}

fn run_worker(receiver: Receiver<Request>, factory: StoreFactory) {
    let mut store = None;
    while let Ok(request) = receiver.recv() {
        if request.operation.is_read() && request.reply.is_closed() {
            continue;
        }
        let result = match store.as_ref() {
            Some(store) => execute(store, request.operation),
            None => match factory() {
                Ok(opened) => {
                    store = Some(opened);
                    execute(
                        store
                            .as_ref()
                            .expect("credential store was installed before execution"),
                        request.operation,
                    )
                }
                Err(failure) => Err(failure),
            },
        };
        if result
            .as_ref()
            .is_err_and(|failure| failure.kind == CredentialFailureKind::Unavailable)
        {
            store = None;
        }
        let _ = request.reply.send(result);
    }
}

fn execute(store: &Arc<CredentialStore>, operation: CredentialOperation) -> CredentialReply {
    match operation {
        CredentialOperation::Store { locator, secret } => {
            entry(store, &locator)?
                .set_secret(secret.expose())
                .map_err(map_keyring_error)?;
            Ok(CredentialOutcome::Stored)
        }
        CredentialOperation::Load { locator } => {
            let bytes = entry(store, &locator)?
                .get_secret()
                .map_err(map_keyring_error)?;
            Secret::from_store(bytes).map(CredentialOutcome::Loaded)
        }
        CredentialOperation::Delete { locator } => {
            match entry(store, &locator)?.delete_credential() {
                Ok(()) => Ok(CredentialOutcome::Deleted(CredentialDeleteOutcome::Deleted)),
                Err(KeyringError::NoEntry) => Ok(CredentialOutcome::Deleted(
                    CredentialDeleteOutcome::AlreadyMissing,
                )),
                Err(error) => Err(map_keyring_error(error)),
            }
        }
    }
}

fn entry(
    store: &Arc<CredentialStore>,
    locator: &CredentialLocator,
) -> Result<Entry, CredentialFailure> {
    store
        .build(SERVICE, locator.as_str(), None)
        .map_err(map_keyring_error)
}

#[cfg(target_os = "linux")]
fn open_platform_store() -> Result<Arc<CredentialStore>, CredentialFailure> {
    let store: Arc<CredentialStore> =
        zbus_secret_service_keyring_store::Store::new().map_err(map_keyring_error)?;
    Ok(store)
}

#[cfg(not(target_os = "linux"))]
fn open_platform_store() -> Result<Arc<CredentialStore>, CredentialFailure> {
    Err(CredentialFailure::new(CredentialFailureKind::Unsupported))
}

fn map_keyring_error(error: KeyringError) -> CredentialFailure {
    let kind = match error {
        KeyringError::NoEntry => CredentialFailureKind::Missing,
        KeyringError::NoStorageAccess(_) => CredentialFailureKind::LockedOrDenied,
        KeyringError::PlatformFailure(_) => CredentialFailureKind::Unavailable,
        KeyringError::Invalid(_, _) | KeyringError::TooLong(_, _) => {
            CredentialFailureKind::InvalidInput
        }
        KeyringError::Ambiguous(_) => CredentialFailureKind::Ambiguous,
        KeyringError::BadEncoding(mut bytes) => {
            bytes.zeroize();
            CredentialFailureKind::CorruptData
        }
        KeyringError::BadDataFormat(mut bytes, _) => {
            bytes.zeroize();
            CredentialFailureKind::CorruptData
        }
        KeyringError::BadStoreFormat(_) => CredentialFailureKind::CorruptData,
        KeyringError::NotSupportedByStore(_) | KeyringError::NoDefaultStore => {
            CredentialFailureKind::Unsupported
        }
        _ => CredentialFailureKind::Unavailable,
    };
    CredentialFailure::new(kind)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashSet,
        sync::atomic::{AtomicUsize, Ordering},
        time::{Duration, Instant},
    };

    use super::*;

    const LOCATOR: &str = "0123456789abcdef0123456789abcdef";

    fn mock_store() -> Arc<CredentialStore> {
        keyring_core::mock::Store::new().expect("create mock credential store")
    }

    fn mock_actor() -> (CredentialClient, CredentialRuntime) {
        let store = mock_store();
        spawn_with_factory(Arc::new(move || Ok(store.clone())))
    }

    fn receive(response: CredentialResponse) -> CredentialReply {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(3), response)
                    .await
                    .unwrap()
                    .unwrap()
            })
    }

    fn locator(value: &str) -> CredentialLocator {
        CredentialLocator::parse(value).unwrap()
    }

    fn store_operation(locator: CredentialLocator, value: &[u8]) -> CredentialOperation {
        CredentialOperation::Store {
            locator,
            secret: Secret::new(value.to_vec()).unwrap(),
        }
    }

    #[test]
    fn locator_generation_and_secret_boundaries_are_exact_and_redacted() {
        let mut locators = HashSet::new();
        for _ in 0..64 {
            let locator = CredentialLocator::generate().unwrap();
            assert_eq!(locator.as_str().len(), 32);
            assert!(
                locator
                    .as_str()
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            );
            locators.insert(locator);
        }
        assert_eq!(locators.len(), 64);
        assert!(CredentialLocator::parse("ABCDEF").is_err());
        assert!(Secret::new(Vec::new()).is_err());
        assert!(Secret::new(vec![0; MAX_SECRET_BYTES + 1]).is_err());

        let operation = store_operation(locator(LOCATOR), b"do-not-log-this");
        let debug = format!("{operation:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("do-not-log-this"));
        assert!(!debug.contains(LOCATOR));
    }

    #[test]
    fn actor_is_lazy_and_store_load_delete_is_bounded_and_idempotent() {
        let opens = Arc::new(AtomicUsize::new(0));
        let store = mock_store();
        let opens_for_factory = opens.clone();
        let (client, runtime) = spawn_with_factory(Arc::new(move || {
            opens_for_factory.fetch_add(1, Ordering::SeqCst);
            Ok(store.clone())
        }));
        assert_eq!(opens.load(Ordering::SeqCst), 0);

        assert!(matches!(
            receive(
                client
                    .try_submit(store_operation(locator(LOCATOR), b"credential"))
                    .unwrap()
            )
            .unwrap(),
            CredentialOutcome::Stored
        ));
        assert_eq!(opens.load(Ordering::SeqCst), 1);
        let loaded = receive(
            client
                .try_submit(CredentialOperation::Load {
                    locator: locator(LOCATOR),
                })
                .unwrap(),
        )
        .unwrap();
        let CredentialOutcome::Loaded(secret) = loaded else {
            panic!("expected loaded secret");
        };
        assert_eq!(secret.expose(), b"credential");
        assert!(matches!(
            receive(
                client
                    .try_submit(CredentialOperation::Delete {
                        locator: locator(LOCATOR),
                    })
                    .unwrap()
            )
            .unwrap(),
            CredentialOutcome::Deleted(CredentialDeleteOutcome::Deleted)
        ));
        assert!(matches!(
            receive(
                client
                    .try_submit(CredentialOperation::Delete {
                        locator: locator(LOCATOR),
                    })
                    .unwrap()
            )
            .unwrap(),
            CredentialOutcome::Deleted(CredentialDeleteOutcome::AlreadyMissing)
        ));
        runtime.shutdown().unwrap();
    }

    #[test]
    fn production_actor_does_not_open_platform_storage_before_first_request() {
        let (_client, runtime) = spawn();
        runtime.shutdown().unwrap();
    }

    #[test]
    fn cancelled_receivers_do_not_cancel_accepted_writes() {
        let (client, runtime) = mock_actor();
        let stored = client
            .try_submit(store_operation(locator(LOCATOR), b"credential"))
            .unwrap();
        drop(stored);
        let loaded = receive(
            client
                .try_submit(CredentialOperation::Load {
                    locator: locator(LOCATOR),
                })
                .unwrap(),
        )
        .unwrap();
        assert!(matches!(loaded, CredentialOutcome::Loaded(_)));

        let deleted = client
            .try_submit(CredentialOperation::Delete {
                locator: locator(LOCATOR),
            })
            .unwrap();
        drop(deleted);
        let missing = receive(
            client
                .try_submit(CredentialOperation::Load {
                    locator: locator(LOCATOR),
                })
                .unwrap(),
        )
        .unwrap_err();
        assert_eq!(missing.kind, CredentialFailureKind::Missing);
        runtime.shutdown().unwrap();
    }

    #[test]
    fn shutdown_drains_writes_and_closes_admission() {
        let (client, runtime) = mock_actor();
        let stored = client
            .try_submit(store_operation(locator(LOCATOR), b"credential"))
            .unwrap();

        runtime.shutdown().unwrap();

        assert!(matches!(
            receive(stored).unwrap(),
            CredentialOutcome::Stored
        ));
        let rejected = client
            .try_submit(CredentialOperation::Delete {
                locator: locator(LOCATOR),
            })
            .unwrap_err();
        assert_eq!(rejected.reason(), CredentialSubmitError::Closed);
    }

    #[test]
    fn a_failed_request_does_not_stop_later_writes() {
        let (client, runtime) = mock_actor();
        let missing = client
            .try_submit(CredentialOperation::Load {
                locator: locator(LOCATOR),
            })
            .unwrap();
        let stored = client
            .try_submit(store_operation(locator(LOCATOR), b"credential"))
            .unwrap();

        assert_eq!(
            receive(missing).unwrap_err().kind,
            CredentialFailureKind::Missing
        );
        assert!(matches!(
            receive(stored).unwrap(),
            CredentialOutcome::Stored
        ));
        runtime.shutdown().unwrap();
    }

    #[test]
    fn global_capacity_returns_the_exact_redacted_operation() {
        let (started_tx, started_rx) = crossbeam_channel::bounded(1);
        let (release_tx, release_rx) = crossbeam_channel::bounded(1);
        let store = mock_store();
        let (client, runtime) = spawn_with_factory(Arc::new(move || {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            Ok(store.clone())
        }));
        let first = client
            .try_submit(CredentialOperation::Load {
                locator: locator(LOCATOR),
            })
            .unwrap();
        started_rx.recv_timeout(Duration::from_secs(3)).unwrap();

        let mut queued = Vec::with_capacity(REQUEST_CAPACITY - 1);
        for value in 0..REQUEST_CAPACITY - 1 {
            let locator = locator(&format!("{value:032x}"));
            queued.push(
                client
                    .try_submit(store_operation(locator, b"queued-secret"))
                    .unwrap(),
            );
        }
        let overflow = client
            .try_submit(store_operation(
                locator("ffffffffffffffffffffffffffffffff"),
                b"retry-secret",
            ))
            .unwrap_err();
        let (reason, operation) = overflow.into_parts();
        assert_eq!(reason, CredentialSubmitError::Busy);
        let CredentialOperation::Store { secret, .. } = operation else {
            panic!("expected exact store operation");
        };
        assert_eq!(secret.expose(), b"retry-secret");

        release_tx.send(()).unwrap();
        assert_eq!(
            receive(first).unwrap_err().kind,
            CredentialFailureKind::Missing
        );
        for reply in queued {
            assert!(matches!(receive(reply).unwrap(), CredentialOutcome::Stored));
        }
        runtime.shutdown().unwrap();
    }

    #[test]
    fn worker_panic_closes_response_and_is_reported_by_shutdown() {
        let (client, runtime) =
            spawn_with_factory(Arc::new(|| panic!("controlled credential worker failure")));
        let response = client
            .try_submit(CredentialOperation::Load {
                locator: locator(LOCATOR),
            })
            .unwrap();
        let outer = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(3), response)
                    .await
                    .unwrap()
            });
        assert!(outer.is_err());

        assert_eq!(
            runtime.shutdown().unwrap_err(),
            CredentialRuntimeError::WorkerPanicked
        );

        let rejected = client
            .try_submit(CredentialOperation::Delete {
                locator: locator(LOCATOR),
            })
            .unwrap_err();
        assert_eq!(rejected.reason(), CredentialSubmitError::Closed);
    }

    #[test]
    fn completed_unconsumed_responses_count_toward_the_global_bound() {
        let (client, runtime) = mock_actor();
        assert!(matches!(
            receive(
                client
                    .try_submit(store_operation(locator(LOCATOR), b"credential"))
                    .unwrap()
            )
            .unwrap(),
            CredentialOutcome::Stored
        ));

        let mut responses = Vec::with_capacity(REQUEST_CAPACITY);
        for _ in 0..REQUEST_CAPACITY {
            responses.push(
                client
                    .try_submit(CredentialOperation::Load {
                        locator: locator(LOCATOR),
                    })
                    .unwrap(),
            );
        }

        let mut completed = Vec::with_capacity(REQUEST_CAPACITY);
        for mut response in responses {
            let deadline = Instant::now() + Duration::from_secs(3);
            let reply = loop {
                match response.receiver.try_recv() {
                    Ok(reply) => break reply,
                    Err(oneshot::error::TryRecvError::Empty) if Instant::now() < deadline => {
                        thread::yield_now();
                    }
                    Err(error) => panic!("credential response did not complete: {error}"),
                }
            };
            assert!(matches!(
                reply,
                Ok(CredentialOutcome::Loaded(ref secret)) if secret.expose() == b"credential"
            ));
            completed.push((response, reply));
        }

        let overflow = client
            .try_submit(CredentialOperation::Delete {
                locator: locator("ffffffffffffffffffffffffffffffff"),
            })
            .unwrap_err();
        let (reason, operation) = overflow.into_parts();
        assert_eq!(reason, CredentialSubmitError::Busy);
        assert!(matches!(operation, CredentialOperation::Delete { .. }));

        drop(completed.pop());
        let admitted = client
            .try_submit(CredentialOperation::Load {
                locator: locator(LOCATOR),
            })
            .unwrap();
        drop(admitted);
        drop(completed);
        runtime.shutdown().unwrap();
    }

    #[test]
    fn raw_keyring_errors_collapse_to_fixed_non_secret_kinds() {
        assert_eq!(
            map_keyring_error(KeyringError::NoEntry).kind,
            CredentialFailureKind::Missing
        );
        assert_eq!(
            map_keyring_error(KeyringError::BadEncoding(b"secret bytes".to_vec())).kind,
            CredentialFailureKind::CorruptData
        );
        assert_eq!(
            map_keyring_error(KeyringError::Invalid("field".into(), "reason".into())).kind,
            CredentialFailureKind::InvalidInput
        );
    }
}
