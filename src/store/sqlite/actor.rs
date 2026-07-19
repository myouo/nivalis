use std::{
    any::Any,
    fmt, fs,
    path::PathBuf,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    task::{Context, Poll},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crossbeam_channel::{Receiver, Sender, TrySendError, bounded, select_biased};
use rusqlite::{Connection, InterruptHandle, OpenFlags, limits::Limit};
use tokio::sync::{mpsc, oneshot};

use crate::content::{ContentStaging, PublishedContent};

use super::{
    content::{
        ContentBatchToken, ContentManifest, ReserveContentRequest,
        finalize_content_with_commit_hook, reserve_content,
    },
    domain::{
        DbFailure, DbReply, Generation, MessageId, MessageMutation, PageSpec, RequestId, Tagged,
    },
    file_gc::{FileGcOutcome, run_file_gc},
    migrations::migrate,
    mutation::mutate_message,
    query::{open_message, query_account_directory, query_mailbox},
    remote::{
        RemoteClaimOutcome, RemoteReportOutcome, RemoteReportSubmission, ReportTransition,
        claim_remote, report_remote,
    },
};

const REQUEST_CAPACITY: usize = 16;
const REPLY_CAPACITY: usize = 8;
const SQLITE_CACHE_KIB: i64 = 1024;
const SQLITE_MAX_VALUE_BYTES: i32 = 2 * 1024 * 1024;
const MAILBOX_PROGRESS_OPS: i32 = 4096;
const CONTENT_IMPORT_TTL_MS: i64 = 60 * 1_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MailboxDbKey {
    request_id: RequestId,
    generation: Generation,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum MailboxQueryState {
    #[default]
    Idle,
    Superseded(MailboxDbKey),
    Active(MailboxDbKey),
    ActiveSuperseded(MailboxDbKey),
}

impl MailboxQueryState {
    fn begin(&mut self, key: MailboxDbKey) -> bool {
        match *self {
            Self::Idle => {
                *self = Self::Active(key);
                true
            }
            Self::Superseded(target) if target == key => {
                *self = Self::Idle;
                false
            }
            Self::Superseded(_) => {
                *self = Self::Active(key);
                true
            }
            Self::Active(_) | Self::ActiveSuperseded(_) => {
                debug_assert!(false, "SQLite actor started overlapping mailbox queries");
                false
            }
        }
    }

    fn supersede(&mut self, key: MailboxDbKey) -> (bool, bool) {
        match *self {
            Self::Idle => {
                *self = Self::Superseded(key);
                (true, false)
            }
            Self::Superseded(target) if target == key => (true, false),
            Self::Active(target) if target == key => {
                *self = Self::ActiveSuperseded(key);
                (true, true)
            }
            Self::ActiveSuperseded(target) if target == key => (true, false),
            Self::Superseded(_) | Self::Active(_) | Self::ActiveSuperseded(_) => (false, false),
        }
    }

    fn finish(&mut self, key: MailboxDbKey) -> bool {
        match *self {
            Self::Active(target) if target == key => {
                *self = Self::Idle;
                false
            }
            Self::ActiveSuperseded(target) if target == key => {
                *self = Self::Idle;
                true
            }
            _ => {
                debug_assert!(false, "SQLite actor finished an inactive mailbox query");
                false
            }
        }
    }
}

#[derive(Default)]
struct MailboxQueryControl {
    state: Mutex<MailboxQueryState>,
    interrupt_requested: AtomicBool,
    #[cfg(test)]
    progress_started: Mutex<Option<Sender<()>>>,
}

impl MailboxQueryControl {
    fn begin(&self, key: MailboxDbKey) -> bool {
        let mut state = lock_mailbox_state(&self.state);
        let execute = state.begin(key);
        self.interrupt_requested.store(false, Ordering::Release);
        execute
    }

    fn finish(&self, key: MailboxDbKey) -> bool {
        let mut state = lock_mailbox_state(&self.state);
        let superseded = state.finish(key);
        self.interrupt_requested.store(false, Ordering::Release);
        superseded
    }

    fn should_interrupt(&self) -> bool {
        #[cfg(test)]
        if let Some(started) = lock_progress_started(&self.progress_started).take() {
            let _ = started.send(());
        }
        self.interrupt_requested.load(Ordering::Acquire)
    }
}

enum Request {
    QueryAccountDirectory {
        request_id: RequestId,
        generation: Generation,
    },
    QueryMailbox {
        request_id: RequestId,
        generation: Generation,
        spec: PageSpec,
        #[cfg(test)]
        gate: Option<MailboxQueryGate>,
        #[cfg(test)]
        long_query: bool,
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
    ClaimRemote {
        account_id: i64,
        reply: oneshot::Sender<Result<RemoteClaimOutcome, DbFailure>>,
    },
    ReportRemote {
        submission: Box<RemoteReportSubmission>,
        reply: oneshot::Sender<RemoteReportReply>,
    },
    ImportContent {
        submission: Box<ContentImportSubmission>,
        reply: oneshot::Sender<ContentImportReply>,
    },
    RunFileGc {
        staging: Arc<ContentStaging>,
        limit: usize,
        reply: oneshot::Sender<Result<FileGcOutcome, DbFailure>>,
    },
    #[cfg(test)]
    RunLongQuery { started: Sender<()> },
}

#[cfg(test)]
struct MailboxQueryGate {
    started: Sender<()>,
    release: Receiver<()>,
}

#[derive(Clone)]
pub(crate) struct DatabaseClient {
    requests: Sender<Request>,
    admission: Arc<Mutex<bool>>,
    interrupt: Arc<InterruptHandle>,
    mailbox_control: Arc<MailboxQueryControl>,
    #[cfg(test)]
    next_mailbox_gate: Arc<Mutex<Option<MailboxQueryGate>>>,
    #[cfg(test)]
    next_mailbox_long: Arc<AtomicBool>,
    write_gate: Arc<Mutex<()>>,
}

impl DatabaseClient {
    pub(crate) fn try_query_account_directory(
        &self,
        request_id: RequestId,
        generation: Generation,
    ) -> Result<(), SubmitError> {
        self.try_submit(Request::QueryAccountDirectory {
            request_id,
            generation,
        })
    }

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
            #[cfg(test)]
            gate: lock_mailbox_gate(&self.next_mailbox_gate).take(),
            #[cfg(test)]
            long_query: self.next_mailbox_long.swap(false, Ordering::AcqRel),
        })
    }

    pub(crate) fn supersede_mailbox_query(
        &self,
        request_id: RequestId,
        generation: Generation,
    ) -> bool {
        let key = MailboxDbKey {
            request_id,
            generation,
        };
        let mut state = lock_mailbox_state(&self.mailbox_control.state);
        let (matched, should_interrupt) = state.supersede(key);
        if should_interrupt {
            self.mailbox_control
                .interrupt_requested
                .store(true, Ordering::Release);
            let _write_guard = lock_write_gate(&self.write_gate);
            self.interrupt.interrupt();
        }
        matched
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

    pub(crate) fn try_claim_remote(
        &self,
        account_id: i64,
    ) -> Result<oneshot::Receiver<Result<RemoteClaimOutcome, DbFailure>>, SubmitError> {
        let (reply, receiver) = oneshot::channel();
        self.try_submit(Request::ClaimRemote { account_id, reply })?;
        Ok(receiver)
    }

    pub(crate) fn try_report_remote(
        &self,
        submission: Box<RemoteReportSubmission>,
    ) -> Result<oneshot::Receiver<RemoteReportReply>, RemoteReportSubmitFailure> {
        let admission = lock_admission(&self.admission);
        if !*admission {
            return Err(RemoteReportSubmitFailure {
                reason: SubmitError::Closed,
                submission,
            });
        }
        let (reply, receiver) = oneshot::channel();
        match self
            .requests
            .try_send(Request::ReportRemote { submission, reply })
        {
            Ok(()) => Ok(receiver),
            Err(TrySendError::Full(Request::ReportRemote { submission, .. })) => {
                Err(RemoteReportSubmitFailure {
                    reason: SubmitError::Busy,
                    submission,
                })
            }
            Err(TrySendError::Disconnected(Request::ReportRemote { submission, .. })) => {
                Err(RemoteReportSubmitFailure {
                    reason: SubmitError::Closed,
                    submission,
                })
            }
            Err(_) => unreachable!("try_report_remote only submits remote reports"),
        }
    }

    pub(crate) fn try_import_content(
        &self,
        submission: Box<ContentImportSubmission>,
    ) -> Result<oneshot::Receiver<ContentImportReply>, ContentImportSubmitFailure> {
        let admission = lock_admission(&self.admission);
        if !*admission {
            return Err(ContentImportSubmitFailure {
                reason: SubmitError::Closed,
                submission,
            });
        }
        let (reply, receiver) = oneshot::channel();
        match self
            .requests
            .try_send(Request::ImportContent { submission, reply })
        {
            Ok(()) => Ok(receiver),
            Err(TrySendError::Full(Request::ImportContent { submission, .. })) => {
                Err(ContentImportSubmitFailure {
                    reason: SubmitError::Busy,
                    submission,
                })
            }
            Err(TrySendError::Disconnected(Request::ImportContent { submission, .. })) => {
                Err(ContentImportSubmitFailure {
                    reason: SubmitError::Closed,
                    submission,
                })
            }
            Err(_) => unreachable!("try_import_content only submits content imports"),
        }
    }

    pub(crate) fn try_run_file_gc(
        &self,
        staging: &Arc<ContentStaging>,
        limit: usize,
    ) -> Result<oneshot::Receiver<Result<FileGcOutcome, DbFailure>>, SubmitError> {
        let (reply, receiver) = oneshot::channel();
        self.try_submit(Request::RunFileGc {
            staging: Arc::clone(staging),
            limit,
            reply,
        })?;
        Ok(receiver)
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

    #[cfg(test)]
    pub(crate) fn gate_next_mailbox_query(&self, started: Sender<()>, release: Receiver<()>) {
        let previous = lock_mailbox_gate(&self.next_mailbox_gate)
            .replace(MailboxQueryGate { started, release });
        assert!(previous.is_none(), "a mailbox query gate is already armed");
    }

    #[cfg(test)]
    pub(crate) fn run_next_mailbox_query_long(&self, progress_started: Sender<()>) {
        let previous =
            lock_progress_started(&self.mailbox_control.progress_started).replace(progress_started);
        assert!(
            previous.is_none(),
            "a mailbox progress probe is already armed"
        );
        assert!(
            !self.next_mailbox_long.swap(true, Ordering::AcqRel),
            "a long mailbox query is already armed"
        );
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SubmitError {
    Busy,
    Closed,
}

pub(crate) type RemoteReportReply = Result<RemoteReportOutcome, RemoteReportExecutionFailure>;

#[derive(Debug)]
pub(crate) struct ContentImportSubmission {
    message_id: MessageId,
    account_id: i64,
    content: PublishedContent,
}

impl ContentImportSubmission {
    pub(crate) fn new(message_id: MessageId, account_id: i64, content: PublishedContent) -> Self {
        Self {
            message_id,
            account_id,
            content,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ContentImportOutcome {
    pub(crate) generation: i64,
}

pub(crate) type ContentImportReply = Result<ContentImportOutcome, ContentImportExecutionFailure>;

#[derive(Debug)]
pub(crate) struct ContentImportSubmitFailure {
    reason: SubmitError,
    submission: Box<ContentImportSubmission>,
}

impl ContentImportSubmitFailure {
    pub(crate) fn reason(&self) -> SubmitError {
        self.reason
    }

    pub(crate) fn submission(&self) -> &ContentImportSubmission {
        &self.submission
    }

    pub(crate) fn into_parts(self) -> (SubmitError, Box<ContentImportSubmission>) {
        (self.reason, self.submission)
    }
}

#[derive(Debug)]
pub(crate) struct ContentImportExecutionFailure {
    failure: DbFailure,
    submission: Box<ContentImportSubmission>,
}

impl ContentImportExecutionFailure {
    pub(crate) fn failure(&self) -> &DbFailure {
        &self.failure
    }

    pub(crate) fn submission(&self) -> &ContentImportSubmission {
        &self.submission
    }

    pub(crate) fn into_parts(self) -> (DbFailure, Box<ContentImportSubmission>) {
        (self.failure, self.submission)
    }
}

#[derive(Debug)]
pub(crate) struct RemoteReportSubmitFailure {
    reason: SubmitError,
    submission: Box<RemoteReportSubmission>,
}

impl RemoteReportSubmitFailure {
    pub(crate) fn reason(&self) -> SubmitError {
        self.reason
    }

    pub(crate) fn submission(&self) -> &RemoteReportSubmission {
        &self.submission
    }

    pub(crate) fn into_parts(self) -> (SubmitError, Box<RemoteReportSubmission>) {
        (self.reason, self.submission)
    }
}

#[derive(Debug)]
pub(crate) struct RemoteReportExecutionFailure {
    failure: DbFailure,
    submission: Box<RemoteReportSubmission>,
}

impl RemoteReportExecutionFailure {
    pub(crate) fn failure(&self) -> &DbFailure {
        &self.failure
    }

    pub(crate) fn submission(&self) -> &RemoteReportSubmission {
        &self.submission
    }

    pub(crate) fn into_parts(self) -> (DbFailure, Box<RemoteReportSubmission>) {
        (self.failure, self.submission)
    }
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
    let mailbox_control = Arc::new(MailboxQueryControl::default());
    let actor_mailbox_control = mailbox_control.clone();
    #[cfg(test)]
    let next_mailbox_gate = Arc::new(Mutex::new(None));
    #[cfg(test)]
    let next_mailbox_long = Arc::new(AtomicBool::new(false));
    let worker = thread::Builder::new()
        .name("nivalis-sqlite".into())
        .spawn(move || {
            run_actor(
                target,
                request_rx,
                reply_tx,
                shutdown_rx,
                startup_tx,
                ActorControl {
                    admission: actor_admission,
                    write_gate: actor_write_gate,
                    mailbox: actor_mailbox_control,
                },
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
            mailbox_control,
            #[cfg(test)]
            next_mailbox_gate,
            #[cfg(test)]
            next_mailbox_long,
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

struct ActorControl {
    admission: Arc<Mutex<bool>>,
    write_gate: Arc<Mutex<()>>,
    mailbox: Arc<MailboxQueryControl>,
}

fn run_actor(
    target: Target,
    requests: Receiver<Request>,
    replies: mpsc::Sender<DbReply>,
    shutdown: Receiver<()>,
    startup: Sender<Result<Started, DbFailure>>,
    control: ActorControl,
) -> Result<(), DbFailure> {
    let mut connection = match open_connection(target).and_then(|mut connection| {
        configure(&mut connection)?;
        let progress_control = control.mailbox.clone();
        connection
            .progress_handler(
                MAILBOX_PROGRESS_OPS,
                Some(move || progress_control.should_interrupt()),
            )
            .map_err(DbFailure::database)?;
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
                close_admission(&control.admission);
                return drain_accepted_writes(
                    &mut connection,
                    &requests,
                    &control.write_gate,
                    None,
                );
            },
            recv(requests) -> request => match request {
                Ok(request) => request,
                Err(_) => return Ok(()),
            },
        };
        let reply = match request {
            Request::QueryAccountDirectory {
                request_id,
                generation,
            } => DbReply::Accounts(Tagged {
                request_id,
                generation,
                result: query_account_directory(&connection),
            }),
            Request::QueryMailbox {
                request_id,
                generation,
                spec,
                #[cfg(test)]
                gate,
                #[cfg(test)]
                long_query,
            } => execute_mailbox_query(
                &connection,
                request_id,
                generation,
                &spec,
                &control.mailbox,
                #[cfg(test)]
                gate,
                #[cfg(test)]
                long_query,
            ),
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
                result: execute_mutation(&mut connection, mutation, &control.write_gate),
            }),
            Request::ClaimRemote { account_id, reply } => {
                if !reply.is_closed() {
                    let result =
                        execute_remote_claim(&mut connection, account_id, &control.write_gate);
                    let _ = reply.send(result);
                }
                continue;
            }
            Request::ReportRemote { submission, reply } => {
                let result =
                    execute_remote_report(&mut connection, submission, &control.write_gate);
                let _ = reply.send(result);
                continue;
            }
            Request::ImportContent { submission, reply } => {
                let result =
                    execute_content_import(&mut connection, submission, &control.write_gate);
                let _ = reply.send(result);
                continue;
            }
            Request::RunFileGc {
                staging,
                limit,
                reply,
            } => {
                if !reply.is_closed() {
                    let result =
                        execute_file_gc(&mut connection, &staging, limit, &control.write_gate);
                    let _ = reply.send(result);
                }
                continue;
            }
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
            close_admission(&control.admission);
            return drain_accepted_writes(
                &mut connection,
                &requests,
                &control.write_gate,
                mutation_failure(*undelivered),
            );
        }
    }
}

fn execute_mailbox_query(
    connection: &Connection,
    request_id: RequestId,
    generation: Generation,
    spec: &PageSpec,
    control: &MailboxQueryControl,
    #[cfg(test)] gate: Option<MailboxQueryGate>,
    #[cfg(test)] long_query: bool,
) -> DbReply {
    let key = MailboxDbKey {
        request_id,
        generation,
    };
    if !control.begin(key) {
        return DbReply::MailboxSuperseded {
            request_id,
            generation,
        };
    }

    #[cfg(test)]
    if let Some(gate) = gate {
        let _ = gate.started.send(());
        let _ = gate.release.recv();
    }

    #[cfg(test)]
    if long_query {
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
    }

    let result = query_mailbox(connection, spec);
    if control.finish(key) {
        DbReply::MailboxSuperseded {
            request_id,
            generation,
        }
    } else {
        DbReply::Mailbox(Tagged {
            request_id,
            generation,
            result,
        })
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

fn execute_remote_claim(
    connection: &mut Connection,
    account_id: i64,
    write_gate: &Mutex<()>,
) -> Result<RemoteClaimOutcome, DbFailure> {
    let _write_guard = lock_write_gate(write_gate);
    claim_remote(connection, account_id, current_time_ms()?)
}

fn execute_remote_report(
    connection: &mut Connection,
    submission: Box<RemoteReportSubmission>,
    write_gate: &Mutex<()>,
) -> RemoteReportReply {
    let _write_guard = lock_write_gate(write_gate);
    let transition = current_time_ms().and_then(|now_ms| {
        report_remote(connection, submission.claim(), submission.report(), now_ms)
    });
    match transition {
        Ok(ReportTransition::Stale) => Ok(RemoteReportOutcome::Stale),
        Ok(ReportTransition::Completed) => Ok(RemoteReportOutcome::Completed),
        Ok(ReportTransition::Pending { state, wake_at_ms }) => {
            Ok(RemoteReportOutcome::Pending { state, wake_at_ms })
        }
        Ok(ReportTransition::Continued(lease)) => Ok(submission.continue_claim(lease)),
        Err(failure) => Err(RemoteReportExecutionFailure {
            failure,
            submission,
        }),
    }
}

fn execute_content_import(
    connection: &mut Connection,
    mut submission: Box<ContentImportSubmission>,
    write_gate: &Mutex<()>,
) -> ContentImportReply {
    let _write_guard = lock_write_gate(write_gate);
    let result = (|| {
        let now_ms = current_time_ms()?;
        let expires_at_ms = now_ms
            .checked_add(CONTENT_IMPORT_TTL_MS)
            .ok_or_else(|| DbFailure::resource_limit("content import lease overflow"))?;
        let record = submission.content.record();
        let request = ReserveContentRequest::new(
            submission.message_id,
            submission.account_id,
            next_content_batch_token(),
            ContentManifest::from_record(&record)?,
            now_ms,
            expires_at_ms,
        )?;
        let reservation = reserve_content(connection, request)?;
        finalize_content_with_commit_hook(connection, &reservation, &record, now_ms, || {
            submission.content.retain_files();
        })
    })();

    match result {
        Ok(outcome) => Ok(ContentImportOutcome {
            generation: outcome.generation,
        }),
        Err(failure) => Err(ContentImportExecutionFailure {
            failure,
            submission,
        }),
    }
}

fn execute_file_gc(
    connection: &mut Connection,
    staging: &ContentStaging,
    limit: usize,
    write_gate: &Mutex<()>,
) -> Result<FileGcOutcome, DbFailure> {
    let _write_guard = lock_write_gate(write_gate);
    run_file_gc(connection, staging, limit)
}

fn drain_accepted_writes(
    connection: &mut Connection,
    requests: &Receiver<Request>,
    write_gate: &Mutex<()>,
    initial_failure: Option<DbFailure>,
) -> Result<(), DbFailure> {
    let mut first_failure = initial_failure;
    while let Ok(request) = requests.try_recv() {
        match request {
            Request::Mutate { mutation, .. } => {
                let result = execute_mutation(connection, mutation, write_gate);
                if first_failure.is_none() {
                    first_failure = result.err();
                }
            }
            Request::ReportRemote { submission, reply } => {
                let result = execute_remote_report(connection, submission, write_gate);
                if first_failure.is_none()
                    && let Err(failure) = &result
                {
                    first_failure = Some(failure.failure.clone());
                }
                let _ = reply.send(result);
            }
            Request::ImportContent { submission, reply } => {
                let result = execute_content_import(connection, submission, write_gate);
                if first_failure.is_none()
                    && let Err(failure) = &result
                {
                    first_failure = Some(failure.failure.clone());
                }
                let _ = reply.send(result);
            }
            _ => {}
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

fn lock_mailbox_state(state: &Mutex<MailboxQueryState>) -> MutexGuard<'_, MailboxQueryState> {
    state.lock().unwrap_or_else(|poison| poison.into_inner())
}

#[cfg(test)]
fn lock_mailbox_gate(
    gate: &Mutex<Option<MailboxQueryGate>>,
) -> MutexGuard<'_, Option<MailboxQueryGate>> {
    gate.lock().unwrap_or_else(|poison| poison.into_inner())
}

#[cfg(test)]
fn lock_progress_started(
    started: &Mutex<Option<Sender<()>>>,
) -> MutexGuard<'_, Option<Sender<()>>> {
    started.lock().unwrap_or_else(|poison| poison.into_inner())
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

fn next_content_batch_token() -> ContentBatchToken {
    static SEQUENCE: AtomicU64 = AtomicU64::new(1);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let process = u128::from(std::process::id());
    ContentBatchToken::new(
        (timestamp ^ process.rotate_left(47) ^ u128::from(sequence)).to_be_bytes(),
    )
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
        io::Read,
        sync::atomic::{AtomicU64, Ordering},
        time::Instant,
    };

    use super::*;
    use crate::{
        content::{ContentLimits, prepare_content},
        store::sqlite::{
            domain::{AccountScope, FailureKind, FolderScope, MessageMutation, PageBoundary},
            migrations::LATEST_SCHEMA_VERSION,
            remote::{RemoteCheckpoint, RemoteImapSource, RemoteReport, RemoteWorkMode},
        },
    };

    fn empty_spec() -> PageSpec {
        PageSpec::new(
            AccountScope::All,
            FolderScope::Inbox,
            None,
            PageBoundary::First,
            50,
        )
        .unwrap()
    }

    fn receive_reply(replies: &mut DatabaseReplies) -> DbReply {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(3), replies.recv())
                    .await
                    .unwrap()
                    .unwrap()
            })
    }

    fn receive_remote_claim(
        receiver: oneshot::Receiver<Result<RemoteClaimOutcome, DbFailure>>,
    ) -> Result<RemoteClaimOutcome, DbFailure> {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(1), receiver)
                    .await
                    .unwrap()
                    .unwrap()
            })
    }

    fn receive_remote_report(receiver: oneshot::Receiver<RemoteReportReply>) -> RemoteReportReply {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(10), receiver)
                    .await
                    .unwrap()
                    .unwrap()
            })
    }

    fn receive_oneshot<T>(receiver: oneshot::Receiver<T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(3), receiver)
                    .await
                    .unwrap()
                    .unwrap()
            })
    }

    fn claimed_remote_intent(client: &DatabaseClient) -> Box<super::super::remote::RemoteClaim> {
        let outcome = receive_remote_claim(client.try_claim_remote(1).unwrap()).unwrap();
        let RemoteClaimOutcome::Claimed(claim) = outcome else {
            panic!("expected a claimed remote intent");
        };
        claim
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

    fn temporary_content_staging(label: &str) -> (PathBuf, Arc<ContentStaging>) {
        static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);
        let root = std::env::temp_dir().join(format!(
            "nivalis-actor-content-{label}-{}-{}",
            std::process::id(),
            NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        let staging = ContentStaging::open(root.clone()).unwrap();
        (root, Arc::new(staging))
    }

    fn publish_test_content(
        staging: &ContentStaging,
        subject: &str,
        body: &str,
    ) -> PublishedContent {
        let raw = format!(
            "From: Ada <ada@example.test>\r\n\
             Subject: {subject}\r\n\
             Date: Thu, 01 Jan 1970 00:00:01 +0000\r\n\
             Content-Type: text/plain; charset=utf-8\r\n\
             \r\n\
             {body}"
        );
        prepare_content(raw.as_bytes(), staging, ContentLimits::default())
            .unwrap()
            .publish()
            .unwrap()
    }

    fn seed_content_message(path: &std::path::Path) {
        remove_database_files(path);
        let mut connection = Connection::open(path).unwrap();
        configure(&mut connection).unwrap();
        connection
            .execute_batch(
                "INSERT INTO accounts
                     (id, provider, remote_key, name, address, state, accent_rgb)
                 VALUES
                     (1, 'imap', 'account', 'Personal',
                      'user@example.test', 'active', 0);
                 INSERT INTO messages
                     (id, account_id, remote_key, received_at_ms)
                 VALUES (1, 1, 'message', 0);",
            )
            .unwrap();
    }

    fn seed_remote_intent(path: &std::path::Path) -> i64 {
        remove_database_files(path);
        let mut connection = Connection::open(path).unwrap();
        configure(&mut connection).unwrap();
        connection
            .execute(
                "INSERT INTO accounts
                     (id, provider, remote_key, name, address, state, accent_rgb)
                 VALUES (1, 'imap', 'account', 'Personal',
                         'user@example.test', 'active', 0)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO remote_change_intents
                     (account_id, target_key, local_revision,
                      unread_base, unread_desired, not_before_ms,
                      created_at_ms, updated_at_ms)
                 VALUES (1, 'message', 1, 1, 0, 0, 0, 0)",
                [],
            )
            .unwrap();
        let intent_id = connection.last_insert_rowid();
        connection
            .execute(
                "INSERT INTO messages
                     (id, account_id, remote_key, received_at_ms, unread, revision)
                 VALUES (1, 1, 'local-message', 0, 1, 0)",
                [],
            )
            .unwrap();
        super::super::stats::rebuild_account(&connection, 1).unwrap();
        connection
            .execute(
                "INSERT INTO remote_change_intent_imap_sources
                     (intent_id, folder_key, uid_validity, uid,
                      remote_seen, remote_flagged)
                 VALUES (?1, 'inbox', 1, 1, 0, 0)",
                [intent_id],
            )
            .unwrap();
        drop(connection);
        intent_id
    }

    fn seed_account_directory(path: &std::path::Path) {
        remove_database_files(path);
        let mut connection = Connection::open(path).unwrap();
        configure(&mut connection).unwrap();
        connection
            .execute_batch(
                "INSERT INTO accounts
                     (id, provider, remote_key, name, address, sort_order, state, accent_rgb)
                 VALUES
                     (2, 'imap', 'two', 'Two', 'two@example.test', 1, 'active', 2),
                     (1, 'jmap', 'one', 'One', 'one@example.test', 0, 'offline', 1);
                 UPDATE account_mailbox_stats
                 SET inbox_total = CASE account_id WHEN 1 THEN 3 ELSE 5 END,
                     inbox_unread = CASE account_id WHEN 1 THEN 3 ELSE 5 END;",
            )
            .unwrap();
    }

    #[test]
    fn content_import_open_replace_and_gc_form_a_bounded_slice() {
        let path = temporary_database_path();
        seed_content_message(&path);
        let (root, staging) = temporary_content_staging("slice");
        let first = publish_test_content(&staging, "First subject", "first body");
        let first_record = first.record();
        let first_key = first_record.body_file_key.clone().unwrap();
        let (client, mut replies, runtime, _info) =
            spawn_target(Target::File(path.clone()), REQUEST_CAPACITY, REPLY_CAPACITY).unwrap();

        let outcome = receive_oneshot(
            client
                .try_import_content(Box::new(ContentImportSubmission::new(
                    MessageId::new(1).unwrap(),
                    1,
                    first,
                )))
                .unwrap(),
        )
        .unwrap();
        assert!(outcome.generation > 0);

        client
            .try_open_message(
                RequestId::new(1).unwrap(),
                Generation::new(0),
                MessageId::new(1).unwrap(),
            )
            .unwrap();
        let DbReply::Message(reply) = receive_reply(&mut replies) else {
            panic!("expected imported message detail");
        };
        let detail = reply.result.unwrap().unwrap();
        assert_eq!(&*detail.subject, "First subject");
        assert_eq!(detail.body_file_key.as_deref(), Some(first_key.as_str()));
        let mut body = String::new();
        staging
            .open_file(&first_key)
            .unwrap()
            .read_to_string(&mut body)
            .unwrap();
        assert!(body.contains("first body"));
        drop(body);

        let second = publish_test_content(&staging, "Second subject", "second body");
        let second_key = second.record().body_file_key.unwrap();
        receive_oneshot(
            client
                .try_import_content(Box::new(ContentImportSubmission::new(
                    MessageId::new(1).unwrap(),
                    1,
                    second,
                )))
                .unwrap(),
        )
        .unwrap();
        let gc = receive_oneshot(client.try_run_file_gc(&staging, 1).unwrap()).unwrap();
        assert_eq!(
            gc,
            FileGcOutcome {
                examined: 1,
                removed: 1,
                ..FileGcOutcome::default()
            }
        );
        assert_eq!(
            staging.open_file(&first_key).unwrap_err().kind,
            std::io::ErrorKind::NotFound
        );
        assert!(staging.open_file(&second_key).is_ok());

        client
            .try_open_message(
                RequestId::new(2).unwrap(),
                Generation::new(0),
                MessageId::new(1).unwrap(),
            )
            .unwrap();
        let DbReply::Message(reply) = receive_reply(&mut replies) else {
            panic!("expected replaced message detail");
        };
        let detail = reply.result.unwrap().unwrap();
        assert_eq!(&*detail.subject, "Second subject");
        assert_eq!(detail.body_file_key.as_deref(), Some(second_key.as_str()));

        runtime.shutdown().unwrap();
        remove_database_files(&path);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn content_import_returns_ownership_when_not_committed() {
        let (root, staging) = temporary_content_staging("recover");
        let (sender, _receiver) = bounded(1);
        let connection = Connection::open_in_memory().unwrap();
        let client = DatabaseClient {
            requests: sender,
            admission: Arc::new(Mutex::new(true)),
            interrupt: Arc::new(connection.get_interrupt_handle()),
            mailbox_control: Arc::new(MailboxQueryControl::default()),
            next_mailbox_gate: Arc::new(Mutex::new(None)),
            next_mailbox_long: Arc::new(AtomicBool::new(false)),
            write_gate: Arc::new(Mutex::new(())),
        };
        client
            .try_query_mailbox(RequestId::new(1).unwrap(), Generation::new(0), empty_spec())
            .unwrap();

        let busy = Box::new(ContentImportSubmission::new(
            MessageId::new(1).unwrap(),
            1,
            publish_test_content(&staging, "Busy", "body"),
        ));
        let busy_pointer: *const ContentImportSubmission = busy.as_ref();
        let failure = client.try_import_content(busy).unwrap_err();
        assert_eq!(failure.reason(), SubmitError::Busy);
        assert!(std::ptr::eq(busy_pointer, failure.submission()));
        drop(failure.into_parts().1);

        close_admission(&client.admission);
        let closed = Box::new(ContentImportSubmission::new(
            MessageId::new(1).unwrap(),
            1,
            publish_test_content(&staging, "Closed", "body"),
        ));
        let closed_pointer: *const ContentImportSubmission = closed.as_ref();
        let failure = client.try_import_content(closed).unwrap_err();
        assert_eq!(failure.reason(), SubmitError::Closed);
        assert!(std::ptr::eq(closed_pointer, failure.submission()));
        drop(failure.into_parts().1);

        let (actor, _replies, runtime, _info) =
            spawn_target(Target::Memory, REQUEST_CAPACITY, REPLY_CAPACITY).unwrap();
        let rejected = Box::new(ContentImportSubmission::new(
            MessageId::new(99).unwrap(),
            1,
            publish_test_content(&staging, "Missing", "body"),
        ));
        let rejected_pointer: *const ContentImportSubmission = rejected.as_ref();
        let failure = receive_oneshot(actor.try_import_content(rejected).unwrap()).unwrap_err();
        assert_eq!(failure.failure().kind, FailureKind::NotFound);
        assert!(std::ptr::eq(rejected_pointer, failure.submission()));
        let (database_failure, recovered) = failure.into_parts();
        assert_eq!(database_failure.kind, FailureKind::NotFound);
        assert!(std::ptr::eq(rejected_pointer, recovered.as_ref()));
        drop(recovered);

        runtime.shutdown().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn shutdown_drains_an_accepted_content_import_after_receiver_cancellation() {
        let path = temporary_database_path();
        seed_content_message(&path);
        let (root, staging) = temporary_content_staging("drain");
        let published = publish_test_content(&staging, "Drained", "durable body");
        let body_key = published.record().body_file_key.unwrap();
        let (client, _replies, runtime, _info) =
            spawn_target(Target::File(path.clone()), 2, REPLY_CAPACITY).unwrap();
        let (started_tx, started_rx) = bounded(1);
        client.try_run_long_query(started_tx).unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let receiver = client
            .try_import_content(Box::new(ContentImportSubmission::new(
                MessageId::new(1).unwrap(),
                1,
                published,
            )))
            .unwrap();
        drop(receiver);

        runtime.shutdown().unwrap();

        let connection = Connection::open(&path).unwrap();
        let stored: (String, Option<String>) = connection
            .query_row(
                "SELECT m.subject, c.body_file_key
                 FROM messages AS m
                 JOIN message_content AS c ON c.message_id = m.id
                 WHERE m.id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(stored, ("Drained".into(), Some(body_key.as_str().into())));
        assert!(staging.open_file(&body_key).is_ok());

        drop(connection);
        remove_database_files(&path);
        fs::remove_dir_all(root).unwrap();
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
    fn mailbox_control_skips_only_the_matching_queued_query() {
        let control = MailboxQueryControl::default();
        let queued = MailboxDbKey {
            request_id: RequestId::new(1).unwrap(),
            generation: Generation::new(1),
        };
        let newer = MailboxDbKey {
            request_id: RequestId::new(2).unwrap(),
            generation: Generation::new(2),
        };

        assert_eq!(
            lock_mailbox_state(&control.state).supersede(queued),
            (true, false)
        );
        assert!(!control.begin(queued));
        assert!(control.begin(newer));
        assert!(!control.finish(newer));
        assert!(!control.should_interrupt());
    }

    #[test]
    fn targeted_mailbox_supersession_is_exact_and_actor_recovers() {
        let (client, mut replies, runtime, _info) =
            spawn_target(Target::Memory, REQUEST_CAPACITY, REPLY_CAPACITY).unwrap();
        let request_id = RequestId::new(11).unwrap();
        let generation = Generation::new(7);
        let (started_tx, started_rx) = bounded(1);
        let (release_tx, release_rx) = bounded(1);
        client.gate_next_mailbox_query(started_tx, release_rx);
        client
            .try_query_mailbox(request_id, generation, empty_spec())
            .unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let mutation_id = RequestId::new(14).unwrap();
        client
            .try_mutate(
                mutation_id,
                Generation::new(10),
                MessageMutation::set_unread(MessageId::new(1).unwrap(), false),
            )
            .unwrap();

        assert!(!client.supersede_mailbox_query(RequestId::new(12).unwrap(), generation));
        assert!(!client.supersede_mailbox_query(request_id, Generation::new(8)));
        assert!(!client.mailbox_control.should_interrupt());
        assert!(client.supersede_mailbox_query(request_id, generation));
        assert!(client.mailbox_control.should_interrupt());
        assert!(client.supersede_mailbox_query(request_id, generation));
        release_tx.send(()).unwrap();

        assert_eq!(
            receive_reply(&mut replies),
            DbReply::MailboxSuperseded {
                request_id,
                generation,
            }
        );
        assert!(!client.mailbox_control.should_interrupt());

        let DbReply::Mutation(mutation) = receive_reply(&mut replies) else {
            panic!("expected the queued mutation after mailbox cancellation");
        };
        assert_eq!(mutation.request_id, mutation_id);
        assert_eq!(mutation.result.unwrap_err().kind, FailureKind::NotFound);

        let retry_id = RequestId::new(13).unwrap();
        client
            .try_query_mailbox(retry_id, Generation::new(9), empty_spec())
            .unwrap();
        let DbReply::Mailbox(retry) = receive_reply(&mut replies) else {
            panic!("expected mailbox actor to accept a query after cancellation");
        };
        assert_eq!(retry.request_id, retry_id);
        retry.result.unwrap();
        runtime.shutdown().unwrap();
    }

    #[test]
    fn targeted_mailbox_supersession_interrupts_a_running_vdbe() {
        let (client, mut replies, runtime, _info) =
            spawn_target(Target::Memory, REQUEST_CAPACITY, REPLY_CAPACITY).unwrap();
        let request_id = RequestId::new(21).unwrap();
        let generation = Generation::new(11);
        let (progress_tx, progress_rx) = bounded(1);
        client.run_next_mailbox_query_long(progress_tx);
        client
            .try_query_mailbox(request_id, generation, empty_spec())
            .unwrap();
        progress_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("long mailbox VDBE did not reach the progress callback");

        let interrupted_at = Instant::now();
        assert!(client.supersede_mailbox_query(request_id, generation));
        assert_eq!(
            receive_reply(&mut replies),
            DbReply::MailboxSuperseded {
                request_id,
                generation,
            }
        );
        assert!(interrupted_at.elapsed() < Duration::from_secs(1));
        assert!(!client.mailbox_control.should_interrupt());

        let retry_id = RequestId::new(22).unwrap();
        client
            .try_query_mailbox(retry_id, Generation::new(12), empty_spec())
            .unwrap();
        let DbReply::Mailbox(retry) = receive_reply(&mut replies) else {
            panic!("expected mailbox actor recovery after VDBE interruption");
        };
        retry.result.unwrap();
        runtime.shutdown().unwrap();
    }

    #[test]
    fn account_directory_round_trip_preserves_identity_and_order() {
        let path = temporary_database_path();
        seed_account_directory(&path);
        let (client, mut replies, runtime, _info) =
            spawn_target(Target::File(path.clone()), REQUEST_CAPACITY, REPLY_CAPACITY).unwrap();
        let request_id = RequestId::new(7).unwrap();
        let generation = Generation::new(4);

        client
            .try_query_account_directory(request_id, generation)
            .unwrap();
        let DbReply::Accounts(reply) = receive_reply(&mut replies) else {
            panic!("expected account directory reply");
        };

        assert_eq!(reply.request_id, request_id);
        assert_eq!(reply.generation, generation);
        let directory = reply.result.unwrap();
        assert_eq!(directory.rows.len(), 2);
        assert_eq!(directory.rows[0].id, 1);
        assert_eq!(directory.rows[0].inbox_unread, 3);
        assert_eq!(directory.rows[1].id, 2);
        assert_eq!(directory.rows[1].inbox_unread, 5);
        runtime.shutdown().unwrap();
        remove_database_files(&path);
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
    fn remote_claim_round_trip_bypasses_the_ui_reply_channel() {
        let path = temporary_database_path();
        let intent_id = seed_remote_intent(&path);
        let (client, replies, runtime, _info) =
            spawn_target(Target::File(path.clone()), REQUEST_CAPACITY, REPLY_CAPACITY).unwrap();

        let receiver = client.try_claim_remote(1).unwrap();
        let claim = receive_remote_claim(receiver).unwrap();

        let RemoteClaimOutcome::Claimed(claim) = claim else {
            panic!("expected a claimed remote intent");
        };
        assert_eq!(claim.lease.intent_id, intent_id);
        assert_eq!(claim.mode, super::super::remote::RemoteWorkMode::Apply);
        assert!(replies.is_empty());
        runtime.shutdown().unwrap();
        remove_database_files(&path);
    }

    #[test]
    fn remote_report_round_trip_bypasses_the_ui_reply_channel() {
        let path = temporary_database_path();
        seed_remote_intent(&path);
        let (client, replies, runtime, _info) =
            spawn_target(Target::File(path.clone()), REQUEST_CAPACITY, REPLY_CAPACITY).unwrap();
        let claim = claimed_remote_intent(&client);
        let submission = RemoteReportSubmission::new(claim, RemoteReport::confirmed(None));

        let receiver = client.try_report_remote(submission).unwrap();
        let outcome = receive_remote_report(receiver).unwrap();

        assert_eq!(outcome, RemoteReportOutcome::Completed);
        assert!(replies.is_empty());
        runtime.shutdown().unwrap();
        let connection = Connection::open(&path).unwrap();
        let intent_count: i64 = connection
            .query_row("SELECT count(*) FROM remote_change_intents", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(intent_count, 0);
        drop(connection);
        remove_database_files(&path);
    }

    #[test]
    fn full_report_queue_returns_the_exact_submission() {
        let path = temporary_database_path();
        seed_remote_intent(&path);
        let (client, mut replies, runtime, _info) =
            spawn_target(Target::File(path.clone()), 1, REPLY_CAPACITY).unwrap();
        let claim = claimed_remote_intent(&client);
        let submission = RemoteReportSubmission::new(claim, RemoteReport::confirmed(None));
        let submission_pointer: *const RemoteReportSubmission = submission.as_ref();
        let (started_tx, started_rx) = bounded(1);
        client.try_run_long_query(started_tx).unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        client
            .try_query_mailbox(RequestId::new(1).unwrap(), Generation::new(0), empty_spec())
            .unwrap();

        let failure = match client.try_report_remote(submission) {
            Ok(_) => panic!("a full request queue accepted a remote report"),
            Err(failure) => failure,
        };

        assert_eq!(failure.reason(), SubmitError::Busy);
        assert!(std::ptr::eq(submission_pointer, failure.submission()));
        client.interrupt_queries();
        let DbReply::Mailbox(reply) = receive_reply(&mut replies) else {
            panic!("expected the queued mailbox reply");
        };
        reply.result.unwrap();
        runtime.shutdown().unwrap();
        remove_database_files(&path);
    }

    #[test]
    fn closed_actor_returns_the_exact_report_submission() {
        let path = temporary_database_path();
        seed_remote_intent(&path);
        let (client, _replies, runtime, _info) =
            spawn_target(Target::File(path.clone()), REQUEST_CAPACITY, REPLY_CAPACITY).unwrap();
        let claim = claimed_remote_intent(&client);
        let submission = RemoteReportSubmission::new(claim, RemoteReport::confirmed(None));
        let submission_pointer: *const RemoteReportSubmission = submission.as_ref();
        runtime.shutdown().unwrap();

        let failure = match client.try_report_remote(submission) {
            Ok(_) => panic!("a closed actor accepted a remote report"),
            Err(failure) => failure,
        };

        assert_eq!(failure.reason(), SubmitError::Closed);
        assert!(std::ptr::eq(submission_pointer, failure.submission()));
        let (reason, recovered) = failure.into_parts();
        assert_eq!(reason, SubmitError::Closed);
        assert!(std::ptr::eq(submission_pointer, recovered.as_ref()));
        remove_database_files(&path);
    }

    #[test]
    fn cancelled_report_receiver_does_not_cancel_the_write() {
        let path = temporary_database_path();
        seed_remote_intent(&path);
        let (client, mut replies, runtime, _info) =
            spawn_target(Target::File(path.clone()), 2, REPLY_CAPACITY).unwrap();
        let claim = claimed_remote_intent(&client);
        let (started_tx, started_rx) = bounded(1);
        client.try_run_long_query(started_tx).unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let receiver = client
            .try_report_remote(RemoteReportSubmission::new(
                claim,
                RemoteReport::confirmed(None),
            ))
            .unwrap();
        drop(receiver);
        client
            .try_query_mailbox(RequestId::new(1).unwrap(), Generation::new(0), empty_spec())
            .unwrap();

        client.interrupt_queries();
        let DbReply::Mailbox(reply) = receive_reply(&mut replies) else {
            panic!("expected the FIFO barrier reply");
        };
        reply.result.unwrap();
        runtime.shutdown().unwrap();

        let connection = Connection::open(&path).unwrap();
        let intent_count: i64 = connection
            .query_row("SELECT count(*) FROM remote_change_intents", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(intent_count, 0);
        drop(connection);
        remove_database_files(&path);
    }

    #[test]
    fn shutdown_drains_an_accepted_remote_report() {
        let path = temporary_database_path();
        seed_remote_intent(&path);
        let (client, _replies, runtime, _info) =
            spawn_target(Target::File(path.clone()), 1, REPLY_CAPACITY).unwrap();
        let claim = claimed_remote_intent(&client);
        let (started_tx, started_rx) = bounded(1);
        client.try_run_long_query(started_tx).unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let receiver = client
            .try_report_remote(RemoteReportSubmission::new(
                claim,
                RemoteReport::confirmed(None),
            ))
            .unwrap();

        runtime.shutdown().unwrap();

        assert_eq!(
            receive_remote_report(receiver).unwrap(),
            RemoteReportOutcome::Completed
        );
        let connection = Connection::open(&path).unwrap();
        let intent_count: i64 = connection
            .query_row("SELECT count(*) FROM remote_change_intents", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(intent_count, 0);
        drop(connection);
        remove_database_files(&path);
    }

    #[test]
    fn shutdown_continues_draining_after_a_report_failure() {
        let path = temporary_database_path();
        seed_remote_intent(&path);
        let (client, _replies, runtime, _info) =
            spawn_target(Target::File(path.clone()), 2, REPLY_CAPACITY).unwrap();
        let mut claim = claimed_remote_intent(&client);
        claim.mode = RemoteWorkMode::Reconcile;
        let (started_tx, started_rx) = bounded(1);
        client.try_run_long_query(started_tx).unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let report_receiver = client
            .try_report_remote(RemoteReportSubmission::new(
                claim,
                RemoteReport::confirmed(None),
            ))
            .unwrap();
        client
            .try_mutate(
                RequestId::new(1).unwrap(),
                Generation::new(0),
                MessageMutation::set_unread(MessageId::new(1).unwrap(), false),
            )
            .unwrap();

        let shutdown_error = runtime.shutdown().unwrap_err();

        assert!(matches!(shutdown_error, ShutdownError::Worker(_)));
        assert_eq!(
            receive_remote_report(report_receiver)
                .unwrap_err()
                .failure()
                .kind,
            FailureKind::Conflict
        );
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
    fn closed_ui_reply_stream_still_drains_an_accepted_report() {
        let path = temporary_database_path();
        seed_remote_intent(&path);
        let (client, replies, runtime, _info) =
            spawn_target(Target::File(path.clone()), 2, REPLY_CAPACITY).unwrap();
        let claim = claimed_remote_intent(&client);
        let (started_tx, started_rx) = bounded(1);
        client.try_run_long_query(started_tx).unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        client
            .try_query_mailbox(RequestId::new(1).unwrap(), Generation::new(0), empty_spec())
            .unwrap();
        let report_receiver = client
            .try_report_remote(RemoteReportSubmission::new(
                claim,
                RemoteReport::confirmed(None),
            ))
            .unwrap();
        drop(replies);

        client.interrupt_queries();

        assert_eq!(
            receive_remote_report(report_receiver).unwrap(),
            RemoteReportOutcome::Completed
        );
        runtime.shutdown().unwrap();
        let connection = Connection::open(&path).unwrap();
        let intent_count: i64 = connection
            .query_row("SELECT count(*) FROM remote_change_intents", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(intent_count, 0);
        drop(connection);
        remove_database_files(&path);
    }

    #[test]
    fn report_execution_failure_returns_the_exact_submission() {
        let path = temporary_database_path();
        seed_remote_intent(&path);
        let (client, _replies, runtime, _info) =
            spawn_target(Target::File(path.clone()), REQUEST_CAPACITY, REPLY_CAPACITY).unwrap();
        let mut claim = claimed_remote_intent(&client);
        claim.mode = RemoteWorkMode::Reconcile;
        let submission = RemoteReportSubmission::new(claim, RemoteReport::confirmed(None));
        let submission_pointer: *const RemoteReportSubmission = submission.as_ref();

        let receiver = client.try_report_remote(submission).unwrap();
        let failure = receive_remote_report(receiver).unwrap_err();

        assert_eq!(failure.failure().kind, FailureKind::Conflict);
        assert!(std::ptr::eq(submission_pointer, failure.submission()));
        let (database_failure, recovered) = failure.into_parts();
        assert_eq!(database_failure.kind, FailureKind::Conflict);
        assert!(std::ptr::eq(submission_pointer, recovered.as_ref()));
        runtime.shutdown().unwrap();
        remove_database_files(&path);
    }

    #[test]
    fn progress_report_returns_the_renewed_claim() {
        let path = temporary_database_path();
        seed_remote_intent(&path);
        let (client, _replies, runtime, _info) =
            spawn_target(Target::File(path.clone()), REQUEST_CAPACITY, REPLY_CAPACITY).unwrap();
        let claim = claimed_remote_intent(&client);
        let source = RemoteImapSource::new(
            "inbox",
            Some("mailbox-1"),
            1,
            1,
            Some(2),
            Some("email-1"),
            false,
            false,
        )
        .unwrap();
        let checkpoint = RemoteCheckpoint::imap_sources(vec![source].into_boxed_slice()).unwrap();
        let receiver = client
            .try_report_remote(RemoteReportSubmission::new(
                claim,
                RemoteReport::progress(checkpoint),
            ))
            .unwrap();

        let RemoteReportOutcome::Continued(claim) = receive_remote_report(receiver).unwrap() else {
            panic!("expected a continued remote claim");
        };
        assert_eq!(claim.lease.claim_epoch, 2);
        assert_eq!(claim.mode, RemoteWorkMode::Apply);
        assert_eq!(claim.imap_sources[0].modseq, Some(2));

        let receiver = client
            .try_report_remote(RemoteReportSubmission::new(
                claim,
                RemoteReport::confirmed(None),
            ))
            .unwrap();
        assert_eq!(
            receive_remote_report(receiver).unwrap(),
            RemoteReportOutcome::Completed
        );
        runtime.shutdown().unwrap();
        remove_database_files(&path);
    }

    #[test]
    fn full_request_queue_reports_busy_without_blocking() {
        let (sender, _receiver) = bounded(1);
        let connection = Connection::open_in_memory().unwrap();
        let client = DatabaseClient {
            requests: sender,
            admission: Arc::new(Mutex::new(true)),
            interrupt: Arc::new(connection.get_interrupt_handle()),
            mailbox_control: Arc::new(MailboxQueryControl::default()),
            next_mailbox_gate: Arc::new(Mutex::new(None)),
            next_mailbox_long: Arc::new(AtomicBool::new(false)),
            write_gate: Arc::new(Mutex::new(())),
        };
        client
            .try_query_mailbox(RequestId::new(1).unwrap(), Generation::new(0), empty_spec())
            .unwrap();

        assert_eq!(
            client.try_query_mailbox(RequestId::new(2).unwrap(), Generation::new(0), empty_spec(),),
            Err(SubmitError::Busy)
        );
        assert_eq!(
            client.try_query_account_directory(RequestId::new(3).unwrap(), Generation::new(0),),
            Err(SubmitError::Busy)
        );
        assert!(matches!(client.try_claim_remote(1), Err(SubmitError::Busy)));
    }

    #[test]
    fn shutdown_drops_a_queued_remote_claim_without_leasing() {
        let path = temporary_database_path();
        let intent_id = seed_remote_intent(&path);
        let (client, _replies, runtime, _info) =
            spawn_target(Target::File(path.clone()), 2, REPLY_CAPACITY).unwrap();
        let (started_tx, started_rx) = bounded(1);
        client.try_run_long_query(started_tx).unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let receiver = client.try_claim_remote(1).unwrap();

        runtime.shutdown().unwrap();

        let receive_result = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(receiver);
        assert!(receive_result.is_err());
        let connection = Connection::open(&path).unwrap();
        let stored: (String, Option<i64>, Option<i64>, i64) = connection
            .query_row(
                "SELECT state, leased_version, lease_expires_at_ms, attempt_count
                 FROM remote_change_intents WHERE id = ?1",
                [intent_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(stored, ("ready".into(), None, None, 0));
        drop(connection);
        remove_database_files(&path);
    }

    #[test]
    fn cancelled_remote_claim_is_skipped_before_leasing() {
        let path = temporary_database_path();
        let intent_id = seed_remote_intent(&path);
        let (client, mut replies, runtime, _info) =
            spawn_target(Target::File(path.clone()), 3, REPLY_CAPACITY).unwrap();
        let (started_tx, started_rx) = bounded(1);
        client.try_run_long_query(started_tx).unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let receiver = client.try_claim_remote(1).unwrap();
        drop(receiver);
        client
            .try_query_mailbox(RequestId::new(1).unwrap(), Generation::new(0), empty_spec())
            .unwrap();

        client.interrupt_queries();
        let DbReply::Mailbox(reply) = receive_reply(&mut replies) else {
            panic!("expected mailbox reply after the cancelled claim");
        };
        reply.result.unwrap();
        runtime.shutdown().unwrap();

        let connection = Connection::open(&path).unwrap();
        let stored: (String, Option<i64>, Option<i64>, i64) = connection
            .query_row(
                "SELECT state, leased_version, lease_expires_at_ms, attempt_count
                 FROM remote_change_intents WHERE id = ?1",
                [intent_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(stored, ("ready".into(), None, None, 0));
        drop(connection);
        remove_database_files(&path);
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
