use std::{
    any::Any,
    fmt, fs,
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
    task::{Context, Poll},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crossbeam_channel::{Receiver, Sender, TrySendError, bounded, select_biased};
use rusqlite::{Connection, InterruptHandle, OpenFlags, limits::Limit};
use tokio::sync::mpsc;

use super::{
    domain::{
        DbFailure, DbReply, Generation, MessageId, MessageMutation, PageSpec, RequestId, Tagged,
    },
    migrations::migrate,
    mutation::mutate_message,
    query::{open_message, query_mailbox},
};

const REQUEST_CAPACITY: usize = 16;
const REPLY_CAPACITY: usize = 8;
const SQLITE_CACHE_KIB: i64 = 1024;
const SQLITE_MAX_VALUE_BYTES: i32 = 2 * 1024 * 1024;

enum Request {
    QueryMailbox {
        request_id: RequestId,
        generation: Generation,
        spec: PageSpec,
    },
    OpenMessage {
        request_id: RequestId,
        generation: Generation,
        id: MessageId,
    },
    Mutate {
        request_id: RequestId,
        generation: Generation,
        mutation: MessageMutation,
    },
    #[cfg(test)]
    RunLongQuery { started: Sender<()> },
}

#[derive(Clone)]
pub(crate) struct DatabaseClient {
    requests: Sender<Request>,
    admission: Arc<Mutex<bool>>,
    interrupt: Arc<InterruptHandle>,
    write_gate: Arc<Mutex<()>>,
}

impl DatabaseClient {
    pub(crate) fn try_query_mailbox(
        &self,
        request_id: RequestId,
        generation: Generation,
        spec: PageSpec,
    ) -> Result<(), SubmitError> {
        self.try_submit(Request::QueryMailbox {
            request_id,
            generation,
            spec,
        })
    }

    pub(crate) fn try_open_message(
        &self,
        request_id: RequestId,
        generation: Generation,
        id: MessageId,
    ) -> Result<(), SubmitError> {
        self.try_submit(Request::OpenMessage {
            request_id,
            generation,
            id,
        })
    }

    pub(crate) fn try_mutate(
        &self,
        request_id: RequestId,
        generation: Generation,
        mutation: MessageMutation,
    ) -> Result<(), SubmitError> {
        self.try_mutate_recover(request_id, generation, mutation)
            .map_err(|(error, _)| error)
    }

    pub(crate) fn try_mutate_recover(
        &self,
        request_id: RequestId,
        generation: Generation,
        mutation: MessageMutation,
    ) -> Result<(), (SubmitError, MessageMutation)> {
        let admission = lock_admission(&self.admission);
        if !*admission {
            return Err((SubmitError::Closed, mutation));
        }
        match self.requests.try_send(Request::Mutate {
            request_id,
            generation,
            mutation,
        }) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(Request::Mutate { mutation, .. })) => {
                Err((SubmitError::Busy, mutation))
            }
            Err(TrySendError::Disconnected(Request::Mutate { mutation, .. })) => {
                Err((SubmitError::Closed, mutation))
            }
            Err(_) => unreachable!("try_mutate_recover only submits mutation requests"),
        }
    }

    pub(crate) fn interrupt_queries(&self) {
        let _write_guard = lock_write_gate(&self.write_gate);
        self.interrupt.interrupt();
    }

    fn try_submit(&self, request: Request) -> Result<(), SubmitError> {
        let admission = lock_admission(&self.admission);
        if !*admission {
            return Err(SubmitError::Closed);
        }
        self.requests.try_send(request).map_err(SubmitError::from)
    }

    #[cfg(test)]
    pub(crate) fn try_run_long_query(&self, started: Sender<()>) -> Result<(), SubmitError> {
        self.try_submit(Request::RunLongQuery { started })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SubmitError {
    Busy,
    Closed,
}

impl From<TrySendError<Request>> for SubmitError {
    fn from(error: TrySendError<Request>) -> Self {
        match error {
            TrySendError::Full(_) => Self::Busy,
            TrySendError::Disconnected(_) => Self::Closed,
        }
    }
}

pub(crate) struct DatabaseReplies {
    replies: mpsc::Receiver<DbReply>,
}

impl DatabaseReplies {
    pub(crate) fn poll_recv(&mut self, context: &mut Context<'_>) -> Poll<Option<DbReply>> {
        self.replies.poll_recv(context)
    }

    pub(crate) async fn recv(&mut self) -> Option<DbReply> {
        self.replies.recv().await
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.replies.is_empty()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DatabaseInfo {
    pub(crate) schema_version: u32,
    pub(crate) page_size: u32,
    pub(crate) cache_kib: u32,
    pub(crate) wal_enabled: bool,
    pub(crate) actor_thread: thread::ThreadId,
}

pub(crate) fn spawn(
    path: PathBuf,
) -> Result<
    (
        DatabaseClient,
        DatabaseReplies,
        DatabaseRuntime,
        DatabaseInfo,
    ),
    StartError,
> {
    spawn_target(Target::File(path), REQUEST_CAPACITY, REPLY_CAPACITY)
}

#[cfg(test)]
pub(super) fn spawn_in_memory() -> Result<
    (
        DatabaseClient,
        DatabaseReplies,
        DatabaseRuntime,
        DatabaseInfo,
    ),
    StartError,
> {
    spawn_target(Target::Memory, REQUEST_CAPACITY, REPLY_CAPACITY)
}

fn spawn_target(
    target: Target,
    request_capacity: usize,
    reply_capacity: usize,
) -> Result<
    (
        DatabaseClient,
        DatabaseReplies,
        DatabaseRuntime,
        DatabaseInfo,
    ),
    StartError,
> {
    let (request_tx, request_rx) = bounded(request_capacity);
    let (reply_tx, reply_rx) = mpsc::channel(reply_capacity);
    let (shutdown_tx, shutdown_rx) = bounded(1);
    let (startup_tx, startup_rx) = bounded(1);
    let admission = Arc::new(Mutex::new(true));
    let actor_admission = admission.clone();
    let write_gate = Arc::new(Mutex::new(()));
    let actor_write_gate = write_gate.clone();
    let worker = thread::Builder::new()
        .name("nivalis-sqlite".into())
        .spawn(move || {
            run_actor(
                target,
                request_rx,
                reply_tx,
                shutdown_rx,
                startup_tx,
                actor_admission,
                actor_write_gate,
            )
        })
        .map_err(StartError::Thread)?;

    let started = match startup_rx.recv() {
        Ok(Ok(started)) => started,
        Ok(Err(failure)) => {
            let _ = worker.join();
            return Err(StartError::Initialize(failure));
        }
        Err(_) => return Err(startup_failure(worker)),
    };

    Ok((
        DatabaseClient {
            requests: request_tx,
            admission: admission.clone(),
            interrupt: started.interrupt.clone(),
            write_gate: write_gate.clone(),
        },
        DatabaseReplies { replies: reply_rx },
        DatabaseRuntime {
            shutdown: Some(shutdown_tx),
            admission,
            interrupt: Some(started.interrupt),
            write_gate,
            worker: Some(worker),
        },
        started.info,
    ))
}

enum Target {
    File(PathBuf),
    Memory,
}

struct Started {
    info: DatabaseInfo,
    interrupt: Arc<InterruptHandle>,
}

fn run_actor(
    target: Target,
    requests: Receiver<Request>,
    replies: mpsc::Sender<DbReply>,
    shutdown: Receiver<()>,
    startup: Sender<Result<Started, DbFailure>>,
    admission: Arc<Mutex<bool>>,
    write_gate: Arc<Mutex<()>>,
) -> Result<(), DbFailure> {
    let mut connection = match open_connection(target).and_then(|mut connection| {
        configure(&mut connection)?;
        Ok(connection)
    }) {
        Ok(connection) => connection,
        Err(failure) => {
            let _ = startup.send(Err(failure.clone()));
            return Err(failure);
        }
    };

    let started = Started {
        info: database_info(&connection)?,
        interrupt: Arc::new(connection.get_interrupt_handle()),
    };
    if startup.send(Ok(started)).is_err() {
        return Ok(());
    }

    loop {
        let request = select_biased! {
            recv(shutdown) -> _ => {
                close_admission(&admission);
                return drain_accepted_mutations(
                    &mut connection,
                    &requests,
                    &write_gate,
                    None,
                );
            },
            recv(requests) -> request => match request {
                Ok(request) => request,
                Err(_) => return Ok(()),
            },
        };
        let reply = match request {
            Request::QueryMailbox {
                request_id,
                generation,
                spec,
            } => DbReply::Mailbox(Tagged {
                request_id,
                generation,
                result: query_mailbox(&connection, &spec),
            }),
            Request::OpenMessage {
                request_id,
                generation,
                id,
            } => DbReply::Message(Tagged {
                request_id,
                generation,
                result: open_message(&connection, id),
            }),
            Request::Mutate {
                request_id,
                generation,
                mutation,
            } => DbReply::Mutation(Tagged {
                request_id,
                generation,
                result: execute_mutation(&mut connection, mutation, &write_gate),
            }),
            #[cfg(test)]
            Request::RunLongQuery { started } => {
                let _ = started.send(());
                let _ = connection.query_row(
                    "WITH RECURSIVE counter(value) AS (
                         VALUES(0)
                         UNION ALL
                         SELECT value + 1 FROM counter WHERE value < 1000000000
                     )
                     SELECT sum(value) FROM counter",
                    [],
                    |row| row.get::<_, i64>(0),
                );
                continue;
            }
        };

        if let Err(undelivered) = send_reply(&replies, &shutdown, reply) {
            close_admission(&admission);
            return drain_accepted_mutations(
                &mut connection,
                &requests,
                &write_gate,
                mutation_failure(*undelivered),
            );
        }
    }
}

fn execute_mutation(
    connection: &mut Connection,
    mutation: MessageMutation,
    write_gate: &Mutex<()>,
) -> Result<super::domain::MutationOutcome, DbFailure> {
    let _write_guard = lock_write_gate(write_gate);
    mutate_message(connection, mutation, current_time_ms()?)
}

fn drain_accepted_mutations(
    connection: &mut Connection,
    requests: &Receiver<Request>,
    write_gate: &Mutex<()>,
    initial_failure: Option<DbFailure>,
) -> Result<(), DbFailure> {
    let mut first_failure = initial_failure;
    while let Ok(request) = requests.try_recv() {
        if let Request::Mutate { mutation, .. } = request {
            let result = execute_mutation(connection, mutation, write_gate);
            if first_failure.is_none() {
                first_failure = result.err();
            }
        }
    }
    first_failure.map_or(Ok(()), Err)
}

fn mutation_failure(reply: DbReply) -> Option<DbFailure> {
    match reply {
        DbReply::Mutation(Tagged {
            result: Err(failure),
            ..
        }) => Some(failure),
        _ => None,
    }
}

fn lock_admission(admission: &Mutex<bool>) -> MutexGuard<'_, bool> {
    admission
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

fn close_admission(admission: &Mutex<bool>) {
    *lock_admission(admission) = false;
}

fn lock_write_gate(write_gate: &Mutex<()>) -> MutexGuard<'_, ()> {
    write_gate
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

fn current_time_ms() -> Result<i64, DbFailure> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| DbFailure::resource_limit(error.to_string()))?;
    i64::try_from(elapsed.as_millis())
        .map_err(|_| DbFailure::resource_limit("system time exceeds millisecond range"))
}

fn send_reply(
    replies: &mpsc::Sender<DbReply>,
    shutdown: &Receiver<()>,
    mut reply: DbReply,
) -> Result<(), Box<DbReply>> {
    loop {
        match replies.try_send(reply) {
            Ok(()) => return Ok(()),
            Err(mpsc::error::TrySendError::Closed(pending)) => return Err(Box::new(pending)),
            Err(mpsc::error::TrySendError::Full(pending)) => reply = pending,
        }

        match shutdown.recv_timeout(Duration::from_millis(1)) {
            Ok(()) | Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                return Err(Box::new(reply));
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
        }
    }
}

fn open_connection(target: Target) -> Result<Connection, DbFailure> {
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_CREATE
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    match target {
        Target::File(path) => {
            if let Some(parent) = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
            {
                let parent_existed = parent.exists();
                fs::create_dir_all(parent).map_err(DbFailure::database)?;
                if !parent_existed {
                    secure_directory(parent)?;
                }
            }
            let connection =
                Connection::open_with_flags(&path, flags).map_err(DbFailure::database)?;
            secure_database_file(&path)?;
            Ok(connection)
        }
        Target::Memory => Connection::open_in_memory_with_flags(flags).map_err(DbFailure::database),
    }
}

fn configure(connection: &mut Connection) -> Result<(), DbFailure> {
    connection
        .busy_timeout(Duration::from_secs(5))
        .map_err(DbFailure::database)?;
    connection
        .set_limit(Limit::SQLITE_LIMIT_LENGTH, SQLITE_MAX_VALUE_BYTES)
        .map_err(DbFailure::database)?;
    connection
        .set_limit(Limit::SQLITE_LIMIT_SQL_LENGTH, 1024 * 1024)
        .map_err(DbFailure::database)?;
    connection
        .set_limit(Limit::SQLITE_LIMIT_COLUMN, 128)
        .map_err(DbFailure::database)?;
    connection
        .set_limit(Limit::SQLITE_LIMIT_VARIABLE_NUMBER, 128)
        .map_err(DbFailure::database)?;
    connection
        .set_limit(Limit::SQLITE_LIMIT_LIKE_PATTERN_LENGTH, 512)
        .map_err(DbFailure::database)?;
    connection
        .set_limit(Limit::SQLITE_LIMIT_ATTACHED, 0)
        .map_err(DbFailure::database)?;
    connection
        .set_limit(Limit::SQLITE_LIMIT_WORKER_THREADS, 0)
        .map_err(DbFailure::database)?;
    connection
        .pragma_update(None, "journal_mode", "WAL")
        .map_err(DbFailure::database)?;
    connection
        .pragma_update(None, "synchronous", "FULL")
        .map_err(DbFailure::database)?;
    connection
        .pragma_update(None, "cache_size", -SQLITE_CACHE_KIB)
        .map_err(DbFailure::database)?;
    connection
        .pragma_update(None, "mmap_size", 0_i64)
        .map_err(DbFailure::database)?;
    connection
        .pragma_update(None, "temp_store", "FILE")
        .map_err(DbFailure::database)?;
    connection
        .pragma_update(None, "wal_autocheckpoint", 256_i64)
        .map_err(DbFailure::database)?;
    connection
        .pragma_update(None, "journal_size_limit", 1024_i64 * 1024)
        .map_err(DbFailure::database)?;
    migrate(connection).map_err(DbFailure::migration)
}

fn database_info(connection: &Connection) -> Result<DatabaseInfo, DbFailure> {
    let schema_version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(DbFailure::database)?;
    let page_size: i64 = connection
        .pragma_query_value(None, "page_size", |row| row.get(0))
        .map_err(DbFailure::database)?;
    let cache_size: i64 = connection
        .pragma_query_value(None, "cache_size", |row| row.get(0))
        .map_err(DbFailure::database)?;
    let journal_mode: String = connection
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .map_err(DbFailure::database)?;
    Ok(DatabaseInfo {
        schema_version: u32::try_from(schema_version)
            .map_err(|_| DbFailure::resource_limit("invalid SQLite schema version"))?,
        page_size: u32::try_from(page_size)
            .map_err(|_| DbFailure::resource_limit("invalid SQLite page size"))?,
        cache_kib: u32::try_from(cache_size.unsigned_abs())
            .map_err(|_| DbFailure::resource_limit("invalid SQLite cache size"))?,
        wal_enabled: journal_mode.eq_ignore_ascii_case("wal"),
        actor_thread: thread::current().id(),
    })
}

#[cfg(unix)]
fn secure_directory(path: &std::path::Path) -> Result<(), DbFailure> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(DbFailure::database)
}

#[cfg(not(unix))]
fn secure_directory(_path: &std::path::Path) -> Result<(), DbFailure> {
    Ok(())
}

#[cfg(unix)]
fn secure_database_file(path: &std::path::Path) -> Result<(), DbFailure> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(DbFailure::database)
}

#[cfg(not(unix))]
fn secure_database_file(_path: &std::path::Path) -> Result<(), DbFailure> {
    Ok(())
}

pub(crate) struct DatabaseRuntime {
    shutdown: Option<Sender<()>>,
    admission: Arc<Mutex<bool>>,
    interrupt: Option<Arc<InterruptHandle>>,
    write_gate: Arc<Mutex<()>>,
    worker: Option<thread::JoinHandle<Result<(), DbFailure>>>,
}

impl DatabaseRuntime {
    pub(crate) fn shutdown(mut self) -> Result<(), ShutdownError> {
        self.stop_and_join()
    }

    fn stop_and_join(&mut self) -> Result<(), ShutdownError> {
        close_admission(&self.admission);
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.try_send(());
        }
        let interrupt = self.interrupt.take();
        while self
            .worker
            .as_ref()
            .is_some_and(|worker| !worker.is_finished())
        {
            if let Some(interrupt) = &interrupt {
                let _write_guard = lock_write_gate(&self.write_gate);
                interrupt.interrupt();
            }
            thread::sleep(Duration::from_millis(1));
        }
        let Some(worker) = self.worker.take() else {
            return Ok(());
        };
        worker
            .join()
            .map_err(|panic| ShutdownError::ThreadPanicked(panic_message(panic)))?
            .map_err(ShutdownError::Worker)
    }
}

impl Drop for DatabaseRuntime {
    fn drop(&mut self) {
        let _ = self.stop_and_join();
    }
}

#[derive(Debug)]
pub(crate) enum StartError {
    Thread(std::io::Error),
    Initialize(DbFailure),
    StartupClosed,
    ThreadPanicked(Arc<str>),
}

impl fmt::Display for StartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Thread(error) => write!(formatter, "could not start SQLite actor: {error}"),
            Self::Initialize(error) => write!(formatter, "could not initialize SQLite: {error}"),
            Self::StartupClosed => formatter.write_str("SQLite actor stopped during startup"),
            Self::ThreadPanicked(message) => {
                write!(formatter, "SQLite actor panicked during startup: {message}")
            }
        }
    }
}

impl std::error::Error for StartError {}

#[derive(Debug)]
pub(crate) enum ShutdownError {
    Worker(DbFailure),
    ThreadPanicked(Arc<str>),
}

impl fmt::Display for ShutdownError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Worker(error) => write!(formatter, "SQLite actor stopped with an error: {error}"),
            Self::ThreadPanicked(message) => write!(formatter, "SQLite actor panicked: {message}"),
        }
    }
}

impl std::error::Error for ShutdownError {}

fn startup_failure(worker: thread::JoinHandle<Result<(), DbFailure>>) -> StartError {
    match worker.join() {
        Ok(Err(failure)) => StartError::Initialize(failure),
        Ok(Ok(())) => StartError::StartupClosed,
        Err(panic) => StartError::ThreadPanicked(panic_message(panic)),
    }
}

fn panic_message(panic: Box<dyn Any + Send>) -> Arc<str> {
    if let Some(message) = panic.downcast_ref::<&str>() {
        Arc::from(*message)
    } else if let Some(message) = panic.downcast_ref::<String>() {
        Arc::from(message.as_str())
    } else {
        Arc::from("unknown panic payload")
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::atomic::{AtomicU64, Ordering},
        time::Instant,
    };

    use super::*;
    use crate::store::sqlite::{
        domain::{AccountScope, FailureKind, FolderScope, MessageMutation},
        migrations::LATEST_SCHEMA_VERSION,
    };

    fn empty_spec() -> PageSpec {
        PageSpec::new(AccountScope::All, FolderScope::Inbox, None, None, 50).unwrap()
    }

    fn receive_reply(replies: &mut DatabaseReplies) -> DbReply {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(1), replies.recv())
                    .await
                    .unwrap()
                    .unwrap()
            })
    }

    fn temporary_database_path() -> PathBuf {
        static NEXT_PATH: AtomicU64 = AtomicU64::new(1);
        std::env::temp_dir().join(format!(
            "nivalis-mail-{}-{}.db",
            std::process::id(),
            NEXT_PATH.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn remove_database_files(path: &std::path::Path) {
        let _ = fs::remove_file(path);
        let _ = fs::remove_file(format!("{}-wal", path.display()));
        let _ = fs::remove_file(format!("{}-shm", path.display()));
    }

    #[test]
    fn actor_owns_connection_and_returns_bounded_page() {
        let caller_thread = thread::current().id();
        let (client, mut replies, runtime, info) =
            spawn_target(Target::Memory, REQUEST_CAPACITY, REPLY_CAPACITY).unwrap();
        assert_ne!(info.actor_thread, caller_thread);
        assert_eq!(info.schema_version, LATEST_SCHEMA_VERSION);
        assert_eq!(info.cache_kib, SQLITE_CACHE_KIB as u32);

        client
            .try_query_mailbox(RequestId::new(1).unwrap(), Generation::new(3), empty_spec())
            .unwrap();
        let reply = receive_reply(&mut replies);
        let DbReply::Mailbox(reply) = reply else {
            panic!("expected mailbox reply");
        };
        assert_eq!(reply.result.unwrap().rows.len(), 0);
        runtime.shutdown().unwrap();
    }

    #[test]
    fn mutation_round_trip_preserves_identity_and_typed_failure() {
        let (client, mut replies, runtime, _info) =
            spawn_target(Target::Memory, REQUEST_CAPACITY, REPLY_CAPACITY).unwrap();
        let request_id = RequestId::new(9).unwrap();
        let generation = Generation::new(4);

        client
            .try_mutate(
                request_id,
                generation,
                MessageMutation::set_unread(MessageId::new(1).unwrap(), false),
            )
            .unwrap();

        let DbReply::Mutation(reply) = receive_reply(&mut replies) else {
            panic!("expected mutation reply");
        };
        assert_eq!(reply.request_id, request_id);
        assert_eq!(reply.generation, generation);
        assert_eq!(reply.result.unwrap_err().kind, FailureKind::NotFound);
        runtime.shutdown().unwrap();
    }

    #[test]
    fn full_request_queue_reports_busy_without_blocking() {
        let (sender, _receiver) = bounded(1);
        let connection = Connection::open_in_memory().unwrap();
        let client = DatabaseClient {
            requests: sender,
            admission: Arc::new(Mutex::new(true)),
            interrupt: Arc::new(connection.get_interrupt_handle()),
            write_gate: Arc::new(Mutex::new(())),
        };
        client
            .try_query_mailbox(RequestId::new(1).unwrap(), Generation::new(0), empty_spec())
            .unwrap();

        assert_eq!(
            client.try_query_mailbox(RequestId::new(2).unwrap(), Generation::new(0), empty_spec(),),
            Err(SubmitError::Busy)
        );
    }

    #[test]
    fn shutdown_cancels_reply_backpressure() {
        let (client, _replies, runtime, _info) = spawn_target(Target::Memory, 4, 1).unwrap();
        client
            .try_query_mailbox(RequestId::new(1).unwrap(), Generation::new(0), empty_spec())
            .unwrap();
        client
            .try_query_mailbox(RequestId::new(2).unwrap(), Generation::new(0), empty_spec())
            .unwrap();
        thread::sleep(Duration::from_millis(20));
        let started = Instant::now();

        runtime.shutdown().unwrap();

        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn shutdown_reports_an_undelivered_mutation_failure() {
        let (client, replies, runtime, _info) = spawn_target(Target::Memory, 2, 1).unwrap();
        client
            .try_query_mailbox(RequestId::new(1).unwrap(), Generation::new(0), empty_spec())
            .unwrap();
        while replies.is_empty() {
            thread::yield_now();
        }
        client
            .try_mutate(
                RequestId::new(2).unwrap(),
                Generation::new(0),
                MessageMutation::set_unread(MessageId::new(1).unwrap(), false),
            )
            .unwrap();
        while !client.requests.is_empty() {
            thread::yield_now();
        }
        thread::sleep(Duration::from_millis(10));

        let error = runtime.shutdown().unwrap_err();

        assert!(matches!(error, ShutdownError::Worker(_)));
    }

    #[test]
    fn shutdown_closes_admission_before_draining_live_clients() {
        let (client, _replies, runtime, _info) =
            spawn_target(Target::Memory, 1, REPLY_CAPACITY).unwrap();
        let (started_tx, started_rx) = bounded(1);
        client.try_run_long_query(started_tx).unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let write_guard = lock_write_gate(&client.write_gate);
        let admission = client.admission.clone();
        let shutdown = thread::spawn(move || runtime.shutdown());
        while *lock_admission(&admission) {
            thread::yield_now();
        }

        assert!(!shutdown.is_finished());
        assert_eq!(
            client.try_query_mailbox(RequestId::new(3).unwrap(), Generation::new(0), empty_spec()),
            Err(SubmitError::Closed)
        );

        drop(write_guard);
        shutdown.join().unwrap().unwrap();
    }

    #[test]
    fn closed_reply_stream_closes_admission_before_actor_drain() {
        let (client, replies, runtime, _info) =
            spawn_target(Target::Memory, 2, REPLY_CAPACITY).unwrap();
        let write_guard = lock_write_gate(&client.write_gate);
        drop(replies);
        client
            .try_query_mailbox(RequestId::new(1).unwrap(), Generation::new(0), empty_spec())
            .unwrap();
        client
            .try_mutate(
                RequestId::new(2).unwrap(),
                Generation::new(0),
                MessageMutation::set_unread(MessageId::new(1).unwrap(), false),
            )
            .unwrap();
        let started = Instant::now();
        while *lock_admission(&client.admission) && started.elapsed() < Duration::from_secs(1) {
            thread::yield_now();
        }

        assert!(!*lock_admission(&client.admission));
        assert_eq!(
            client.try_query_mailbox(RequestId::new(3).unwrap(), Generation::new(0), empty_spec()),
            Err(SubmitError::Closed)
        );

        drop(write_guard);
        assert!(matches!(
            runtime.shutdown().unwrap_err(),
            ShutdownError::Worker(_)
        ));
    }

    #[test]
    fn shutdown_interrupts_an_active_sql_query() {
        let (client, _replies, runtime, _info) =
            spawn_target(Target::Memory, 1, REPLY_CAPACITY).unwrap();
        let (started_tx, started_rx) = bounded(1);
        client
            .requests
            .send(Request::RunLongQuery {
                started: started_tx,
            })
            .unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        thread::sleep(Duration::from_millis(20));
        let started = Instant::now();

        runtime.shutdown().unwrap();

        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn shutdown_drains_all_accepted_mutations_after_interrupting_queries() {
        let path = temporary_database_path();
        remove_database_files(&path);
        let mut seed = Connection::open(&path).unwrap();
        configure(&mut seed).unwrap();
        seed.execute(
            "INSERT INTO accounts
             (id, provider, remote_key, name, address, state, accent_rgb)
             VALUES (1, 'imap', 'account', 'Personal', 'user@example.test', 'active', 0)",
            [],
        )
        .unwrap();
        seed.execute(
            "INSERT INTO messages (id, account_id, remote_key, received_at_ms)
             VALUES (1, 1, 'message', 0)",
            [],
        )
        .unwrap();
        super::super::stats::rebuild_account(&seed, 1).unwrap();
        drop(seed);

        let (client, _replies, runtime, _info) =
            spawn_target(Target::File(path.clone()), 2, REPLY_CAPACITY).unwrap();
        let (started_tx, started_rx) = bounded(1);
        client
            .requests
            .send(Request::RunLongQuery {
                started: started_tx,
            })
            .unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        client
            .try_mutate(
                RequestId::new(1).unwrap(),
                Generation::new(0),
                MessageMutation::set_unread(MessageId::new(2).unwrap(), false),
            )
            .unwrap();
        client
            .try_mutate(
                RequestId::new(2).unwrap(),
                Generation::new(0),
                MessageMutation::set_unread(MessageId::new(1).unwrap(), false),
            )
            .unwrap();
        drop(client);

        let error = runtime.shutdown().unwrap_err();
        assert!(matches!(error, ShutdownError::Worker(_)));

        let connection = Connection::open(&path).unwrap();
        let unread: bool = connection
            .query_row("SELECT unread FROM messages WHERE id = 1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(!unread);
        drop(connection);
        remove_database_files(&path);
    }

    #[test]
    fn shutdown_wins_race_with_a_queued_query() {
        let (client, _replies, runtime, _info) =
            spawn_target(Target::Memory, 1, REPLY_CAPACITY).unwrap();
        let (started_tx, _started_rx) = bounded(1);
        client
            .requests
            .send(Request::RunLongQuery {
                started: started_tx,
            })
            .unwrap();
        let started = Instant::now();

        runtime.shutdown().unwrap();

        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn connection_configuration_enforces_memory_and_parallelism_limits() {
        let mut connection = Connection::open_in_memory().unwrap();
        configure(&mut connection).unwrap();

        let foreign_keys: i64 = connection
            .pragma_query_value(None, "foreign_keys", |row| row.get(0))
            .unwrap();
        let recursive_triggers: i64 = connection
            .pragma_query_value(None, "recursive_triggers", |row| row.get(0))
            .unwrap();
        let cache_size: i64 = connection
            .pragma_query_value(None, "cache_size", |row| row.get(0))
            .unwrap();
        let synchronous: i64 = connection
            .pragma_query_value(None, "synchronous", |row| row.get(0))
            .unwrap();
        let temp_store: i64 = connection
            .pragma_query_value(None, "temp_store", |row| row.get(0))
            .unwrap();
        assert_eq!(foreign_keys, 1);
        assert_eq!(recursive_triggers, 1);
        assert_eq!(cache_size, -SQLITE_CACHE_KIB);
        assert_eq!(synchronous, 2);
        assert_eq!(temp_store, 1);
        assert_eq!(
            connection.limit(Limit::SQLITE_LIMIT_LENGTH).unwrap(),
            SQLITE_MAX_VALUE_BYTES
        );
        assert_eq!(connection.limit(Limit::SQLITE_LIMIT_ATTACHED).unwrap(), 0);
        assert_eq!(
            connection
                .limit(Limit::SQLITE_LIMIT_WORKER_THREADS)
                .unwrap(),
            0
        );
    }

    #[test]
    fn file_database_reopens_with_wal_persistence_and_private_permissions() {
        let path = temporary_database_path();
        remove_database_files(&path);
        let (_client, _replies, runtime, info) =
            spawn_target(Target::File(path.clone()), 1, 1).unwrap();
        assert!(info.wal_enabled);
        runtime.shutdown().unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        let connection = Connection::open(&path).unwrap();
        connection
            .execute(
                "INSERT INTO accounts
                 (id, provider, remote_key, name, address, state, accent_rgb)
                 VALUES (1, 'imap', 'account', 'Personal', 'user@example.test', 'active', 0)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO folders (id, account_id, remote_key, name, role)
                 VALUES (1, 1, 'inbox', 'Inbox', 'inbox')",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO messages
                 (id, account_id, remote_key, subject, received_at_ms)
                 VALUES (1, 1, 'message', 'Persisted', 1)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO message_folders (message_id, folder_id, account_id)
                 VALUES (1, 1, 1)",
                [],
            )
            .unwrap();
        super::super::stats::rebuild_account(&connection, 1).unwrap();
        drop(connection);

        let (client, mut replies, runtime, info) =
            spawn_target(Target::File(path.clone()), 1, 1).unwrap();
        assert!(info.wal_enabled);
        client
            .try_query_mailbox(RequestId::new(1).unwrap(), Generation::new(0), empty_spec())
            .unwrap();
        let DbReply::Mailbox(reply) = receive_reply(&mut replies) else {
            panic!("expected mailbox reply");
        };
        let page = reply.result.unwrap();
        assert_eq!(page.rows.len(), 1);
        assert_eq!(page.stats.selected_total, Some(1));
        runtime.shutdown().unwrap();
        remove_database_files(&path);
    }
}
