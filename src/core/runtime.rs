use super::message::{
    COMMAND_CAPACITY, Command, CoreHandle, EVENT_CAPACITY, Event, EventReceiver, EventSender,
    MailboxLoadError, MailboxQuery, MailboxRequestKey, MessageLoadError, MessageQuery,
    MessageRequestKey, event_channel,
};
use crate::store::sqlite::{
    self, DatabaseClient, DatabaseInfo, DatabaseReplies, DatabaseRuntime, DatabaseSubmitError,
    DbReply,
};
use std::{
    fmt,
    future::{Future, poll_fn},
    path::PathBuf,
    pin::Pin,
    sync::Arc,
    task::Poll,
    thread,
    time::Duration,
};
use tokio::{runtime::Builder, sync::mpsc, sync::oneshot, time};

const SYNC_DELAY: Duration = Duration::from_millis(900);
const COMMAND_BURST_LIMIT: u8 = 8;

#[cfg(test)]
type SyncStarted = Option<std::sync::mpsc::SyncSender<()>>;
#[cfg(not(test))]
type SyncStarted = ();

type DatabaseParts = (
    DatabaseClient,
    DatabaseReplies,
    DatabaseRuntime,
    DatabaseInfo,
);

pub(crate) fn spawn(
    database_path: PathBuf,
) -> Result<(CoreHandle, EventReceiver, CoreRuntime), StartError> {
    let database = sqlite::spawn(database_path).map_err(StartError::Database)?;
    #[cfg(test)]
    {
        spawn_with_options(SYNC_DELAY, EVENT_CAPACITY, database, None)
    }
    #[cfg(not(test))]
    {
        spawn_with_options(SYNC_DELAY, EVENT_CAPACITY, database, ())
    }
}

#[cfg(test)]
fn spawn_with_delay(
    sync_delay: Duration,
) -> Result<(CoreHandle, EventReceiver, CoreRuntime), StartError> {
    let database = sqlite::spawn_in_memory().map_err(StartError::Database)?;
    spawn_with_options(sync_delay, EVENT_CAPACITY, database, None)
}

fn spawn_with_options(
    sync_delay: Duration,
    event_capacity: usize,
    database: DatabaseParts,
    sync_started: SyncStarted,
) -> Result<(CoreHandle, EventReceiver, CoreRuntime), StartError> {
    let (database, database_replies, database_runtime, _database_info) = database;
    let (command_tx, command_rx) = mpsc::channel(COMMAND_CAPACITY);
    let (event_tx, event_rx) = event_channel(event_capacity);
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let runtime = Builder::new_current_thread()
        .enable_time()
        .max_blocking_threads(2)
        .build()
        .map_err(StartError::Runtime)?;
    let worker = thread::Builder::new()
        .name("nivalis-core".into())
        .spawn(move || {
            let core_result = runtime.block_on(run_core(
                command_rx,
                event_tx,
                shutdown_rx,
                database,
                database_replies,
                sync_delay,
                sync_started,
            ));
            let database_result =
                database_runtime
                    .shutdown()
                    .map_err(|error| RuntimeError::DatabaseShutdown {
                        message: Arc::from(error.to_string()),
                    });
            match core_result {
                Err(error) => Err(error),
                Ok(()) => database_result,
            }
        })
        .map_err(StartError::Runtime)?;

    Ok((
        CoreHandle::new(command_tx),
        event_rx,
        CoreRuntime {
            shutdown: Some(shutdown_tx),
            worker: Some(worker),
        },
    ))
}

async fn run_core(
    mut commands: mpsc::Receiver<Command>,
    events: EventSender,
    shutdown: oneshot::Receiver<()>,
    database: DatabaseClient,
    mut database_replies: DatabaseReplies,
    sync_delay: Duration,
    _sync_started: SyncStarted,
) -> Result<(), RuntimeError> {
    let mut shutdown = Box::pin(shutdown);
    let mut active_sync: Option<(super::OperationId, Pin<Box<time::Sleep>>)> = None;
    let mut pending_sync = None;
    let mut mailbox = MailboxSchedule::default();
    let mut message = MessageSchedule::default();
    let mut command_streak = 0_u8;

    loop {
        let input = next_input(
            &mut shutdown,
            &mut commands,
            &mut database_replies,
            &mut active_sync,
            command_streak < COMMAND_BURST_LIMIT,
        )
        .await;
        if matches!(&input, CoreInput::Command(_)) {
            command_streak = command_streak.saturating_add(1);
        } else {
            command_streak = 0;
        }

        match input {
            CoreInput::Shutdown | CoreInput::CommandsClosed => return Ok(()),
            CoreInput::DatabaseClosed => return Err(RuntimeError::DatabaseClosed),
            CoreInput::Command(Command::SyncNow { operation_id }) => {
                if active_sync.is_some() {
                    pending_sync = Some(operation_id);
                } else {
                    #[cfg(test)]
                    if let Some(sync_started) = &_sync_started {
                        let _ = sync_started.try_send(());
                    }
                    active_sync = Some((operation_id, Box::pin(time::sleep(sync_delay))));
                }
            }
            CoreInput::Command(Command::QueryMailbox(query)) => {
                if let Some(query) = mailbox.enqueue(query)
                    && let Some(event) = submit_mailbox(&database, &mut mailbox, query)
                    && !send_event(&events, &mut shutdown, event).await
                {
                    return Ok(());
                }
            }
            CoreInput::Command(Command::OpenMessage(query)) => {
                if let Some(query) = message.enqueue(query)
                    && let Some(event) = submit_message(&database, &mut message, query)
                    && !send_event(&events, &mut shutdown, event).await
                {
                    return Ok(());
                }
            }
            CoreInput::Database(DbReply::Mailbox(reply)) => {
                let key = MailboxRequestKey {
                    request_id: reply.request_id,
                    generation: reply.generation,
                };
                match mailbox.complete(key) {
                    MailboxCompletion::Ignore => {}
                    MailboxCompletion::Publish => {
                        if !send_event(&events, &mut shutdown, Event::MailboxLoaded(reply)).await {
                            return Ok(());
                        }
                    }
                    MailboxCompletion::Dispatch(query) => {
                        if let Some(event) = submit_mailbox(&database, &mut mailbox, query)
                            && !send_event(&events, &mut shutdown, event).await
                        {
                            return Ok(());
                        }
                    }
                }
            }
            CoreInput::Database(DbReply::Message(reply)) => {
                let key = MessageRequestKey {
                    request_id: reply.request_id,
                    generation: reply.generation,
                };
                match message.complete(key) {
                    MessageCompletion::Ignore => {}
                    MessageCompletion::Publish => {
                        if !send_event(&events, &mut shutdown, Event::MessageLoaded(reply)).await {
                            return Ok(());
                        }
                    }
                    MessageCompletion::Dispatch(query) => {
                        if let Some(event) = submit_message(&database, &mut message, query)
                            && !send_event(&events, &mut shutdown, event).await
                        {
                            return Ok(());
                        }
                    }
                }
            }
            CoreInput::SyncElapsed => {
                let Some((operation_id, _)) = active_sync.take() else {
                    continue;
                };
                if !send_event(&events, &mut shutdown, Event::SyncFinished { operation_id }).await {
                    return Ok(());
                }
                if let Some(operation_id) = pending_sync.take() {
                    active_sync = Some((operation_id, Box::pin(time::sleep(sync_delay))));
                }
            }
        }
    }
}

enum CoreInput {
    Shutdown,
    CommandsClosed,
    DatabaseClosed,
    Command(Command),
    Database(DbReply),
    SyncElapsed,
}

async fn next_input(
    shutdown: &mut Pin<Box<oneshot::Receiver<()>>>,
    commands: &mut mpsc::Receiver<Command>,
    database_replies: &mut DatabaseReplies,
    active_sync: &mut Option<(super::OperationId, Pin<Box<time::Sleep>>)>,
    commands_first: bool,
) -> CoreInput {
    poll_fn(|context| {
        if shutdown.as_mut().poll(context).is_ready() {
            return Poll::Ready(CoreInput::Shutdown);
        }

        if commands_first {
            match commands.poll_recv(context) {
                Poll::Ready(Some(command)) => return Poll::Ready(CoreInput::Command(command)),
                Poll::Ready(None) => return Poll::Ready(CoreInput::CommandsClosed),
                Poll::Pending => {}
            }
        }

        match database_replies.poll_recv(context) {
            Poll::Ready(Some(reply)) => return Poll::Ready(CoreInput::Database(reply)),
            Poll::Ready(None) => return Poll::Ready(CoreInput::DatabaseClosed),
            Poll::Pending => {}
        }

        if let Some((_, delay)) = active_sync
            && delay.as_mut().poll(context).is_ready()
        {
            return Poll::Ready(CoreInput::SyncElapsed);
        }

        if !commands_first {
            match commands.poll_recv(context) {
                Poll::Ready(Some(command)) => return Poll::Ready(CoreInput::Command(command)),
                Poll::Ready(None) => return Poll::Ready(CoreInput::CommandsClosed),
                Poll::Pending => {}
            }
        }

        Poll::Pending
    })
    .await
}

async fn send_event(
    events: &EventSender,
    shutdown: &mut Pin<Box<oneshot::Receiver<()>>>,
    event: Event,
) -> bool {
    let mut delivery = Box::pin(events.send(event));
    poll_fn(|context| {
        if shutdown.as_mut().poll(context).is_ready() {
            Poll::Ready(false)
        } else {
            delivery.as_mut().poll(context).map(|result| result.is_ok())
        }
    })
    .await
}

#[derive(Default)]
struct MailboxSchedule {
    active: Option<MailboxRequestKey>,
    pending: Option<MailboxQuery>,
}

impl MailboxSchedule {
    fn enqueue(&mut self, query: MailboxQuery) -> Option<MailboxQuery> {
        if self.active.is_some() {
            self.pending = Some(query);
            None
        } else {
            self.active = Some(query.key());
            Some(query)
        }
    }

    fn complete(&mut self, key: MailboxRequestKey) -> MailboxCompletion {
        if self.active != Some(key) {
            return MailboxCompletion::Ignore;
        }

        self.active = None;
        if let Some(query) = self.pending.take() {
            self.active = Some(query.key());
            MailboxCompletion::Dispatch(query)
        } else {
            MailboxCompletion::Publish
        }
    }

    fn submission_failed(&mut self, key: MailboxRequestKey) {
        if self.active == Some(key) {
            self.active = None;
        }
    }
}

enum MailboxCompletion {
    Ignore,
    Publish,
    Dispatch(MailboxQuery),
}

fn submit_mailbox(
    database: &DatabaseClient,
    schedule: &mut MailboxSchedule,
    query: MailboxQuery,
) -> Option<Event> {
    let key = query.key();
    let result = database.try_query_mailbox(query.request_id, query.generation, query.spec);
    let reason = match result {
        Ok(()) => return None,
        Err(DatabaseSubmitError::Busy) => MailboxLoadError::Busy,
        Err(DatabaseSubmitError::Closed) => MailboxLoadError::Unavailable,
    };
    schedule.submission_failed(key);
    Some(Event::MailboxLoadRejected {
        request_id: key.request_id,
        generation: key.generation,
        reason,
    })
}

#[derive(Default)]
struct MessageSchedule {
    active: Option<MessageRequestKey>,
    pending: Option<MessageQuery>,
}

impl MessageSchedule {
    fn enqueue(&mut self, query: MessageQuery) -> Option<MessageQuery> {
        if self.active.is_some() {
            self.pending = Some(query);
            None
        } else {
            self.active = Some(query.key());
            Some(query)
        }
    }

    fn complete(&mut self, key: MessageRequestKey) -> MessageCompletion {
        if self.active != Some(key) {
            return MessageCompletion::Ignore;
        }

        self.active = None;
        if let Some(query) = self.pending.take() {
            self.active = Some(query.key());
            MessageCompletion::Dispatch(query)
        } else {
            MessageCompletion::Publish
        }
    }

    fn submission_failed(&mut self, key: MessageRequestKey) {
        if self.active == Some(key) {
            self.active = None;
        }
    }
}

enum MessageCompletion {
    Ignore,
    Publish,
    Dispatch(MessageQuery),
}

fn submit_message(
    database: &DatabaseClient,
    schedule: &mut MessageSchedule,
    query: MessageQuery,
) -> Option<Event> {
    let key = query.key();
    let result = database.try_open_message(query.request_id, query.generation, query.message_id);
    let reason = match result {
        Ok(()) => return None,
        Err(DatabaseSubmitError::Busy) => MessageLoadError::Busy,
        Err(DatabaseSubmitError::Closed) => MessageLoadError::Unavailable,
    };
    schedule.submission_failed(key);
    Some(Event::MessageLoadRejected {
        request_id: key.request_id,
        generation: key.generation,
        reason,
    })
}

pub(crate) struct CoreRuntime {
    shutdown: Option<oneshot::Sender<()>>,
    worker: Option<thread::JoinHandle<Result<(), RuntimeError>>>,
}

impl CoreRuntime {
    pub(crate) fn shutdown(mut self) -> Result<(), RuntimeError> {
        self.stop_and_join()
    }

    fn stop_and_join(&mut self) -> Result<(), RuntimeError> {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }

        let Some(worker) = self.worker.take() else {
            return Ok(());
        };
        worker
            .join()
            .map_err(|panic| RuntimeError::ThreadPanicked {
                message: panic_message(panic),
            })?
    }
}

impl Drop for CoreRuntime {
    fn drop(&mut self) {
        let _ = self.stop_and_join();
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RuntimeError {
    DatabaseClosed,
    DatabaseShutdown { message: Arc<str> },
    ThreadPanicked { message: Arc<str> },
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DatabaseClosed => formatter.write_str("SQLite actor stopped unexpectedly"),
            Self::DatabaseShutdown { message } => {
                write!(formatter, "could not stop SQLite actor: {message}")
            }
            Self::ThreadPanicked { message } => {
                write!(formatter, "core worker panicked: {message}")
            }
        }
    }
}

impl std::error::Error for RuntimeError {}

#[derive(Debug)]
pub(crate) enum StartError {
    Database(sqlite::StartError),
    Runtime(std::io::Error),
}

impl fmt::Display for StartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => write!(formatter, "could not start mail database: {error}"),
            Self::Runtime(error) => write!(formatter, "could not start core runtime: {error}"),
        }
    }
}

impl std::error::Error for StartError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::Runtime(error) => Some(error),
        }
    }
}

fn panic_message(panic: Box<dyn std::any::Any + Send>) -> Arc<str> {
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
    use super::*;
    use crate::core::{
        AccountScope, FolderScope, Generation, MailboxQuery, MessageId, MessageQuery, OperationId,
        PageSpec, RequestId,
    };
    use std::time::Instant;

    fn test_database() -> DatabaseParts {
        sqlite::spawn_in_memory().unwrap()
    }

    fn mailbox_query(request_id: u64, generation: u64) -> MailboxQuery {
        MailboxQuery::new(
            RequestId::new(request_id).unwrap(),
            Generation::new(generation),
            PageSpec::new(AccountScope::All, FolderScope::Inbox, None, None, 50).unwrap(),
        )
    }

    fn message_query(request_id: u64, generation: u64, message_id: i64) -> MessageQuery {
        MessageQuery::new(
            RequestId::new(request_id).unwrap(),
            Generation::new(generation),
            MessageId::new(message_id).unwrap(),
        )
    }

    #[test]
    fn sync_round_trip_preserves_operation_id() {
        let (core, mut events, runtime) = spawn_with_delay(Duration::from_millis(1)).unwrap();
        let operation_id = OperationId::new(42);

        core.try_send_sync(operation_id).unwrap();

        assert_eq!(
            events.blocking_recv(),
            Some(Event::SyncFinished { operation_id })
        );
        runtime.shutdown().unwrap();
    }

    #[test]
    fn shutdown_interrupts_pending_sync() {
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let (core, _events, runtime) = spawn_with_options(
            Duration::from_secs(5),
            EVENT_CAPACITY,
            test_database(),
            Some(started_tx),
        )
        .unwrap();
        core.try_send_sync(OperationId::new(1)).unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let started = Instant::now();

        runtime.shutdown().unwrap();

        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn mailbox_query_completes_while_sync_is_active() {
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let (core, mut events, runtime) = spawn_with_options(
            Duration::from_secs(5),
            EVENT_CAPACITY,
            test_database(),
            Some(started_tx),
        )
        .unwrap();
        core.try_send_sync(OperationId::new(1)).unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        core.try_query_mailbox(mailbox_query(5, 5)).unwrap();

        let event = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(1), events.recv())
                    .await
                    .unwrap()
            });
        assert!(matches!(event, Some(Event::MailboxLoaded(_))));
        runtime.shutdown().unwrap();
    }

    #[test]
    fn full_event_queue_applies_backpressure_without_stopping_core() {
        let (core, mut events, runtime) =
            spawn_with_options(Duration::from_millis(1), 1, test_database(), None).unwrap();
        let first = OperationId::new(1);
        let second = OperationId::new(2);
        core.try_send_sync(first).unwrap();
        core.try_send_sync(second).unwrap();

        assert_eq!(
            events.blocking_recv(),
            Some(Event::SyncFinished {
                operation_id: first
            })
        );
        assert_eq!(
            events.blocking_recv(),
            Some(Event::SyncFinished {
                operation_id: second
            })
        );
        runtime.shutdown().unwrap();
    }

    #[test]
    fn mailbox_query_round_trip_preserves_request_identity() {
        let (core, mut events, runtime) = spawn_with_delay(Duration::from_millis(1)).unwrap();
        let query = mailbox_query(7, 3);

        core.try_query_mailbox(query).unwrap();

        let Some(Event::MailboxLoaded(reply)) = events.blocking_recv() else {
            panic!("expected mailbox page");
        };
        assert_eq!(reply.request_id, RequestId::new(7).unwrap());
        assert_eq!(reply.generation, Generation::new(3));
        assert!(reply.result.unwrap().rows.is_empty());
        runtime.shutdown().unwrap();
    }

    #[test]
    fn message_query_round_trip_preserves_request_identity() {
        let (core, mut events, runtime) = spawn_with_delay(Duration::from_millis(1)).unwrap();

        core.try_open_message(message_query(8, 4, 1)).unwrap();

        let Some(Event::MessageLoaded(reply)) = events.blocking_recv() else {
            panic!("expected message detail");
        };
        assert_eq!(reply.request_id, RequestId::new(8).unwrap());
        assert_eq!(reply.generation, Generation::new(4));
        assert_eq!(reply.result.unwrap(), None);
        runtime.shutdown().unwrap();
    }

    #[test]
    fn mailbox_schedule_replaces_obsolete_pending_query() {
        let mut schedule = MailboxSchedule::default();
        let first = mailbox_query(1, 1);
        let first_key = first.key();
        let obsolete = mailbox_query(2, 2);
        let latest = mailbox_query(3, 3);
        let latest_key = latest.key();

        assert!(schedule.enqueue(first).is_some());
        assert!(schedule.enqueue(obsolete).is_none());
        assert!(schedule.enqueue(latest).is_none());

        let MailboxCompletion::Dispatch(next) = schedule.complete(first_key) else {
            panic!("latest pending query should be dispatched");
        };
        assert_eq!(next.key(), latest_key);
        assert!(matches!(
            schedule.complete(latest_key),
            MailboxCompletion::Publish
        ));
    }

    #[test]
    fn message_schedule_replaces_obsolete_pending_query() {
        let mut schedule = MessageSchedule::default();
        let first = message_query(1, 1, 1);
        let first_key = first.key();
        let obsolete = message_query(2, 2, 2);
        let latest = message_query(3, 3, 3);
        let latest_key = latest.key();

        assert!(schedule.enqueue(first).is_some());
        assert!(schedule.enqueue(obsolete).is_none());
        assert!(schedule.enqueue(latest).is_none());

        let MessageCompletion::Dispatch(next) = schedule.complete(first_key) else {
            panic!("latest pending detail should be dispatched");
        };
        assert_eq!(next.key(), latest_key);
        assert!(matches!(
            schedule.complete(latest_key),
            MessageCompletion::Publish
        ));
    }

    #[test]
    fn command_burst_yields_to_a_ready_database_reply() {
        let (database, mut database_replies, database_runtime, _) = test_database();
        let query = mailbox_query(6, 6);
        database
            .try_query_mailbox(query.request_id, query.generation, query.spec)
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while database_replies.is_empty() && Instant::now() < deadline {
            thread::yield_now();
        }
        assert!(!database_replies.is_empty());

        let (command_tx, mut commands) = mpsc::channel(1);
        command_tx
            .try_send(Command::SyncNow {
                operation_id: OperationId::new(1),
            })
            .unwrap();
        let (_shutdown_tx, shutdown_rx) = oneshot::channel();
        let mut shutdown = Box::pin(shutdown_rx);
        let mut active_sync = None;
        let input = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(next_input(
                &mut shutdown,
                &mut commands,
                &mut database_replies,
                &mut active_sync,
                false,
            ));

        assert!(matches!(input, CoreInput::Database(DbReply::Mailbox(_))));
        database_runtime.shutdown().unwrap();
    }

    #[test]
    fn shutdown_interrupts_mailbox_event_backpressure() {
        let (core, events, runtime) =
            spawn_with_options(Duration::from_millis(1), 1, test_database(), None).unwrap();
        core.try_send_sync(OperationId::new(1)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while events.is_empty() && Instant::now() < deadline {
            thread::yield_now();
        }
        assert_eq!(events.len(), 1, "sync event should fill the event queue");
        core.try_query_mailbox(mailbox_query(4, 4)).unwrap();
        thread::sleep(Duration::from_millis(20));
        let started = Instant::now();

        runtime.shutdown().unwrap();

        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
