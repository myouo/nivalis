use std::{
    any::Any,
    fmt, fs,
    path::PathBuf,
    sync::Arc,
    task::{Context, Poll},
    thread,
    time::Duration,
};

use crossbeam_channel::{Receiver, Sender, TrySendError, bounded, select_biased};
use rusqlite::{Connection, InterruptHandle, OpenFlags, limits::Limit};
use tokio::sync::mpsc;

use super::{
    domain::{DbFailure, DbReply, Generation, MessageId, PageSpec, RequestId, Tagged},
    migrations::migrate,
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
    #[cfg(test)]
    RunLongQuery { started: Sender<()> },
}

#[derive(Clone)]
pub(crate) struct DatabaseClient {
    requests: Sender<Request>,
}

impl DatabaseClient {
    pub(crate) fn try_query_mailbox(
        &self,
        request_id: RequestId,
        generation: Generation,
        spec: PageSpec,
    ) -> Result<(), SubmitError> {
        self.requests
            .try_send(Request::QueryMailbox {
                request_id,
                generation,
                spec,
            })
            .map_err(SubmitError::from)
    }

    pub(crate) fn try_open_message(
        &self,
        request_id: RequestId,
        generation: Generation,
        id: MessageId,
    ) -> Result<(), SubmitError> {
        self.requests
            .try_send(Request::OpenMessage {
                request_id,
                generation,
                id,
            })
            .map_err(SubmitError::from)
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
    let worker = thread::Builder::new()
        .name("nivalis-sqlite".into())
        .spawn(move || run_actor(target, request_rx, reply_tx, shutdown_rx, startup_tx))
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
        },
        DatabaseReplies { replies: reply_rx },
        DatabaseRuntime {
            shutdown: Some(shutdown_tx),
            interrupt: Some(started.interrupt),
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
    interrupt: InterruptHandle,
}

fn run_actor(
    target: Target,
    requests: Receiver<Request>,
    replies: mpsc::Sender<DbReply>,
    shutdown: Receiver<()>,
    startup: Sender<Result<Started, DbFailure>>,
) -> Result<(), DbFailure> {
    let connection = match open_connection(target).and_then(|mut connection| {
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
        interrupt: connection.get_interrupt_handle(),
    };
    if startup.send(Ok(started)).is_err() {
        return Ok(());
    }

    loop {
        let request = select_biased! {
            recv(shutdown) -> _ => return Ok(()),
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

        if !send_reply(&replies, &shutdown, reply) {
            return Ok(());
        }
    }
}

fn send_reply(
    replies: &mpsc::Sender<DbReply>,
    shutdown: &Receiver<()>,
    mut reply: DbReply,
) -> bool {
    loop {
        match replies.try_send(reply) {
            Ok(()) => return true,
            Err(mpsc::error::TrySendError::Closed(_)) => return false,
            Err(mpsc::error::TrySendError::Full(pending)) => reply = pending,
        }

        match shutdown.recv_timeout(Duration::from_millis(1)) {
            Ok(()) | Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return false,
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
    interrupt: Option<InterruptHandle>,
    worker: Option<thread::JoinHandle<Result<(), DbFailure>>>,
}

impl DatabaseRuntime {
    pub(crate) fn shutdown(mut self) -> Result<(), ShutdownError> {
        self.stop_and_join()
    }

    fn stop_and_join(&mut self) -> Result<(), ShutdownError> {
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
        domain::{AccountScope, FolderScope},
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
    fn full_request_queue_reports_busy_without_blocking() {
        let (sender, _receiver) = bounded(1);
        let client = DatabaseClient { requests: sender };
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
        assert_eq!(reply.result.unwrap().rows.len(), 1);
        runtime.shutdown().unwrap();
        remove_database_files(&path);
    }
}
