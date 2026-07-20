use super::{
    account::{
        AccountDriverError, AccountWorkflows, ImapDiagnosticProbe, production_imap_diagnostic,
    },
    compose::{ComposeSuccess, execute_compose},
    message::{
        ACCOUNT_COMMAND_CAPACITY, AccountCommand, AccountDirectoryLoadError, AccountDirectoryQuery,
        AccountDirectoryRequestKey, COMMAND_CAPACITY, COMPOSE_COMMAND_CAPACITY, Command,
        ComposeCommand, CoreHandle, EVENT_CAPACITY, Event, EventReceiver, EventSender,
        MailboxLoadError, MailboxQuery, MailboxRequestKey, MessageLoadError, MessageQuery,
        MessageRequestKey, MutationRequest, MutationSubmitError, event_channel,
    },
    outbox_driver::{
        OutboxStatus, SmtpCancellationTracker, SmtpSubmitProbe, production_smtp_submit,
        run_outbox_driver,
    },
    sync::{ImapInboxFetchProbe, production_imap_inbox_fetch},
};
use crate::{
    content::{ContentStaging, StorageError},
    credentials::{self, CredentialClient, CredentialRuntime},
    store::sqlite::{
        self, DatabaseClient, DatabaseInfo, DatabaseReplies, DatabaseRuntime, DatabaseSubmitError,
        DbReply, MutationOutcome,
    },
};
use std::{
    collections::VecDeque,
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

const COMMAND_BURST_LIMIT: u8 = 8;
const OUTBOX_STATUS_CAPACITY: usize = 16;
const OUTBOX_WAKE_CAPACITY: usize = 1;
const FILE_GC_WAKE_CAPACITY: usize = 1;
const FILE_GC_BATCH_LIMIT: usize = 16;
const FILE_GC_INITIAL_DELAY: Duration = Duration::from_millis(50);
const FILE_GC_BATCH_DELAY: Duration = Duration::from_millis(10);
const FILE_GC_SUBMIT_RETRY_DELAY: Duration = Duration::from_millis(10);
const FILE_GC_FAILURE_RETRY_DELAY: Duration = Duration::from_secs(300);
const OUTBOX_SHUTDOWN_GRACE: Duration = Duration::from_secs(1);
const FILE_GC_SHUTDOWN_GRACE: Duration = Duration::from_secs(1);
const SHUTDOWN_QUERY_INTERRUPT_INTERVAL: Duration = Duration::from_millis(10);

type DatabaseParts = (
    DatabaseClient,
    DatabaseReplies,
    DatabaseRuntime,
    DatabaseInfo,
);

type CredentialParts = (CredentialClient, CredentialRuntime);

#[cfg_attr(feature = "bench-harness", allow(dead_code))]
pub(crate) fn spawn(
    database_path: PathBuf,
) -> Result<(CoreHandle, EventReceiver, CoreRuntime), StartError> {
    let content_root = database_path.with_file_name("content");
    let database = sqlite::spawn(database_path).map_err(StartError::Database)?;
    spawn_with_sync_components(
        EVENT_CAPACITY,
        database,
        credentials::spawn(),
        production_imap_diagnostic,
        production_imap_inbox_fetch,
        production_smtp_submit,
        Some(content_root),
    )
}

#[cfg(feature = "bench-harness")]
pub(crate) fn spawn_with_database(
    database_path: PathBuf,
) -> Result<(CoreHandle, EventReceiver, CoreRuntime, DatabaseClient), StartError> {
    let content_root = database_path.with_file_name("content");
    let database = sqlite::spawn(database_path).map_err(StartError::Database)?;
    let benchmark_database = database.0.clone();
    let (core, events, runtime) = spawn_with_sync_components(
        EVENT_CAPACITY,
        database,
        credentials::spawn(),
        production_imap_diagnostic,
        production_imap_inbox_fetch,
        production_smtp_submit,
        Some(content_root),
    )?;
    Ok((core, events, runtime, benchmark_database))
}

#[cfg(test)]
fn spawn_for_test() -> Result<(CoreHandle, EventReceiver, CoreRuntime), StartError> {
    let database = sqlite::spawn_in_memory().map_err(StartError::Database)?;
    spawn_with_options(EVENT_CAPACITY, database)
}

#[cfg(test)]
fn spawn_with_options(
    event_capacity: usize,
    database: DatabaseParts,
) -> Result<(CoreHandle, EventReceiver, CoreRuntime), StartError> {
    spawn_with_components(event_capacity, database, credentials::spawn())
}

#[cfg(test)]
fn spawn_with_components(
    event_capacity: usize,
    database: DatabaseParts,
    credentials: CredentialParts,
) -> Result<(CoreHandle, EventReceiver, CoreRuntime), StartError> {
    spawn_with_components_and_probe(
        event_capacity,
        database,
        credentials,
        production_imap_diagnostic,
    )
}

#[cfg(test)]
fn spawn_with_components_and_probe(
    event_capacity: usize,
    database: DatabaseParts,
    credentials: CredentialParts,
    diagnostic_probe: ImapDiagnosticProbe,
) -> Result<(CoreHandle, EventReceiver, CoreRuntime), StartError> {
    spawn_with_sync_components(
        event_capacity,
        database,
        credentials,
        diagnostic_probe,
        production_imap_inbox_fetch,
        production_smtp_submit,
        None,
    )
}

fn spawn_with_sync_components(
    event_capacity: usize,
    database: DatabaseParts,
    credentials: CredentialParts,
    diagnostic_probe: ImapDiagnosticProbe,
    inbox_fetch_probe: ImapInboxFetchProbe,
    smtp_submit_probe: SmtpSubmitProbe,
    content_root: Option<PathBuf>,
) -> Result<(CoreHandle, EventReceiver, CoreRuntime), StartError> {
    let staging = content_root
        .as_ref()
        .map(|root| ContentStaging::open(root.clone()).map(Arc::new))
        .transpose()
        .map_err(StartError::Content)?;
    let (database, database_replies, database_runtime, _database_info) = database;
    let (credentials, credential_runtime) = credentials;
    let (command_tx, command_rx) = mpsc::channel(COMMAND_CAPACITY);
    let (account_command_tx, account_command_rx) = mpsc::channel(ACCOUNT_COMMAND_CAPACITY);
    let (compose_command_tx, compose_command_rx) = mpsc::channel(COMPOSE_COMMAND_CAPACITY);
    let (outbox_wakeup_tx, outbox_wakeup_rx) = mpsc::channel(OUTBOX_WAKE_CAPACITY);
    let (outbox_status_tx, outbox_status_rx) = mpsc::channel(OUTBOX_STATUS_CAPACITY);
    let (file_gc_wakeup_tx, file_gc_wakeup_rx) = mpsc::channel(FILE_GC_WAKE_CAPACITY);
    let outbox_file_gc_wakeup_tx = file_gc_wakeup_tx.clone();
    let smtp_cancellations = SmtpCancellationTracker::default();
    let driver_smtp_cancellations = smtp_cancellations.clone();
    let worker_smtp_cancellations = smtp_cancellations.clone();
    let runtime_smtp_cancellations = smtp_cancellations.clone();
    let compose_command_rx = staging.as_ref().map(|_| compose_command_rx);
    let outbox_status_rx = staging.as_ref().map(|_| outbox_status_rx);
    let (event_tx, event_rx) = event_channel(event_capacity);
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let runtime = Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .max_blocking_threads(2)
        .build()
        .map_err(StartError::Runtime)?;
    let worker = thread::Builder::new()
        .name("nivalis-core".into())
        .spawn(move || {
            let outbox_credentials = credentials.clone();
            let file_gc_task = staging.as_ref().map(|staging| {
                runtime.spawn(run_file_janitor(
                    database.clone(),
                    Arc::clone(staging),
                    file_gc_wakeup_rx,
                ))
            });
            let account_workflows = AccountWorkflows::with_sync(
                database.clone(),
                credentials,
                diagnostic_probe,
                inbox_fetch_probe,
                content_root,
            );
            let outbox_task = staging.as_ref().map(|staging| {
                runtime.spawn(run_outbox_driver(
                    database.clone(),
                    outbox_credentials,
                    Arc::clone(staging),
                    outbox_wakeup_rx,
                    outbox_status_tx,
                    outbox_file_gc_wakeup_tx,
                    driver_smtp_cancellations,
                    smtp_submit_probe,
                ))
            });
            let core_result = runtime.block_on(run_core(
                command_rx,
                event_tx,
                shutdown_rx,
                database,
                database_replies,
                account_command_rx,
                account_workflows,
                compose_command_rx,
                staging,
                outbox_wakeup_tx,
                outbox_status_rx,
                file_gc_wakeup_tx,
            ));
            worker_smtp_cancellations.begin_shutdown();
            if let Some(mut outbox_task) = outbox_task
                && runtime
                    .block_on(async {
                        time::timeout(OUTBOX_SHUTDOWN_GRACE, &mut outbox_task).await
                    })
                    .is_err()
            {
                outbox_task.abort();
                let _ = runtime.block_on(outbox_task);
            }
            if let Some(mut file_gc_task) = file_gc_task
                && runtime
                    .block_on(async {
                        time::timeout(FILE_GC_SHUTDOWN_GRACE, &mut file_gc_task).await
                    })
                    .is_err()
            {
                file_gc_task.abort();
                let _ = runtime.block_on(file_gc_task);
            }
            let credential_result =
                credential_runtime
                    .shutdown()
                    .map_err(|error| RuntimeError::CredentialShutdown {
                        message: Arc::from(error.to_string()),
                    });
            let database_result =
                database_runtime
                    .shutdown()
                    .map_err(|error| RuntimeError::DatabaseShutdown {
                        message: Arc::from(error.to_string()),
                    });
            match core_result {
                Err(error) => Err(error),
                Ok(()) => credential_result.and(database_result),
            }
        })
        .map_err(StartError::Runtime)?;

    Ok((
        CoreHandle::new(
            command_tx,
            account_command_tx,
            compose_command_tx,
            smtp_cancellations,
        ),
        event_rx,
        CoreRuntime {
            shutdown: Some(shutdown_tx),
            worker: Some(worker),
            smtp_cancellations: runtime_smtp_cancellations,
        },
    ))
}

#[allow(clippy::too_many_arguments)]
async fn run_core(
    mut commands: mpsc::Receiver<Command>,
    events: EventSender,
    shutdown: oneshot::Receiver<()>,
    database: DatabaseClient,
    mut database_replies: DatabaseReplies,
    account_commands: mpsc::Receiver<AccountCommand>,
    mut account_workflows: AccountWorkflows,
    compose_commands: Option<mpsc::Receiver<ComposeCommand>>,
    staging: Option<Arc<ContentStaging>>,
    outbox_wakeups: mpsc::Sender<()>,
    outbox_statuses: Option<mpsc::Receiver<OutboxStatus>>,
    file_gc_wakeups: mpsc::Sender<()>,
) -> Result<(), RuntimeError> {
    let mut shutdown = Box::pin(shutdown);
    let mut accounts = AccountDirectorySchedule::default();
    let mut mailbox = MailboxSchedule::default();
    let mut message = MessageSchedule::default();
    let mut active_mutations = 0_usize;
    let mut command_streak = 0_u8;
    let mut account_commands = Some(account_commands);
    let mut compose_commands = compose_commands;
    let mut outbox_statuses = outbox_statuses;

    loop {
        let input = next_input(
            &mut shutdown,
            &mut commands,
            &mut database_replies,
            &mut account_commands,
            &mut account_workflows,
            &mut compose_commands,
            &mut outbox_statuses,
            command_streak < COMMAND_BURST_LIMIT,
        )
        .await;
        if matches!(&input, CoreInput::Command(_)) {
            command_streak = command_streak.saturating_add(1);
        } else {
            command_streak = 0;
        }

        match input {
            CoreInput::Shutdown | CoreInput::CommandsClosed => {
                return finish_accepted_work(
                    &mut commands,
                    &mut compose_commands,
                    staging.as_deref(),
                    &outbox_wakeups,
                    &database,
                    &mut database_replies,
                    active_mutations,
                    None,
                )
                .await;
            }
            CoreInput::DatabaseClosed => return Err(RuntimeError::DatabaseClosed),
            CoreInput::AccountCommandsClosed => account_commands = None,
            CoreInput::ComposeCommandsClosed => compose_commands = None,
            CoreInput::OutboxStatusesClosed => outbox_statuses = None,
            CoreInput::AccountProgress => {
                let _ = file_gc_wakeups.try_send(());
            }
            CoreInput::AccountFailure(error) => return Err(error.into()),
            CoreInput::AccountCommand(command) => {
                account_workflows
                    .start_user_operation(command.operation, command.reply)
                    .map_err(RuntimeError::from)?;
            }
            CoreInput::ComposeCommand(command) => {
                let Some(staging) = staging.as_ref() else {
                    drop(command.reply);
                    continue;
                };
                let reply = execute_compose(
                    &database,
                    staging.as_ref(),
                    &outbox_wakeups,
                    command.operation,
                )
                .await;
                let collect_files = matches!(
                    &reply,
                    Ok(ComposeSuccess::Saved(_)
                        | ComposeSuccess::Queued { .. }
                        | ComposeSuccess::Recovering
                        | ComposeSuccess::OutboxReleased { .. }
                        | ComposeSuccess::UncertainOutboxResolved { .. })
                );
                let _ = command.reply.send(reply);
                if collect_files {
                    let _ = file_gc_wakeups.try_send(());
                }
            }
            CoreInput::OutboxStatus(status) => {
                if !send_event(&events, &mut shutdown, Event::OutboxStatus(status)).await {
                    return finish_accepted_work(
                        &mut commands,
                        &mut compose_commands,
                        staging.as_deref(),
                        &outbox_wakeups,
                        &database,
                        &mut database_replies,
                        active_mutations,
                        None,
                    )
                    .await;
                }
            }
            CoreInput::Command(Command::QueryAccountDirectory(query)) => {
                if let Some(query) = accounts.enqueue(query)
                    && let Some(event) = submit_account_directory(&database, &mut accounts, query)
                    && !send_event(&events, &mut shutdown, event).await
                {
                    return finish_accepted_work(
                        &mut commands,
                        &mut compose_commands,
                        staging.as_deref(),
                        &outbox_wakeups,
                        &database,
                        &mut database_replies,
                        active_mutations,
                        None,
                    )
                    .await;
                }
            }
            CoreInput::Command(Command::QueryMailbox(query)) => {
                let dispatch = match mailbox.enqueue(query) {
                    MailboxEnqueue::Dispatch(query) => Some(query),
                    MailboxEnqueue::Supersede(key) => {
                        database.supersede_mailbox_query(key.request_id, key.generation);
                        None
                    }
                };
                if let Some(query) = dispatch
                    && let Some(event) = submit_mailbox(&database, &mut mailbox, query)
                    && !send_event(&events, &mut shutdown, event).await
                {
                    return finish_accepted_work(
                        &mut commands,
                        &mut compose_commands,
                        staging.as_deref(),
                        &outbox_wakeups,
                        &database,
                        &mut database_replies,
                        active_mutations,
                        None,
                    )
                    .await;
                }
            }
            CoreInput::Command(Command::OpenMessage(query)) => {
                if let Some(query) = message.enqueue(query)
                    && let Some(event) = submit_message(&database, &mut message, query)
                    && !send_event(&events, &mut shutdown, event).await
                {
                    return finish_accepted_work(
                        &mut commands,
                        &mut compose_commands,
                        staging.as_deref(),
                        &outbox_wakeups,
                        &database,
                        &mut database_replies,
                        active_mutations,
                        None,
                    )
                    .await;
                }
            }
            CoreInput::Command(Command::Mutate(request)) => {
                match submit_mutation(&database, request) {
                    MutationSubmission::Submitted => {
                        active_mutations = active_mutations
                            .checked_add(1)
                            .ok_or(RuntimeError::MutationAccounting)?;
                    }
                    MutationSubmission::Rejected { event, request } => {
                        if !send_event(&events, &mut shutdown, event).await {
                            return finish_accepted_work(
                                &mut commands,
                                &mut compose_commands,
                                staging.as_deref(),
                                &outbox_wakeups,
                                &database,
                                &mut database_replies,
                                active_mutations,
                                Some(request),
                            )
                            .await;
                        }
                    }
                }
            }
            #[cfg(test)]
            CoreInput::Command(Command::Barrier(reached)) => {
                let _ = reached.send(());
            }
            CoreInput::Database(DbReply::Accounts(reply)) => {
                let key = AccountDirectoryRequestKey {
                    request_id: reply.request_id,
                    generation: reply.generation,
                };
                match accounts.complete(key) {
                    AccountDirectoryCompletion::Ignore => {}
                    AccountDirectoryCompletion::Publish => {
                        if !send_event(&events, &mut shutdown, Event::AccountsLoaded(reply)).await {
                            return finish_accepted_work(
                                &mut commands,
                                &mut compose_commands,
                                staging.as_deref(),
                                &outbox_wakeups,
                                &database,
                                &mut database_replies,
                                active_mutations,
                                None,
                            )
                            .await;
                        }
                    }
                    AccountDirectoryCompletion::Dispatch(query) => {
                        if let Some(event) =
                            submit_account_directory(&database, &mut accounts, query)
                            && !send_event(&events, &mut shutdown, event).await
                        {
                            return finish_accepted_work(
                                &mut commands,
                                &mut compose_commands,
                                staging.as_deref(),
                                &outbox_wakeups,
                                &database,
                                &mut database_replies,
                                active_mutations,
                                None,
                            )
                            .await;
                        }
                    }
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
                            return finish_accepted_work(
                                &mut commands,
                                &mut compose_commands,
                                staging.as_deref(),
                                &outbox_wakeups,
                                &database,
                                &mut database_replies,
                                active_mutations,
                                None,
                            )
                            .await;
                        }
                    }
                    MailboxCompletion::Dispatch(query) => {
                        if let Some(event) = submit_mailbox(&database, &mut mailbox, query)
                            && !send_event(&events, &mut shutdown, event).await
                        {
                            return finish_accepted_work(
                                &mut commands,
                                &mut compose_commands,
                                staging.as_deref(),
                                &outbox_wakeups,
                                &database,
                                &mut database_replies,
                                active_mutations,
                                None,
                            )
                            .await;
                        }
                    }
                }
            }
            CoreInput::Database(DbReply::MailboxSuperseded {
                request_id,
                generation,
            }) => {
                let key = MailboxRequestKey {
                    request_id,
                    generation,
                };
                if let MailboxCompletion::Dispatch(query) = mailbox.complete(key)
                    && let Some(event) = submit_mailbox(&database, &mut mailbox, query)
                    && !send_event(&events, &mut shutdown, event).await
                {
                    return finish_accepted_work(
                        &mut commands,
                        &mut compose_commands,
                        staging.as_deref(),
                        &outbox_wakeups,
                        &database,
                        &mut database_replies,
                        active_mutations,
                        None,
                    )
                    .await;
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
                            return finish_accepted_work(
                                &mut commands,
                                &mut compose_commands,
                                staging.as_deref(),
                                &outbox_wakeups,
                                &database,
                                &mut database_replies,
                                active_mutations,
                                None,
                            )
                            .await;
                        }
                    }
                    MessageCompletion::Dispatch(query) => {
                        if let Some(event) = submit_message(&database, &mut message, query)
                            && !send_event(&events, &mut shutdown, event).await
                        {
                            return finish_accepted_work(
                                &mut commands,
                                &mut compose_commands,
                                staging.as_deref(),
                                &outbox_wakeups,
                                &database,
                                &mut database_replies,
                                active_mutations,
                                None,
                            )
                            .await;
                        }
                    }
                }
            }
            CoreInput::Database(DbReply::Mutation(reply)) => {
                active_mutations = active_mutations
                    .checked_sub(1)
                    .ok_or(RuntimeError::MutationAccounting)?;
                let collect_files = matches!(
                    &reply.result,
                    Ok(MutationOutcome::PermanentlyDeleted { .. })
                );
                let undelivered_failure = reply
                    .result
                    .as_ref()
                    .err()
                    .map(|failure| Arc::from(failure.to_string()));
                if !send_event(&events, &mut shutdown, Event::MutationFinished(reply)).await {
                    let finish = finish_accepted_work(
                        &mut commands,
                        &mut compose_commands,
                        staging.as_deref(),
                        &outbox_wakeups,
                        &database,
                        &mut database_replies,
                        active_mutations,
                        None,
                    )
                    .await;
                    return match (finish, undelivered_failure) {
                        (Err(error), _) => Err(error),
                        (Ok(()), Some(message)) => {
                            Err(RuntimeError::MutationDuringShutdown { message })
                        }
                        (Ok(()), None) => Ok(()),
                    };
                }
                if collect_files {
                    let _ = file_gc_wakeups.try_send(());
                }
            }
        }
    }
}

enum MutationSubmission {
    Submitted,
    Rejected {
        event: Event,
        request: MutationRequest,
    },
}

fn submit_mutation(database: &DatabaseClient, mut request: MutationRequest) -> MutationSubmission {
    let request_id = request.request_id;
    let generation = request.generation;
    let (reason, mutation) =
        match database.try_mutate_recover(request_id, generation, request.mutation) {
            Ok(()) => return MutationSubmission::Submitted,
            Err((DatabaseSubmitError::Busy, mutation)) => (MutationSubmitError::Busy, mutation),
            Err((DatabaseSubmitError::Closed, mutation)) => {
                (MutationSubmitError::Unavailable, mutation)
            }
        };
    request.mutation = mutation;
    MutationSubmission::Rejected {
        event: Event::MutationRejected {
            request_id,
            generation,
            reason,
        },
        request,
    }
}

#[allow(clippy::too_many_arguments)]
async fn finish_accepted_work(
    commands: &mut mpsc::Receiver<Command>,
    compose_commands: &mut Option<mpsc::Receiver<ComposeCommand>>,
    staging: Option<&ContentStaging>,
    outbox_wakeups: &mpsc::Sender<()>,
    database: &DatabaseClient,
    database_replies: &mut DatabaseReplies,
    active_mutations: usize,
    initial_request: Option<MutationRequest>,
) -> Result<(), RuntimeError> {
    let mutation_result = finish_accepted_mutations_with(
        commands,
        database,
        database_replies,
        active_mutations,
        initial_request,
    )
    .await;
    finish_accepted_compose(compose_commands, staging, database, outbox_wakeups).await;
    mutation_result
}

async fn finish_accepted_compose(
    compose_commands: &mut Option<mpsc::Receiver<ComposeCommand>>,
    staging: Option<&ContentStaging>,
    database: &DatabaseClient,
    outbox_wakeups: &mpsc::Sender<()>,
) {
    let (Some(compose_commands), Some(staging)) = (compose_commands.as_mut(), staging) else {
        return;
    };
    compose_commands.close();
    while let Ok(command) = compose_commands.try_recv() {
        let reply = execute_compose(database, staging, outbox_wakeups, command.operation).await;
        let _ = command.reply.send(reply);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FileJanitorExit {
    WakeChannel,
    Database,
}

async fn run_file_janitor(
    database: DatabaseClient,
    staging: Arc<ContentStaging>,
    mut wakeups: mpsc::Receiver<()>,
) -> FileJanitorExit {
    let mut delay = FILE_GC_INITIAL_DELAY;
    let mut shutting_down = false;
    loop {
        if !shutting_down && !wait_for_file_gc_delay(&mut wakeups, delay).await {
            shutting_down = true;
        }

        let receiver = match database.try_run_file_gc(&staging, FILE_GC_BATCH_LIMIT) {
            Ok(receiver) => receiver,
            Err(DatabaseSubmitError::Busy) => {
                if shutting_down {
                    return FileJanitorExit::WakeChannel;
                }
                delay = FILE_GC_SUBMIT_RETRY_DELAY;
                continue;
            }
            Err(DatabaseSubmitError::Closed) => return FileJanitorExit::Database,
        };
        let outcome = match receiver.await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) => {
                if shutting_down {
                    return FileJanitorExit::WakeChannel;
                }
                delay = FILE_GC_FAILURE_RETRY_DELAY;
                continue;
            }
            Err(_) => return FileJanitorExit::Database,
        };

        if outcome.has_pending {
            let progress = outcome
                .referenced
                .saturating_add(outcome.removed)
                .saturating_add(outcome.missing);
            if shutting_down {
                if progress == 0 {
                    return FileJanitorExit::WakeChannel;
                }
                continue;
            }
            delay = if progress == 0 {
                FILE_GC_FAILURE_RETRY_DELAY
            } else {
                FILE_GC_BATCH_DELAY
            };
            continue;
        }

        if shutting_down {
            return FileJanitorExit::WakeChannel;
        }

        match wakeups.recv().await {
            Some(()) => delay = FILE_GC_INITIAL_DELAY,
            None => shutting_down = true,
        }
    }
}

async fn wait_for_file_gc_delay(wakeups: &mut mpsc::Receiver<()>, delay: Duration) -> bool {
    let mut sleep = Box::pin(time::sleep(delay));
    poll_fn(|context| {
        loop {
            match wakeups.poll_recv(context) {
                Poll::Ready(Some(())) => {}
                Poll::Ready(None) => return Poll::Ready(false),
                Poll::Pending => break,
            }
        }
        sleep.as_mut().poll(context).map(|()| true)
    })
    .await
}

async fn finish_accepted_mutations_with(
    commands: &mut mpsc::Receiver<Command>,
    database: &DatabaseClient,
    database_replies: &mut DatabaseReplies,
    mut active_mutations: usize,
    initial_request: Option<MutationRequest>,
) -> Result<(), RuntimeError> {
    commands.close();
    let mut pending =
        VecDeque::with_capacity(commands.len() + usize::from(initial_request.is_some()));
    pending.extend(initial_request);
    while let Ok(command) = commands.try_recv() {
        if let Command::Mutate(request) = command {
            pending.push_back(request);
        }
    }

    let mut first_failure = None;
    database.interrupt_queries();
    while !pending.is_empty() || active_mutations != 0 {
        while let Some(mut request) = pending.pop_front() {
            match database.try_mutate_recover(
                request.request_id,
                request.generation,
                request.mutation,
            ) {
                Ok(()) => {
                    active_mutations = active_mutations
                        .checked_add(1)
                        .ok_or(RuntimeError::MutationAccounting)?;
                }
                Err((DatabaseSubmitError::Busy, mutation)) => {
                    request.mutation = mutation;
                    pending.push_front(request);
                    break;
                }
                Err((DatabaseSubmitError::Closed, _)) => {
                    return Err(RuntimeError::DatabaseClosed);
                }
            }
        }

        if pending.is_empty() && active_mutations == 0 {
            break;
        }
        let reply = loop {
            match time::timeout(SHUTDOWN_QUERY_INTERRUPT_INTERVAL, database_replies.recv()).await {
                Ok(Some(reply)) => break reply,
                Ok(None) => return Err(RuntimeError::DatabaseClosed),
                Err(_) => database.interrupt_queries(),
            }
        };
        if let DbReply::Mutation(reply) = reply {
            active_mutations = active_mutations
                .checked_sub(1)
                .ok_or(RuntimeError::MutationAccounting)?;
            if let Err(failure) = reply.result
                && first_failure.is_none()
            {
                first_failure = Some(Arc::from(failure.to_string()));
            }
        }
    }

    if let Some(message) = first_failure {
        Err(RuntimeError::MutationDuringShutdown { message })
    } else {
        Ok(())
    }
}

enum CoreInput {
    Shutdown,
    CommandsClosed,
    AccountCommandsClosed,
    ComposeCommandsClosed,
    OutboxStatusesClosed,
    DatabaseClosed,
    Command(Command),
    AccountCommand(AccountCommand),
    ComposeCommand(ComposeCommand),
    OutboxStatus(OutboxStatus),
    AccountProgress,
    AccountFailure(AccountDriverError),
    Database(DbReply),
}

#[allow(clippy::too_many_arguments)]
async fn next_input(
    shutdown: &mut Pin<Box<oneshot::Receiver<()>>>,
    commands: &mut mpsc::Receiver<Command>,
    database_replies: &mut DatabaseReplies,
    account_commands: &mut Option<mpsc::Receiver<AccountCommand>>,
    account_workflows: &mut AccountWorkflows,
    compose_commands: &mut Option<mpsc::Receiver<ComposeCommand>>,
    outbox_statuses: &mut Option<mpsc::Receiver<OutboxStatus>>,
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

        match account_workflows.poll_progress(context) {
            Poll::Ready(Ok(())) => return Poll::Ready(CoreInput::AccountProgress),
            Poll::Ready(Err(error)) => return Poll::Ready(CoreInput::AccountFailure(error)),
            Poll::Pending => {}
        }

        if let Some(outbox_statuses) = outbox_statuses.as_mut() {
            match outbox_statuses.poll_recv(context) {
                Poll::Ready(Some(status)) => return Poll::Ready(CoreInput::OutboxStatus(status)),
                Poll::Ready(None) => return Poll::Ready(CoreInput::OutboxStatusesClosed),
                Poll::Pending => {}
            }
        }

        if account_workflows.can_start_user_operation()
            && let Some(account_commands) = account_commands.as_mut()
        {
            match account_commands.poll_recv(context) {
                Poll::Ready(Some(command)) => {
                    return Poll::Ready(CoreInput::AccountCommand(command));
                }
                Poll::Ready(None) => return Poll::Ready(CoreInput::AccountCommandsClosed),
                Poll::Pending => {}
            }
        }

        if let Some(compose_commands) = compose_commands.as_mut() {
            match compose_commands.poll_recv(context) {
                Poll::Ready(Some(command)) => {
                    return Poll::Ready(CoreInput::ComposeCommand(command));
                }
                Poll::Ready(None) => return Poll::Ready(CoreInput::ComposeCommandsClosed),
                Poll::Pending => {}
            }
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
struct AccountDirectorySchedule {
    active: Option<AccountDirectoryRequestKey>,
    pending: Option<AccountDirectoryQuery>,
}

impl AccountDirectorySchedule {
    fn enqueue(&mut self, query: AccountDirectoryQuery) -> Option<AccountDirectoryQuery> {
        if self.active.is_some() {
            self.pending = Some(query);
            None
        } else {
            self.active = Some(query.key());
            Some(query)
        }
    }

    fn complete(&mut self, key: AccountDirectoryRequestKey) -> AccountDirectoryCompletion {
        if self.active != Some(key) {
            return AccountDirectoryCompletion::Ignore;
        }

        self.active = None;
        if let Some(query) = self.pending.take() {
            self.active = Some(query.key());
            AccountDirectoryCompletion::Dispatch(query)
        } else {
            AccountDirectoryCompletion::Publish
        }
    }

    fn submission_failed(&mut self, key: AccountDirectoryRequestKey) {
        if self.active == Some(key) {
            self.active = None;
        }
    }
}

enum AccountDirectoryCompletion {
    Ignore,
    Publish,
    Dispatch(AccountDirectoryQuery),
}

fn submit_account_directory(
    database: &DatabaseClient,
    schedule: &mut AccountDirectorySchedule,
    query: AccountDirectoryQuery,
) -> Option<Event> {
    let key = query.key();
    let reason = match database.try_query_account_directory(query.request_id, query.generation) {
        Ok(()) => return None,
        Err(DatabaseSubmitError::Busy) => AccountDirectoryLoadError::Busy,
        Err(DatabaseSubmitError::Closed) => AccountDirectoryLoadError::Unavailable,
    };
    schedule.submission_failed(key);
    Some(Event::AccountsLoadRejected {
        request_id: key.request_id,
        generation: key.generation,
        reason,
    })
}

#[derive(Default)]
struct MailboxSchedule {
    active: Option<MailboxRequestKey>,
    pending: Option<MailboxQuery>,
}

impl MailboxSchedule {
    fn enqueue(&mut self, query: MailboxQuery) -> MailboxEnqueue {
        if let Some(active) = self.active {
            self.pending = Some(query);
            MailboxEnqueue::Supersede(active)
        } else {
            self.active = Some(query.key());
            MailboxEnqueue::Dispatch(query)
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

enum MailboxEnqueue {
    Dispatch(MailboxQuery),
    Supersede(MailboxRequestKey),
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
    smtp_cancellations: SmtpCancellationTracker,
}

impl CoreRuntime {
    pub(crate) fn shutdown(mut self) -> Result<(), RuntimeError> {
        self.stop_and_join()
    }

    fn stop_and_join(&mut self) -> Result<(), RuntimeError> {
        self.smtp_cancellations.begin_shutdown();
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
    DatabaseShutdown {
        message: Arc<str>,
    },
    CredentialShutdown {
        message: Arc<str>,
    },
    AccountRecovery {
        failure: crate::store::sqlite::FailureKind,
    },
    AccountWorkflowInvariant,
    MutationAccounting,
    MutationDuringShutdown {
        message: Arc<str>,
    },
    ThreadPanicked {
        message: Arc<str>,
    },
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DatabaseClosed => formatter.write_str("SQLite actor stopped unexpectedly"),
            Self::DatabaseShutdown { message } => {
                write!(formatter, "could not stop SQLite actor: {message}")
            }
            Self::CredentialShutdown { message } => {
                write!(formatter, "could not stop credential actor: {message}")
            }
            Self::AccountRecovery { failure } => {
                write!(
                    formatter,
                    "could not recover account lifecycle: {failure:?}"
                )
            }
            Self::AccountWorkflowInvariant => {
                formatter.write_str("account workflow entered an invalid state")
            }
            Self::MutationAccounting => {
                formatter.write_str("SQLite mutation reply accounting became inconsistent")
            }
            Self::MutationDuringShutdown { message } => {
                write!(formatter, "mail mutation failed during shutdown: {message}")
            }
            Self::ThreadPanicked { message } => {
                write!(formatter, "core worker panicked: {message}")
            }
        }
    }
}

impl std::error::Error for RuntimeError {}

impl From<AccountDriverError> for RuntimeError {
    fn from(error: AccountDriverError) -> Self {
        match error {
            AccountDriverError::DatabaseClosed => Self::DatabaseClosed,
            AccountDriverError::Recovery(failure) => Self::AccountRecovery { failure },
            AccountDriverError::WorkflowRejected => Self::AccountWorkflowInvariant,
        }
    }
}

#[derive(Debug)]
pub(crate) enum StartError {
    Database(sqlite::StartError),
    Content(StorageError),
    Runtime(std::io::Error),
}

impl fmt::Display for StartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => write!(formatter, "could not start mail database: {error}"),
            Self::Content(error) => {
                write!(formatter, "could not open private mail storage: {error}")
            }
            Self::Runtime(error) => write!(formatter, "could not start core runtime: {error}"),
        }
    }
}

impl std::error::Error for StartError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::Content(error) => Some(error),
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
    use crate::{
        core::{
            AccountConfigDraft, AccountDirectoryQuery, AccountOperation, AccountOperationSuccess,
            AccountScope, AccountSetupMode, ComposeDraftInput, ComposeOperation, ComposeSuccess,
            FolderScope, Generation, MailboxQuery, MessageId, MessageQuery, PageBoundary, PageSpec,
            RequestId, account::ImapDiagnosticFuture,
        },
        credentials::{
            CredentialDeleteOutcome, CredentialFailure, CredentialFailureKind, CredentialLocator,
            CredentialOperation, CredentialOutcome, Secret,
        },
        network::{
            imap::{ImapDiagnosticFailure, ImapDiagnosticRequest},
            smtp::{
                SmtpDataFence, SmtpSubmissionCancellation, SmtpSubmissionFailure,
                SmtpSubmissionFailureKind, SmtpSubmissionReceipt, SmtpSubmissionRequest,
                SmtpSubmissionStage,
            },
        },
        store::sqlite::{
            AccountAuthKind, AccountConfigInput, AccountConfiguration, AccountDiagnostic,
            AccountLifecycle, AccountWrite, AccountWriteOutcome, DatabaseClient, FailureKind,
            OutboxState, SmtpSecurity,
        },
    };
    use keyring_core::CredentialStore;
    use rusqlite::Connection;
    use std::{
        fs,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        },
        time::Instant,
    };

    fn test_database() -> DatabaseParts {
        sqlite::spawn_in_memory().unwrap()
    }

    fn account_directory_query(request_id: u64, generation: u64) -> AccountDirectoryQuery {
        AccountDirectoryQuery::new(
            RequestId::new(request_id).unwrap(),
            Generation::new(generation),
        )
    }

    fn mailbox_query(request_id: u64, generation: u64) -> MailboxQuery {
        MailboxQuery::new(
            RequestId::new(request_id).unwrap(),
            Generation::new(generation),
            PageSpec::new(
                AccountScope::All,
                FolderScope::Inbox,
                None,
                PageBoundary::First,
                50,
            )
            .unwrap(),
        )
    }

    fn message_query(request_id: u64, generation: u64, message_id: i64) -> MessageQuery {
        MessageQuery::new(
            RequestId::new(request_id).unwrap(),
            Generation::new(generation),
            MessageId::new(message_id).unwrap(),
        )
    }

    fn mutation_request(request_id: u64, generation: u64, message_id: i64) -> MutationRequest {
        MutationRequest::new(
            RequestId::new(request_id).unwrap(),
            Generation::new(generation),
            crate::core::MessageMutation::set_unread(MessageId::new(message_id).unwrap(), false),
        )
    }

    fn temporary_database_path() -> PathBuf {
        static NEXT_PATH: AtomicU64 = AtomicU64::new(1);
        std::env::temp_dir().join(format!(
            "nivalis-core-{}-{}.db",
            std::process::id(),
            NEXT_PATH.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn remove_database_files(path: &std::path::Path) {
        let _ = fs::remove_file(path);
        let _ = fs::remove_file(format!("{}-wal", path.display()));
        let _ = fs::remove_file(format!("{}-shm", path.display()));
    }

    fn wait_for<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(3), future)
                    .await
                    .expect("test operation timed out")
            })
    }

    fn successful_diagnostic(_: ImapDiagnosticRequest) -> ImapDiagnosticFuture {
        Box::pin(async { Ok(()) })
    }

    static PENDING_DIAGNOSTIC_POLLS: AtomicUsize = AtomicUsize::new(0);
    static PENDING_DIAGNOSTIC_DROPS: AtomicUsize = AtomicUsize::new(0);
    static FENCED_SMTP_CALLS: AtomicUsize = AtomicUsize::new(0);
    static COMPLETED_DATA_FENCES: AtomicUsize = AtomicUsize::new(0);
    static SHUTDOWN_SMTP_CALLS: AtomicUsize = AtomicUsize::new(0);
    static SHUTDOWN_DATA_FENCED: AtomicBool = AtomicBool::new(false);
    static SHUTDOWN_SMTP_DROPPED: AtomicBool = AtomicBool::new(false);
    static STUBBORN_SMTP_READY: AtomicBool = AtomicBool::new(false);
    static STUBBORN_SMTP_DROPPED: AtomicBool = AtomicBool::new(false);
    static CANCEL_SMTP_CALLS: AtomicUsize = AtomicUsize::new(0);
    static CANCEL_AFTER_DATA_READY: AtomicBool = AtomicBool::new(false);

    struct PendingDiagnostic;

    impl Future for PendingDiagnostic {
        type Output = Result<(), ImapDiagnosticFailure>;

        fn poll(self: Pin<&mut Self>, _context: &mut std::task::Context<'_>) -> Poll<Self::Output> {
            PENDING_DIAGNOSTIC_POLLS.fetch_add(1, Ordering::AcqRel);
            Poll::Pending
        }
    }

    impl Drop for PendingDiagnostic {
        fn drop(&mut self) {
            PENDING_DIAGNOSTIC_DROPS.fetch_add(1, Ordering::AcqRel);
        }
    }

    fn pending_diagnostic(_: ImapDiagnosticRequest) -> ImapDiagnosticFuture {
        Box::pin(PendingDiagnostic)
    }

    fn fenced_successful_smtp(
        _request: SmtpSubmissionRequest,
        _cancellation: SmtpSubmissionCancellation,
        data_fence: SmtpDataFence,
    ) -> crate::core::outbox_driver::SmtpSubmitFuture {
        Box::pin(async move {
            FENCED_SMTP_CALLS.fetch_add(1, Ordering::AcqRel);
            data_fence.await.map_err(|_| SmtpSubmissionFailure {
                stage: SmtpSubmissionStage::DataFence,
                kind: SmtpSubmissionFailureKind::LocalState,
            })?;
            COMPLETED_DATA_FENCES.fetch_add(1, Ordering::AcqRel);
            Ok(SmtpSubmissionReceipt {
                response_code: 250,
                wire_byte_count: 1,
            })
        })
    }

    struct PendingSmtpGuard;

    impl Drop for PendingSmtpGuard {
        fn drop(&mut self) {
            SHUTDOWN_SMTP_DROPPED.store(true, Ordering::Release);
        }
    }

    struct StubbornSmtpGuard;

    impl Drop for StubbornSmtpGuard {
        fn drop(&mut self) {
            STUBBORN_SMTP_DROPPED.store(true, Ordering::Release);
        }
    }

    fn pending_smtp_after_fence(
        _request: SmtpSubmissionRequest,
        cancellation: SmtpSubmissionCancellation,
        data_fence: SmtpDataFence,
    ) -> crate::core::outbox_driver::SmtpSubmitFuture {
        SHUTDOWN_SMTP_CALLS.fetch_add(1, Ordering::AcqRel);
        Box::pin(async move {
            data_fence.await.map_err(|_| SmtpSubmissionFailure {
                stage: SmtpSubmissionStage::DataFence,
                kind: SmtpSubmissionFailureKind::LocalState,
            })?;
            let _drop_guard = PendingSmtpGuard;
            SHUTDOWN_DATA_FENCED.store(true, Ordering::Release);
            cancellation.cancelled().await;
            Err(SmtpSubmissionFailure {
                stage: SmtpSubmissionStage::Body,
                kind: SmtpSubmissionFailureKind::Uncertain,
            })
        })
    }

    fn stubborn_smtp_after_fence(
        _request: SmtpSubmissionRequest,
        _cancellation: SmtpSubmissionCancellation,
        data_fence: SmtpDataFence,
    ) -> crate::core::outbox_driver::SmtpSubmitFuture {
        Box::pin(async move {
            data_fence.await.map_err(|_| SmtpSubmissionFailure {
                stage: SmtpSubmissionStage::DataFence,
                kind: SmtpSubmissionFailureKind::LocalState,
            })?;
            let _drop_guard = StubbornSmtpGuard;
            STUBBORN_SMTP_READY.store(true, Ordering::Release);
            std::future::pending().await
        })
    }

    fn unexpected_smtp_after_restart(
        _request: SmtpSubmissionRequest,
        _cancellation: SmtpSubmissionCancellation,
        _data_fence: SmtpDataFence,
    ) -> crate::core::outbox_driver::SmtpSubmitFuture {
        SHUTDOWN_SMTP_CALLS.fetch_add(1, Ordering::AcqRel);
        Box::pin(async {
            Err(SmtpSubmissionFailure {
                stage: SmtpSubmissionStage::Connect,
                kind: SmtpSubmissionFailureKind::Retryable,
            })
        })
    }

    fn cancellable_smtp_after_fence(
        _request: SmtpSubmissionRequest,
        cancellation: SmtpSubmissionCancellation,
        data_fence: SmtpDataFence,
    ) -> crate::core::outbox_driver::SmtpSubmitFuture {
        CANCEL_SMTP_CALLS.fetch_add(1, Ordering::AcqRel);
        Box::pin(async move {
            data_fence.await.map_err(|_| SmtpSubmissionFailure {
                stage: SmtpSubmissionStage::DataFence,
                kind: SmtpSubmissionFailureKind::LocalState,
            })?;
            CANCEL_AFTER_DATA_READY.store(true, Ordering::Release);
            cancellation.cancelled().await;
            Err(SmtpSubmissionFailure {
                stage: SmtpSubmissionStage::Body,
                kind: SmtpSubmissionFailureKind::Uncertain,
            })
        })
    }

    fn create_outbound_account(
        database: &DatabaseClient,
        credential_key: &str,
    ) -> AccountConfiguration {
        let input = AccountConfigInput::new_with_smtp(
            credential_key,
            "Alice",
            "alice@example.test",
            AccountAuthKind::AppPassword,
            "alice@example.test",
            "imap.example.test",
            993,
            "smtp.example.test",
            465,
            SmtpSecurity::ImplicitTls,
            true,
            0x335577,
        )
        .unwrap();
        let outcome = wait_for(
            database
                .try_write_account(Box::new(AccountWrite::Create(input)))
                .unwrap(),
        )
        .unwrap()
        .unwrap();
        let AccountWriteOutcome::Saved(configuration) = outcome else {
            panic!("account creation must return configuration")
        };
        configuration
    }

    fn diagnostic_fixture(
        diagnostic_probe: ImapDiagnosticProbe,
    ) -> (
        CoreHandle,
        CoreRuntime,
        DatabaseClient,
        AccountConfiguration,
    ) {
        const KEY: &str = "0123456789abcdef0123456789abcdef";
        let database = test_database();
        let observer = database.0.clone();
        let created = wait_for(
            observer
                .try_write_account(Box::new(AccountWrite::Create(
                    AccountConfigInput::new(
                        KEY,
                        "Diagnostic",
                        "diagnostic@example.test",
                        AccountAuthKind::AppPassword,
                        "diagnostic@example.test",
                        "imap.example.test",
                        993,
                        0x335577,
                    )
                    .unwrap(),
                )))
                .unwrap(),
        )
        .unwrap()
        .unwrap();
        let AccountWriteOutcome::Saved(configuration) = created else {
            panic!("account creation must return configuration")
        };
        let store: Arc<CredentialStore> =
            keyring_core::mock::Store::new().expect("create shared credential store");
        let credential_parts = credentials::spawn_with_test_factory(move || Ok(store.clone()));
        let credential_client = credential_parts.0.clone();
        let stored = wait_for(
            credential_client
                .try_submit(CredentialOperation::Store {
                    locator: CredentialLocator::parse(KEY).unwrap(),
                    secret: Secret::new(b"diagnostic-secret".to_vec()).unwrap(),
                })
                .unwrap(),
        )
        .unwrap()
        .unwrap();
        assert!(matches!(stored, CredentialOutcome::Stored));
        let (core, _events, runtime) = spawn_with_components_and_probe(
            EVENT_CAPACITY,
            database,
            credential_parts,
            diagnostic_probe,
        )
        .unwrap();
        (core, runtime, observer, configuration)
    }

    fn seed_file_database(path: &std::path::Path) {
        let (client, replies, database_runtime, _info) = sqlite::spawn(path.to_owned()).unwrap();
        drop(client);
        drop(replies);
        database_runtime.shutdown().unwrap();
        let connection = Connection::open(path).unwrap();
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
                "INSERT INTO messages (id, account_id, remote_key, received_at_ms)
                 VALUES (1, 1, 'message', 0)",
                [],
            )
            .unwrap();
        sqlite::rebuild_account_stats_for_test(&connection, 1).unwrap();
    }

    #[test]
    fn startup_file_janitor_drains_more_than_one_bounded_batch() {
        let path = temporary_database_path();
        let content_root = path.with_extension("janitor-content");
        remove_database_files(&path);
        let _ = fs::remove_dir_all(&content_root);

        let (database, replies, database_runtime, _info) = sqlite::spawn(path.clone()).unwrap();
        drop(database);
        drop(replies);
        database_runtime.shutdown().unwrap();
        let connection = Connection::open(&path).unwrap();
        for index in 0..33_i64 {
            connection
                .execute(
                    "INSERT INTO file_gc (file_key, queued_at_ms) VALUES (?1, ?2)",
                    rusqlite::params![format!("body/{index:032x}.txt"), index],
                )
                .unwrap();
        }
        drop(connection);

        let database = sqlite::spawn(path.clone()).unwrap();
        let (core, _events, runtime) = spawn_with_sync_components(
            EVENT_CAPACITY,
            database,
            credentials::spawn(),
            successful_diagnostic,
            production_imap_inbox_fetch,
            fenced_successful_smtp,
            Some(content_root.clone()),
        )
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let pending = Connection::open(&path)
                .unwrap()
                .query_row("SELECT count(*) FROM file_gc", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap();
            if pending == 0 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "startup janitor left {pending} durable file-GC rows"
            );
            thread::sleep(Duration::from_millis(5));
        }

        runtime.shutdown().unwrap();
        drop(core);
        remove_database_files(&path);
        fs::remove_dir_all(content_root).unwrap();
    }

    #[test]
    fn compose_restart_queue_and_fenced_delivery_form_a_vertical_slice() {
        const CREDENTIAL_KEY: &str = "89abcdef0123456789abcdef01234567";

        FENCED_SMTP_CALLS.store(0, Ordering::Release);
        COMPLETED_DATA_FENCES.store(0, Ordering::Release);
        let path = temporary_database_path();
        let content_root = path.with_extension("content");
        remove_database_files(&path);
        let _ = fs::remove_dir_all(&content_root);

        let store: Arc<CredentialStore> =
            keyring_core::mock::Store::new().expect("create shared credential store");
        let first_store = Arc::clone(&store);
        let credential_parts =
            credentials::spawn_with_test_factory(move || Ok(Arc::clone(&first_store)));
        let credential_client = credential_parts.0.clone();
        let database = sqlite::spawn(path.clone()).unwrap();
        let account = create_outbound_account(&database.0, CREDENTIAL_KEY);
        let stored = wait_for(
            credential_client
                .try_submit(CredentialOperation::Store {
                    locator: CredentialLocator::parse(CREDENTIAL_KEY).unwrap(),
                    secret: Secret::new(b"outbound-secret".to_vec()).unwrap(),
                })
                .unwrap(),
        )
        .unwrap()
        .unwrap();
        assert!(matches!(stored, CredentialOutcome::Stored));

        let (core, _events, runtime) = spawn_with_sync_components(
            EVENT_CAPACITY,
            database,
            credential_parts,
            successful_diagnostic,
            production_imap_inbox_fetch,
            fenced_successful_smtp,
            Some(content_root.clone()),
        )
        .unwrap();
        let saved = wait_for(
            core.try_compose_operation(ComposeOperation::SaveAndClose(
                ComposeDraftInput::new(
                    account.account_id,
                    account.generation,
                    None,
                    "bob@example.test",
                    "Restart-safe subject",
                    "Body persisted before restart.",
                )
                .unwrap(),
            ))
            .unwrap(),
        )
        .unwrap()
        .unwrap();
        let ComposeSuccess::Saved(saved_identity) = saved else {
            panic!("compose save must persist a draft")
        };
        runtime.shutdown().unwrap();
        drop(core);

        let restarted_store = Arc::clone(&store);
        let credential_parts =
            credentials::spawn_with_test_factory(move || Ok(Arc::clone(&restarted_store)));
        let database = sqlite::spawn(path.clone()).unwrap();
        let (core, mut events, runtime) = spawn_with_sync_components(
            EVENT_CAPACITY,
            database,
            credential_parts,
            successful_diagnostic,
            production_imap_inbox_fetch,
            fenced_successful_smtp,
            Some(content_root.clone()),
        )
        .unwrap();
        let loaded = wait_for(
            core.try_compose_operation(ComposeOperation::LoadLatest {
                account_id: account.account_id,
                expected_generation: account.generation,
            })
            .unwrap(),
        )
        .unwrap()
        .unwrap();
        let ComposeSuccess::Loaded(Some(loaded)) = loaded else {
            panic!("restart must load the saved draft")
        };
        assert_eq!(loaded.identity, saved_identity);
        assert_eq!(loaded.to.as_ref(), "bob@example.test");
        assert_eq!(loaded.subject.as_ref(), "Restart-safe subject");
        assert_eq!(loaded.body.as_ref(), "Body persisted before restart.");

        let queued = wait_for(
            core.try_compose_operation(ComposeOperation::Queue(
                ComposeDraftInput::new(
                    account.account_id,
                    account.generation,
                    Some(loaded.identity),
                    &loaded.to,
                    &loaded.subject,
                    &loaded.body,
                )
                .unwrap(),
            ))
            .unwrap(),
        )
        .unwrap()
        .unwrap();
        let ComposeSuccess::Queued { draft, .. } = queued else {
            panic!("compose queue must commit a ready outbox item")
        };

        wait_for(async {
            loop {
                match events.recv().await {
                    Some(Event::OutboxStatus(OutboxStatus::StateChanged {
                        message_id,
                        state: OutboxState::Delivered,
                        ..
                    })) if message_id == draft.message_id => break,
                    Some(Event::OutboxStatus(OutboxStatus::Fault { kind, .. })) => {
                        panic!("outbox driver faulted before delivery: {kind:?}")
                    }
                    Some(_) => {}
                    None => panic!("core event channel closed before delivery"),
                }
            }
        });
        assert_eq!(FENCED_SMTP_CALLS.load(Ordering::Acquire), 1);
        assert_eq!(COMPLETED_DATA_FENCES.load(Ordering::Acquire), 1);

        let sent_request = RequestId::new(91).unwrap();
        core.try_query_mailbox(MailboxQuery::new(
            sent_request,
            Generation::new(1),
            PageSpec::new(
                AccountScope::Account(account.account_id),
                FolderScope::Sent,
                None,
                PageBoundary::First,
                50,
            )
            .unwrap(),
        ))
        .unwrap();
        let sent = wait_for(async {
            loop {
                match events.recv().await {
                    Some(Event::MailboxLoaded(reply)) if reply.request_id == sent_request => {
                        break reply
                            .result
                            .expect("delivered mailbox stats must remain queryable");
                    }
                    Some(Event::MailboxLoadRejected {
                        request_id, reason, ..
                    }) if request_id == sent_request => {
                        panic!("sent mailbox query was rejected: {reason:?}")
                    }
                    Some(_) => {}
                    None => panic!("core event channel closed before Sent projection"),
                }
            }
        });
        assert_eq!(sent.rows.len(), 1);
        assert_eq!(sent.rows[0].id, draft.message_id);

        let removed = wait_for(
            core.try_account_operation(AccountOperation::Remove {
                request_id: RequestId::new(92).unwrap(),
                account_id: account.account_id,
                expected_generation: account.generation,
            })
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            removed.result,
            Ok(AccountOperationSuccess::Removed {
                account_id: account.account_id,
            })
        );

        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let connection = Connection::open(&path).unwrap();
            let pending_gc = connection
                .query_row("SELECT count(*) FROM file_gc", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap();
            drop(connection);
            let body_files = fs::read_dir(content_root.join("body"))
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
                .count();
            if pending_gc == 0 && body_files == 0 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "account removal left {pending_gc} GC rows and {body_files} body files"
            );
            thread::sleep(Duration::from_millis(5));
        }

        runtime.shutdown().unwrap();
        drop(core);
        let connection = Connection::open(&path).unwrap();
        let pending_gc = connection
            .query_row("SELECT count(*) FROM file_gc", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap();
        assert_eq!(pending_gc, 0);
        drop(connection);
        let body_files = fs::read_dir(content_root.join("body"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
            .count();
        assert_eq!(body_files, 0);
        remove_database_files(&path);
        fs::remove_dir_all(content_root).unwrap();
    }

    #[test]
    fn shutdown_finishes_a_compose_save_already_accepted_by_core() {
        const CREDENTIAL_KEY: &str = "fedcba9876543210fedcba9876543210";

        let path = temporary_database_path();
        let content_root = path.with_extension("shutdown-content");
        remove_database_files(&path);
        let _ = fs::remove_dir_all(&content_root);
        let database = sqlite::spawn(path.clone()).unwrap();
        let account = create_outbound_account(&database.0, CREDENTIAL_KEY);
        let (core, _events, runtime) = spawn_with_sync_components(
            EVENT_CAPACITY,
            database,
            credentials::spawn(),
            successful_diagnostic,
            production_imap_inbox_fetch,
            fenced_successful_smtp,
            Some(content_root.clone()),
        )
        .unwrap();
        let initial = wait_for(
            core.try_compose_operation(ComposeOperation::SaveAndClose(
                ComposeDraftInput::new(
                    account.account_id,
                    account.generation,
                    None,
                    "bob@example.test",
                    "Initial subject",
                    "This body must be replaced during shutdown.",
                )
                .unwrap(),
            ))
            .unwrap(),
        )
        .unwrap()
        .unwrap();
        let ComposeSuccess::Saved(initial) = initial else {
            panic!("initial compose save must persist a draft")
        };
        let response = core
            .try_compose_operation(ComposeOperation::SaveAndClose(
                ComposeDraftInput::new(
                    account.account_id,
                    account.generation,
                    Some(initial),
                    "bob@example.test",
                    "Shutdown-safe subject",
                    "Accepted body must reach durable storage.",
                )
                .unwrap(),
            ))
            .unwrap();

        runtime.shutdown().unwrap();
        let saved = wait_for(response).unwrap().unwrap();
        assert!(matches!(saved, ComposeSuccess::Saved(_)));
        drop(core);

        let connection = Connection::open(&path).unwrap();
        let (subject, body_key) = connection
            .query_row(
                "SELECT message.subject, content.body_file_key
                   FROM local_drafts AS draft
                   JOIN messages AS message ON message.id = draft.message_id
                   JOIN message_content AS content ON content.message_id = message.id",
                [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .unwrap();
        assert_eq!(subject, "Shutdown-safe subject");
        assert!(content_root.join(body_key).is_file());
        let pending_gc = connection
            .query_row("SELECT count(*) FROM file_gc", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap();
        assert_eq!(pending_gc, 0);
        drop(connection);
        let body_files = fs::read_dir(content_root.join("body"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
            .count();
        assert_eq!(body_files, 1);
        remove_database_files(&path);
        fs::remove_dir_all(content_root).unwrap();
    }

    #[test]
    fn shutdown_after_data_fence_persists_uncertain_without_resubmitting() {
        const CREDENTIAL_KEY: &str = "76543210fedcba9876543210fedcba98";

        SHUTDOWN_SMTP_CALLS.store(0, Ordering::Release);
        SHUTDOWN_DATA_FENCED.store(false, Ordering::Release);
        SHUTDOWN_SMTP_DROPPED.store(false, Ordering::Release);
        let path = temporary_database_path();
        let content_root = path.with_extension("content");
        remove_database_files(&path);
        let _ = fs::remove_dir_all(&content_root);

        let store: Arc<CredentialStore> =
            keyring_core::mock::Store::new().expect("create shared credential store");
        let first_store = Arc::clone(&store);
        let credential_parts =
            credentials::spawn_with_test_factory(move || Ok(Arc::clone(&first_store)));
        let credential_client = credential_parts.0.clone();
        let database = sqlite::spawn(path.clone()).unwrap();
        let account = create_outbound_account(&database.0, CREDENTIAL_KEY);
        let stored = wait_for(
            credential_client
                .try_submit(CredentialOperation::Store {
                    locator: CredentialLocator::parse(CREDENTIAL_KEY).unwrap(),
                    secret: Secret::new(b"shutdown-secret".to_vec()).unwrap(),
                })
                .unwrap(),
        )
        .unwrap()
        .unwrap();
        assert!(matches!(stored, CredentialOutcome::Stored));

        let (core, _events, runtime) = spawn_with_sync_components(
            EVENT_CAPACITY,
            database,
            credential_parts,
            successful_diagnostic,
            production_imap_inbox_fetch,
            pending_smtp_after_fence,
            Some(content_root.clone()),
        )
        .unwrap();
        let queued = wait_for(
            core.try_compose_operation(ComposeOperation::Queue(
                ComposeDraftInput::new(
                    account.account_id,
                    account.generation,
                    None,
                    "bob@example.test",
                    "Shutdown subject",
                    "The DATA fence must survive shutdown.",
                )
                .unwrap(),
            ))
            .unwrap(),
        )
        .unwrap()
        .unwrap();
        let ComposeSuccess::Queued { draft, .. } = queued else {
            panic!("compose queue must create the pending submission")
        };
        let deadline = Instant::now() + Duration::from_secs(3);
        while !SHUTDOWN_DATA_FENCED.load(Ordering::Acquire) {
            assert!(
                Instant::now() < deadline,
                "SMTP DATA fence did not complete"
            );
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(SHUTDOWN_SMTP_CALLS.load(Ordering::Acquire), 1);

        let shutdown_started = Instant::now();
        runtime.shutdown().unwrap();
        assert!(shutdown_started.elapsed() < Duration::from_secs(1));
        assert!(SHUTDOWN_SMTP_DROPPED.load(Ordering::Acquire));
        drop(core);

        let mut connection = Connection::open(&path).unwrap();
        let (state, delivery_started, lease_cleared): (String, bool, bool) = connection
            .query_row(
                "SELECT state, delivery_started <> 0, lease_expires_at_ms IS NULL
                 FROM outbox WHERE message_id = ?1",
                [draft.message_id.get()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(state, "uncertain");
        assert!(delivery_started);
        assert!(lease_cleared);
        assert!(matches!(
            sqlite::claim_next_outbox(&mut connection, 2_000_000_000_000).unwrap(),
            crate::store::sqlite::OutboxClaimOutcome::Idle { .. }
        ));
        drop(connection);

        let restart_store = Arc::clone(&store);
        let credential_parts =
            credentials::spawn_with_test_factory(move || Ok(Arc::clone(&restart_store)));
        let database = sqlite::spawn(path.clone()).unwrap();
        let (core, mut events, runtime) = spawn_with_sync_components(
            EVENT_CAPACITY,
            database,
            credential_parts,
            successful_diagnostic,
            production_imap_inbox_fetch,
            unexpected_smtp_after_restart,
            Some(content_root.clone()),
        )
        .unwrap();
        core.try_query_account_directory(account_directory_query(901, 1))
            .unwrap();
        wait_for(async {
            loop {
                match events.recv().await {
                    Some(Event::AccountsLoaded(_)) => break,
                    Some(Event::OutboxStatus(OutboxStatus::Fault { kind, .. })) => {
                        panic!("outbox restart faulted: {kind:?}")
                    }
                    Some(_) => {}
                    None => panic!("core event channel closed before restart synchronization"),
                }
            }
        });
        assert_eq!(SHUTDOWN_SMTP_CALLS.load(Ordering::Acquire), 1);
        runtime.shutdown().unwrap();
        drop(core);

        let state: String = Connection::open(&path)
            .unwrap()
            .query_row(
                "SELECT state FROM outbox WHERE message_id = ?1",
                [draft.message_id.get()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(state, "uncertain");
        remove_database_files(&path);
        fs::remove_dir_all(content_root).unwrap();
    }

    #[test]
    fn shutdown_aborts_uncooperative_smtp_after_bounded_grace() {
        const CREDENTIAL_KEY: &str = "11112222333344445555666677778888";

        STUBBORN_SMTP_READY.store(false, Ordering::Release);
        STUBBORN_SMTP_DROPPED.store(false, Ordering::Release);
        let path = temporary_database_path();
        let content_root = path.with_extension("stubborn-content");
        remove_database_files(&path);
        let _ = fs::remove_dir_all(&content_root);

        let store: Arc<CredentialStore> =
            keyring_core::mock::Store::new().expect("create shared credential store");
        let credential_parts = credentials::spawn_with_test_factory(move || Ok(Arc::clone(&store)));
        let credential_client = credential_parts.0.clone();
        let database = sqlite::spawn(path.clone()).unwrap();
        let account = create_outbound_account(&database.0, CREDENTIAL_KEY);
        let stored = wait_for(
            credential_client
                .try_submit(CredentialOperation::Store {
                    locator: CredentialLocator::parse(CREDENTIAL_KEY).unwrap(),
                    secret: Secret::new(b"stubborn-secret".to_vec()).unwrap(),
                })
                .unwrap(),
        )
        .unwrap()
        .unwrap();
        assert!(matches!(stored, CredentialOutcome::Stored));

        let (core, _events, runtime) = spawn_with_sync_components(
            EVENT_CAPACITY,
            database,
            credential_parts,
            successful_diagnostic,
            production_imap_inbox_fetch,
            stubborn_smtp_after_fence,
            Some(content_root.clone()),
        )
        .unwrap();
        let queued = wait_for(
            core.try_compose_operation(ComposeOperation::Queue(
                ComposeDraftInput::new(
                    account.account_id,
                    account.generation,
                    None,
                    "bob@example.test",
                    "Bounded shutdown",
                    "A non-cooperative transport must not block shutdown.",
                )
                .unwrap(),
            ))
            .unwrap(),
        )
        .unwrap()
        .unwrap();
        assert!(matches!(queued, ComposeSuccess::Queued { .. }));
        let deadline = Instant::now() + Duration::from_secs(3);
        while !STUBBORN_SMTP_READY.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline, "SMTP fence did not complete");
            thread::sleep(Duration::from_millis(1));
        }

        let shutdown_started = Instant::now();
        runtime.shutdown().unwrap();
        let shutdown_elapsed = shutdown_started.elapsed();
        assert!(shutdown_elapsed >= OUTBOX_SHUTDOWN_GRACE);
        assert!(shutdown_elapsed < OUTBOX_SHUTDOWN_GRACE + Duration::from_secs(1));
        assert!(STUBBORN_SMTP_DROPPED.load(Ordering::Acquire));
        drop(core);

        remove_database_files(&path);
        fs::remove_dir_all(content_root).unwrap();
    }

    #[test]
    fn current_attempt_cancel_is_message_fenced_and_uncertain_after_data() {
        const CREDENTIAL_KEY: &str = "abcdefabcdefabcdefabcdefabcdefab";

        CANCEL_SMTP_CALLS.store(0, Ordering::Release);
        CANCEL_AFTER_DATA_READY.store(false, Ordering::Release);
        let path = temporary_database_path();
        let content_root = path.with_extension("cancel-content");
        remove_database_files(&path);
        let _ = fs::remove_dir_all(&content_root);

        let store: Arc<CredentialStore> =
            keyring_core::mock::Store::new().expect("create shared credential store");
        let credential_parts = credentials::spawn_with_test_factory(move || Ok(Arc::clone(&store)));
        let credential_client = credential_parts.0.clone();
        let database = sqlite::spawn(path.clone()).unwrap();
        let account = create_outbound_account(&database.0, CREDENTIAL_KEY);
        let stored = wait_for(
            credential_client
                .try_submit(CredentialOperation::Store {
                    locator: CredentialLocator::parse(CREDENTIAL_KEY).unwrap(),
                    secret: Secret::new(b"cancel-secret".to_vec()).unwrap(),
                })
                .unwrap(),
        )
        .unwrap()
        .unwrap();
        assert!(matches!(stored, CredentialOutcome::Stored));

        let (core, mut events, runtime) = spawn_with_sync_components(
            EVENT_CAPACITY,
            database,
            credential_parts,
            successful_diagnostic,
            production_imap_inbox_fetch,
            cancellable_smtp_after_fence,
            Some(content_root.clone()),
        )
        .unwrap();
        let queued = wait_for(
            core.try_compose_operation(ComposeOperation::Queue(
                ComposeDraftInput::new(
                    account.account_id,
                    account.generation,
                    None,
                    "bob@example.test",
                    "Cancel after DATA",
                    "Acceptance must remain conservative.",
                )
                .unwrap(),
            ))
            .unwrap(),
        )
        .unwrap()
        .unwrap();
        let ComposeSuccess::Queued { draft, .. } = queued else {
            panic!("compose queue must create a pending submission")
        };
        let deadline = Instant::now() + Duration::from_secs(3);
        while !CANCEL_AFTER_DATA_READY.load(Ordering::Acquire) {
            assert!(
                Instant::now() < deadline,
                "SMTP DATA fence did not complete"
            );
            thread::sleep(Duration::from_millis(1));
        }

        let wrong_message_id = MessageId::new(draft.message_id.get() + 1).unwrap();
        assert_eq!(
            core.cancel_outbox_attempt(wrong_message_id),
            crate::core::message::OutboxCancelOutcome::NotActive
        );
        assert_eq!(
            core.cancel_outbox_attempt(draft.message_id),
            crate::core::message::OutboxCancelOutcome::Applied
        );
        assert_eq!(
            core.cancel_outbox_attempt(draft.message_id),
            crate::core::message::OutboxCancelOutcome::NotActive
        );

        wait_for(async {
            loop {
                match events.recv().await {
                    Some(Event::OutboxStatus(OutboxStatus::StateChanged {
                        message_id,
                        state: OutboxState::Uncertain,
                        ..
                    })) if message_id == draft.message_id => break,
                    Some(Event::OutboxStatus(OutboxStatus::Fault { kind, .. })) => {
                        panic!("outbox cancellation faulted: {kind:?}")
                    }
                    Some(_) => {}
                    None => panic!("core event channel closed before uncertain cancellation"),
                }
            }
        });
        assert_eq!(CANCEL_SMTP_CALLS.load(Ordering::Acquire), 1);
        assert_eq!(
            Connection::open(&path)
                .unwrap()
                .query_row(
                    "SELECT state FROM outbox WHERE message_id = ?1",
                    [draft.message_id.get()],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "uncertain"
        );

        runtime.shutdown().unwrap();
        drop(core);
        remove_database_files(&path);
        fs::remove_dir_all(content_root).unwrap();
    }

    #[test]
    fn account_diagnostic_records_a_fenced_ready_result() {
        let (core, runtime, observer, configuration) = diagnostic_fixture(successful_diagnostic);
        let reply = wait_for(
            core.try_account_operation(AccountOperation::Diagnose {
                request_id: RequestId::new(46).unwrap(),
                account_id: configuration.account_id,
                expected_generation: configuration.generation,
            })
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            reply.result,
            Ok(AccountOperationSuccess::Diagnosed {
                account_id: configuration.account_id,
                generation: configuration.generation,
            })
        );

        let loaded = wait_for(observer.try_load_account(configuration.account_id).unwrap())
            .unwrap()
            .unwrap();
        let crate::store::sqlite::AccountRecord::Configured(loaded) = loaded else {
            panic!("diagnostic account must remain configured")
        };
        assert!(matches!(
            loaded.diagnostic,
            AccountDiagnostic::Ready { checked_at_ms } if checked_at_ms > 0
        ));
        runtime.shutdown().unwrap();
    }

    #[test]
    fn cancelled_account_diagnostic_drops_probe_without_recording() {
        PENDING_DIAGNOSTIC_POLLS.store(0, Ordering::Release);
        PENDING_DIAGNOSTIC_DROPS.store(0, Ordering::Release);
        let (core, runtime, observer, configuration) = diagnostic_fixture(pending_diagnostic);
        let response = core
            .try_account_operation(AccountOperation::Diagnose {
                request_id: RequestId::new(47).unwrap(),
                account_id: configuration.account_id,
                expected_generation: configuration.generation,
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while PENDING_DIAGNOSTIC_POLLS.load(Ordering::Acquire) == 0 {
            assert!(Instant::now() < deadline, "diagnostic probe did not start");
            thread::sleep(Duration::from_millis(1));
        }
        drop(response);
        let deadline = Instant::now() + Duration::from_secs(3);
        while PENDING_DIAGNOSTIC_DROPS.load(Ordering::Acquire) == 0 {
            assert!(Instant::now() < deadline, "cancelled probe was not dropped");
            thread::sleep(Duration::from_millis(1));
        }

        let retry = wait_for(
            core.try_account_operation(AccountOperation::RetryCredential {
                request_id: RequestId::new(48).unwrap(),
                account_id: configuration.account_id,
                expected_generation: configuration.generation,
                secret: Secret::new(b"replacement-secret".to_vec()).unwrap(),
            })
            .unwrap(),
        )
        .unwrap();
        assert!(matches!(
            retry.result,
            Ok(AccountOperationSuccess::Configured { .. })
        ));
        let loaded = wait_for(observer.try_load_account(configuration.account_id).unwrap())
            .unwrap()
            .unwrap();
        let crate::store::sqlite::AccountRecord::Configured(loaded) = loaded else {
            panic!("cancelled diagnostic account must remain configured")
        };
        assert_eq!(loaded.diagnostic, AccountDiagnostic::Never);
        runtime.shutdown().unwrap();
    }

    #[test]
    fn full_event_queue_applies_backpressure_without_stopping_core() {
        let (core, mut events, runtime) = spawn_with_options(1, test_database()).unwrap();
        core.try_mutate(mutation_request(1, 0, 1)).unwrap();
        core.try_mutate(mutation_request(2, 0, 1)).unwrap();

        for request_id in [1, 2] {
            let Some(Event::MutationFinished(reply)) = events.blocking_recv() else {
                panic!("expected mutation result");
            };
            assert_eq!(reply.request_id, RequestId::new(request_id).unwrap());
        }
        runtime.shutdown().unwrap();
    }

    #[test]
    fn mailbox_query_round_trip_preserves_request_identity() {
        let (core, mut events, runtime) = spawn_for_test().unwrap();
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
    fn restart_recovers_the_remove_after_credential_deletion_crash_window() {
        let path = temporary_database_path();
        remove_database_files(&path);
        let store: Arc<CredentialStore> =
            keyring_core::mock::Store::new().expect("create shared credential store");

        let first_store = store.clone();
        let credential_parts =
            credentials::spawn_with_test_factory(move || Ok(first_store.clone()));
        let credential_probe = credential_parts.0.clone();
        let database = sqlite::spawn(path.clone()).unwrap();
        let (core, _events, runtime) =
            spawn_with_components(EVENT_CAPACITY, database, credential_parts).unwrap();
        let setup = core
            .try_account_operation(AccountOperation::Setup {
                request_id: RequestId::new(40).unwrap(),
                mode: AccountSetupMode::Create,
                draft: AccountConfigDraft::new(
                    "Personal",
                    "user@example.test",
                    "user@example.test",
                    "imap.example.test",
                    993,
                    "smtp.example.test",
                    465,
                    0x336699,
                )
                .unwrap(),
                secret: Secret::new(b"restart-secret".to_vec()).unwrap(),
            })
            .unwrap();
        let setup = wait_for(setup).unwrap();
        assert_eq!(setup.request_id, RequestId::new(40).unwrap());
        let AccountOperationSuccess::Configured {
            account_id,
            generation,
        } = setup.result.unwrap()
        else {
            panic!("setup must configure an account")
        };

        let connection = Connection::open(&path).unwrap();
        let credential_key: String = connection
            .query_row(
                "SELECT credential_key FROM account_connections WHERE account_id = ?1",
                [account_id.get()],
                |row| row.get(0),
            )
            .unwrap();
        drop(connection);
        let locator = CredentialLocator::parse(&credential_key).unwrap();
        let loaded = wait_for(
            credential_probe
                .try_submit(CredentialOperation::Load {
                    locator: locator.clone(),
                })
                .unwrap(),
        )
        .unwrap()
        .unwrap();
        let CredentialOutcome::Loaded(secret) = loaded else {
            panic!("setup must persist the credential")
        };
        assert_eq!(secret.expose(), b"restart-secret");
        runtime.shutdown().unwrap();

        let (database, replies, database_runtime, _) = sqlite::spawn(path.clone()).unwrap();
        let removal = wait_for(
            database
                .try_write_account(Box::new(AccountWrite::BeginRemove {
                    account_id,
                    expected_generation: generation,
                }))
                .unwrap(),
        )
        .unwrap()
        .unwrap();
        let AccountWriteOutcome::RemovalStarted(ticket) = removal else {
            panic!("removal must enter the credential deletion stage")
        };
        drop(database);
        drop(replies);
        database_runtime.shutdown().unwrap();

        let delete_store = store.clone();
        let (delete_client, delete_runtime) =
            credentials::spawn_with_test_factory(move || Ok(delete_store.clone()));
        let deleted = wait_for(
            delete_client
                .try_submit(CredentialOperation::Delete {
                    locator: CredentialLocator::parse(
                        ticket
                            .credential_key
                            .as_deref()
                            .expect("configured account has a credential key"),
                    )
                    .unwrap(),
                })
                .unwrap(),
        )
        .unwrap()
        .unwrap();
        assert!(matches!(
            deleted,
            CredentialOutcome::Deleted(CredentialDeleteOutcome::Deleted)
        ));
        delete_runtime.shutdown().unwrap();

        let connection = Connection::open(&path).unwrap();
        let state: String = connection
            .query_row(
                "SELECT state FROM accounts WHERE id = ?1",
                [account_id.get()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(state, "removing_credentials");
        drop(connection);

        let (opened_tx, opened_rx) = crossbeam_channel::bounded(1);
        let (release_tx, release_rx) = crossbeam_channel::bounded(1);
        let opened = Arc::new(AtomicBool::new(false));
        let recovery_store = store.clone();
        let recovery_opened = opened.clone();
        let credential_parts = credentials::spawn_with_test_factory(move || {
            if !recovery_opened.swap(true, Ordering::AcqRel) {
                opened_tx.send(()).unwrap();
                release_rx
                    .recv_timeout(Duration::from_secs(10))
                    .expect("release blocked credential recovery");
            }
            Ok(recovery_store.clone())
        });
        let database = sqlite::spawn(path.clone()).unwrap();
        let observer = database.0.clone();
        let (core, mut events, runtime) =
            spawn_with_components(EVENT_CAPACITY, database, credential_parts).unwrap();
        opened_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("startup recovery must retry credential deletion");

        core.try_query_mailbox(mailbox_query(41, 0)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while events.is_empty() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        if events.is_empty() {
            let _ = release_tx.send(());
            panic!("mailbox query timed out while credentials were blocked");
        }
        let mailbox = events.blocking_recv();
        release_tx.send(()).unwrap();
        let Some(Event::MailboxLoaded(mailbox)) = mailbox else {
            panic!("mailbox query must complete while credentials are blocked")
        };
        assert_eq!(mailbox.request_id, RequestId::new(41).unwrap());
        assert!(mailbox.result.unwrap().rows.is_empty());

        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match observer.try_load_account(account_id) {
                Ok(receiver) => match wait_for(receiver) {
                    Ok(Err(failure)) if failure.kind == FailureKind::NotFound => break,
                    Ok(Ok(_)) => {}
                    Ok(Err(failure)) => panic!("unexpected recovery failure: {:?}", failure.kind),
                    Err(_) => panic!("database stopped before recovery completed"),
                },
                Err(DatabaseSubmitError::Busy) => {}
                Err(DatabaseSubmitError::Closed) => {
                    panic!("database stopped before recovery completed")
                }
            }
            assert!(
                Instant::now() < deadline,
                "restart recovery did not remove the account"
            );
            thread::sleep(Duration::from_millis(1));
        }

        runtime.shutdown().unwrap();
        drop(observer);
        remove_database_files(&path);
    }

    #[test]
    fn failed_credential_store_is_visible_and_retryable() {
        let store: Arc<CredentialStore> =
            keyring_core::mock::Store::new().expect("create shared credential store");
        let first_open = Arc::new(AtomicBool::new(true));
        let factory_store = store.clone();
        let factory_first_open = first_open.clone();
        let credential_parts = credentials::spawn_with_test_factory(move || {
            if factory_first_open.swap(false, Ordering::AcqRel) {
                Err(CredentialFailure {
                    kind: CredentialFailureKind::Unavailable,
                })
            } else {
                Ok(factory_store.clone())
            }
        });
        let credential_probe = credential_parts.0.clone();
        let database = test_database();
        let observer = database.0.clone();
        let (core, _events, runtime) =
            spawn_with_components(EVENT_CAPACITY, database, credential_parts).unwrap();

        let setup = wait_for(
            core.try_account_operation(AccountOperation::Setup {
                request_id: RequestId::new(42).unwrap(),
                mode: AccountSetupMode::Create,
                draft: AccountConfigDraft::new(
                    "Retry account",
                    "retry@example.test",
                    "retry@example.test",
                    "imap.example.test",
                    993,
                    "smtp.example.test",
                    465,
                    0x335577,
                )
                .unwrap(),
                secret: Secret::new(b"first-secret".to_vec()).unwrap(),
            })
            .unwrap(),
        )
        .unwrap();
        let failure = setup.result.unwrap_err();
        assert_eq!(
            failure.stage,
            crate::core::AccountWorkflowStage::StoreCredential
        );
        assert_eq!(
            failure.kind,
            crate::core::AccountWorkflowFailureKind::Credential(CredentialFailureKind::Unavailable)
        );
        let account_id = failure.account_id.expect("configuration must be durable");
        let generation = failure
            .generation
            .expect("retry requires a generation fence");

        let retried = wait_for(
            core.try_account_operation(AccountOperation::RetryCredential {
                request_id: RequestId::new(43).unwrap(),
                account_id,
                expected_generation: generation,
                secret: Secret::new(b"retry-secret".to_vec()).unwrap(),
            })
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            retried.result,
            Ok(AccountOperationSuccess::Configured {
                account_id,
                generation,
            })
        );

        let configuration = wait_for(observer.try_load_account(account_id).unwrap())
            .unwrap()
            .unwrap();
        let crate::store::sqlite::AccountRecord::Configured(configuration) = configuration else {
            panic!("retry must keep the configured account")
        };
        let loaded = wait_for(
            credential_probe
                .try_submit(CredentialOperation::Load {
                    locator: CredentialLocator::parse(&configuration.credential_key).unwrap(),
                })
                .unwrap(),
        )
        .unwrap()
        .unwrap();
        let CredentialOutcome::Loaded(secret) = loaded else {
            panic!("retry must store the credential")
        };
        assert_eq!(secret.expose(), b"retry-secret");

        runtime.shutdown().unwrap();
    }

    #[test]
    fn remove_resumes_in_process_after_a_transient_credential_failure() {
        let database = test_database();
        let observer = database.0.clone();
        let created = wait_for(
            observer
                .try_write_account(Box::new(AccountWrite::Create(
                    AccountConfigInput::new(
                        "0123456789abcdef0123456789abcdef",
                        "Removal retry",
                        "remove@example.test",
                        AccountAuthKind::AppPassword,
                        "remove@example.test",
                        "imap.example.test",
                        993,
                        0x446688,
                    )
                    .unwrap(),
                )))
                .unwrap(),
        )
        .unwrap()
        .unwrap();
        let AccountWriteOutcome::Saved(created) = created else {
            panic!("account creation must return configuration")
        };
        let store: Arc<CredentialStore> =
            keyring_core::mock::Store::new().expect("create shared credential store");
        let first_open = Arc::new(AtomicBool::new(true));
        let factory_store = store.clone();
        let factory_first_open = first_open.clone();
        let credential_parts = credentials::spawn_with_test_factory(move || {
            if factory_first_open.swap(false, Ordering::AcqRel) {
                Err(CredentialFailure {
                    kind: CredentialFailureKind::Unavailable,
                })
            } else {
                Ok(factory_store.clone())
            }
        });
        let (core, _events, runtime) =
            spawn_with_components(EVENT_CAPACITY, database, credential_parts).unwrap();

        let first = wait_for(
            core.try_account_operation(AccountOperation::Remove {
                request_id: RequestId::new(44).unwrap(),
                account_id: created.account_id,
                expected_generation: created.generation,
            })
            .unwrap(),
        )
        .unwrap();
        let failure = first.result.unwrap_err();
        assert_eq!(
            failure.stage,
            crate::core::AccountWorkflowStage::DeleteCredential
        );
        assert_eq!(
            failure.kind,
            crate::core::AccountWorkflowFailureKind::Credential(CredentialFailureKind::Unavailable)
        );
        let removal_generation = failure
            .generation
            .expect("failed delete must expose the durable removal fence");

        let retried = wait_for(
            core.try_account_operation(AccountOperation::Remove {
                request_id: RequestId::new(45).unwrap(),
                account_id: created.account_id,
                expected_generation: removal_generation,
            })
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            retried.result,
            Ok(AccountOperationSuccess::Removed {
                account_id: created.account_id,
            })
        );
        let missing = wait_for(observer.try_load_account(created.account_id).unwrap())
            .unwrap()
            .unwrap_err();
        assert_eq!(missing.kind, FailureKind::NotFound);

        runtime.shutdown().unwrap();
    }

    #[test]
    fn restart_purges_removing_cache_without_opening_credentials() {
        let path = temporary_database_path();
        remove_database_files(&path);
        let (database, replies, database_runtime, _) = sqlite::spawn(path.clone()).unwrap();
        let created = wait_for(
            database
                .try_write_account(Box::new(AccountWrite::Create(
                    AccountConfigInput::new(
                        "fedcba9876543210fedcba9876543210",
                        "Cache recovery",
                        "cache@example.test",
                        AccountAuthKind::AppPassword,
                        "cache@example.test",
                        "imap.example.test",
                        993,
                        0x557799,
                    )
                    .unwrap(),
                )))
                .unwrap(),
        )
        .unwrap()
        .unwrap();
        let AccountWriteOutcome::Saved(created) = created else {
            panic!("account creation must return configuration")
        };
        let removal = wait_for(
            database
                .try_write_account(Box::new(AccountWrite::BeginRemove {
                    account_id: created.account_id,
                    expected_generation: created.generation,
                }))
                .unwrap(),
        )
        .unwrap()
        .unwrap();
        let AccountWriteOutcome::RemovalStarted(removal) = removal else {
            panic!("removal must start")
        };
        let confirmed = wait_for(
            database
                .try_write_account(Box::new(AccountWrite::ConfirmCredentialsRemoved {
                    account_id: removal.account_id,
                    expected_generation: removal.generation,
                }))
                .unwrap(),
        )
        .unwrap()
        .unwrap();
        let AccountWriteOutcome::Saved(confirmed) = confirmed else {
            panic!("credential confirmation must save the cache-removal state")
        };
        assert_eq!(confirmed.lifecycle, AccountLifecycle::RemovingCache);
        drop(database);
        drop(replies);
        database_runtime.shutdown().unwrap();

        let credential_parts = credentials::spawn_with_test_factory(|| {
            panic!("cache-only recovery must not open credential storage")
        });
        let database = sqlite::spawn(path.clone()).unwrap();
        let observer = database.0.clone();
        let (_core, _events, runtime) =
            spawn_with_components(EVENT_CAPACITY, database, credential_parts).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match observer.try_load_account(created.account_id) {
                Ok(receiver) => match wait_for(receiver) {
                    Ok(Err(failure)) if failure.kind == FailureKind::NotFound => break,
                    Ok(Ok(_)) => {}
                    Ok(Err(failure)) => panic!("unexpected recovery failure: {:?}", failure.kind),
                    Err(_) => panic!("database stopped before cache recovery completed"),
                },
                Err(DatabaseSubmitError::Busy) => {}
                Err(DatabaseSubmitError::Closed) => {
                    panic!("database stopped before cache recovery completed")
                }
            }
            assert!(
                Instant::now() < deadline,
                "restart recovery did not purge removing_cache"
            );
            thread::sleep(Duration::from_millis(1));
        }

        runtime.shutdown().unwrap();
        drop(observer);
        remove_database_files(&path);
    }

    #[test]
    fn superseded_mailbox_work_publishes_only_the_latest_query() {
        let (database, replies, database_runtime, info) = test_database();
        let control = database.clone();
        let (started_tx, started_rx) = crossbeam_channel::bounded(1);
        let (release_tx, release_rx) = crossbeam_channel::bounded(1);
        control.gate_next_mailbox_query(started_tx, release_rx);
        let (core, mut events, runtime) =
            spawn_with_options(EVENT_CAPACITY, (database, replies, database_runtime, info))
                .unwrap();

        core.try_query_mailbox(mailbox_query(1, 1)).unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        core.try_query_mailbox(mailbox_query(2, 2)).unwrap();
        core.try_query_mailbox(mailbox_query(3, 3)).unwrap();
        let (barrier_tx, barrier_rx) = std::sync::mpsc::channel();
        core.try_barrier(barrier_tx).unwrap();
        barrier_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        release_tx.send(()).unwrap();

        let Some(Event::MailboxLoaded(reply)) = events.blocking_recv() else {
            panic!("expected the latest mailbox page");
        };
        assert_eq!(reply.request_id, RequestId::new(3).unwrap());
        assert_eq!(reply.generation, Generation::new(3));
        reply.result.unwrap();
        assert!(events.is_empty());
        runtime.shutdown().unwrap();
    }

    #[test]
    fn account_directory_round_trip_preserves_request_identity() {
        let (core, mut events, runtime) = spawn_for_test().unwrap();
        let query = account_directory_query(6, 2);

        core.try_query_account_directory(query).unwrap();

        let Some(Event::AccountsLoaded(reply)) = events.blocking_recv() else {
            panic!("expected account directory");
        };
        assert_eq!(reply.request_id, RequestId::new(6).unwrap());
        assert_eq!(reply.generation, Generation::new(2));
        assert!(reply.result.unwrap().rows.is_empty());
        runtime.shutdown().unwrap();
    }

    #[test]
    fn message_query_round_trip_preserves_request_identity() {
        let (core, mut events, runtime) = spawn_for_test().unwrap();

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
    fn mutation_round_trip_preserves_request_identity_and_failure() {
        let (core, mut events, runtime) = spawn_for_test().unwrap();

        core.try_mutate(mutation_request(9, 5, 1)).unwrap();

        let Some(Event::MutationFinished(reply)) = events.blocking_recv() else {
            panic!("expected mutation result");
        };
        assert_eq!(reply.request_id, RequestId::new(9).unwrap());
        assert_eq!(reply.generation, Generation::new(5));
        assert!(reply.result.is_err());
        runtime.shutdown().unwrap();
    }

    #[test]
    fn shutdown_commits_mutations_already_accepted_by_core() {
        let path = temporary_database_path();
        remove_database_files(&path);
        seed_file_database(&path);

        let database = sqlite::spawn(path.clone()).unwrap();
        let (core, _events, runtime) = spawn_with_options(EVENT_CAPACITY, database).unwrap();
        core.try_mutate(mutation_request(1, 0, 1)).unwrap();

        runtime.shutdown().unwrap();

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
    fn shutdown_interrupts_database_queries_before_draining_mutations() {
        let database = test_database();
        let (started_tx, started_rx) = crossbeam_channel::bounded(1);
        database.0.try_run_long_query(started_tx).unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let (core, _events, runtime) = spawn_with_options(EVENT_CAPACITY, database).unwrap();
        core.try_mutate(mutation_request(1, 0, 1)).unwrap();
        let started = Instant::now();

        let error = runtime.shutdown().unwrap_err();

        assert!(matches!(error, RuntimeError::MutationDuringShutdown { .. }));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn shutdown_executes_busy_mutation_when_rejection_feedback_was_not_delivered() {
        let path = temporary_database_path();
        remove_database_files(&path);
        seed_file_database(&path);
        let database = sqlite::spawn(path.clone()).unwrap();
        let (started_tx, started_rx) = crossbeam_channel::bounded(1);
        database.0.try_run_long_query(started_tx).unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        for offset in 0..16_u64 {
            database
                .0
                .try_query_mailbox(
                    RequestId::new(100 + offset).unwrap(),
                    Generation::new(0),
                    PageSpec::new(
                        AccountScope::All,
                        FolderScope::Inbox,
                        None,
                        PageBoundary::First,
                        1,
                    )
                    .unwrap(),
                )
                .unwrap();
        }
        let (core, events, runtime) = spawn_with_options(1, database).unwrap();
        core.try_mutate(mutation_request(1, 0, 1)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while events.is_empty() && Instant::now() < deadline {
            thread::yield_now();
        }
        assert_eq!(
            events.len(),
            1,
            "busy mutation rejection should fill the event queue"
        );
        core.try_mutate(mutation_request(2, 0, 1)).unwrap();
        thread::sleep(Duration::from_millis(20));

        runtime.shutdown().unwrap();

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
    fn mailbox_schedule_replaces_obsolete_pending_query() {
        let mut schedule = MailboxSchedule::default();
        let first = mailbox_query(1, 1);
        let first_key = first.key();
        let obsolete = mailbox_query(2, 2);
        let latest = mailbox_query(3, 3);
        let latest_key = latest.key();

        let MailboxEnqueue::Dispatch(initial) = schedule.enqueue(first) else {
            panic!("first mailbox query should be dispatched");
        };
        assert_eq!(initial.key(), first_key);
        assert!(matches!(
            schedule.enqueue(obsolete),
            MailboxEnqueue::Supersede(key) if key == first_key
        ));
        assert!(matches!(
            schedule.enqueue(latest),
            MailboxEnqueue::Supersede(key) if key == first_key
        ));

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
    fn account_directory_schedule_replaces_obsolete_pending_query() {
        let mut schedule = AccountDirectorySchedule::default();
        let first = account_directory_query(1, 1);
        let first_key = first.key();
        let obsolete = account_directory_query(2, 2);
        let latest = account_directory_query(3, 3);
        let latest_key = latest.key();

        assert!(schedule.enqueue(first).is_some());
        assert!(schedule.enqueue(obsolete).is_none());
        assert!(schedule.enqueue(latest).is_none());

        let AccountDirectoryCompletion::Dispatch(next) = schedule.complete(first_key) else {
            panic!("latest pending account query should be dispatched");
        };
        assert_eq!(next.key(), latest_key);
        assert!(matches!(
            schedule.complete(latest_key),
            AccountDirectoryCompletion::Publish
        ));
    }

    #[test]
    fn account_directory_schedule_accepts_retry_after_submission_failure() {
        let mut schedule = AccountDirectorySchedule::default();
        let failed = account_directory_query(1, 1);
        let failed_key = failed.key();

        assert!(schedule.enqueue(failed).is_some());
        schedule.submission_failed(failed_key);

        let retry = account_directory_query(2, 1);
        let retry_key = retry.key();
        assert_eq!(
            schedule.enqueue(retry).map(|query| query.key()),
            Some(retry_key)
        );
        assert!(matches!(
            schedule.complete(retry_key),
            AccountDirectoryCompletion::Publish
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
            .try_send(Command::QueryAccountDirectory(account_directory_query(
                1, 1,
            )))
            .unwrap();
        let (_shutdown_tx, shutdown_rx) = oneshot::channel();
        let mut shutdown = Box::pin(shutdown_rx);
        let (_account_tx, account_rx) = mpsc::channel(1);
        let mut account_commands = Some(account_rx);
        let mut compose_commands = None;
        let mut outbox_statuses = None;
        let (credential_client, credential_runtime) = credentials::spawn();
        let mut account_workflows = AccountWorkflows::new(
            database.clone(),
            credential_client,
            production_imap_diagnostic,
        );
        let input = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(next_input(
                &mut shutdown,
                &mut commands,
                &mut database_replies,
                &mut account_commands,
                &mut account_workflows,
                &mut compose_commands,
                &mut outbox_statuses,
                false,
            ));

        assert!(matches!(input, CoreInput::Database(DbReply::Mailbox(_))));
        credential_runtime.shutdown().unwrap();
        database_runtime.shutdown().unwrap();
    }

    #[test]
    fn shutdown_interrupts_query_event_backpressure() {
        let (core, events, runtime) = spawn_with_options(1, test_database()).unwrap();
        core.try_mutate(mutation_request(1, 0, 1)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while events.is_empty() && Instant::now() < deadline {
            thread::yield_now();
        }
        assert_eq!(
            events.len(),
            1,
            "mutation event should fill the event queue"
        );
        core.try_query_mailbox(mailbox_query(4, 4)).unwrap();
        thread::sleep(Duration::from_millis(20));
        let started = Instant::now();

        runtime.shutdown().unwrap();

        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
