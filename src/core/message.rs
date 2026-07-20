use super::{
    account::{AccountOperation, AccountOperationReply},
    compose::{ComposeOperation, ComposeReply},
    outbox_driver::OutboxStatus,
};
use crate::store::sqlite::{
    AccountDirectory, Generation, MailboxPage, MessageDetail, MessageId, MessageMutation,
    MutationOutcome, PageSpec, RequestId, Tagged,
};
use std::{
    fmt,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex, MutexGuard},
    task::{Context, Poll},
};
use tokio::sync::{mpsc, oneshot};

pub(crate) const COMMAND_CAPACITY: usize = 64;
pub(crate) const ACCOUNT_COMMAND_CAPACITY: usize = 4;
pub(crate) const COMPOSE_COMMAND_CAPACITY: usize = 1;
pub(crate) const EVENT_CAPACITY: usize = 128;

#[derive(Debug)]
pub(super) struct AccountCommand {
    pub(super) operation: AccountOperation,
    pub(super) reply: oneshot::Sender<AccountOperationReply>,
}

#[derive(Debug)]
pub(super) struct ComposeCommand {
    pub(super) operation: ComposeOperation,
    pub(super) reply: oneshot::Sender<ComposeReply>,
}

#[derive(Debug)]
pub(super) enum Command {
    #[cfg_attr(not(test), allow(dead_code))]
    QueryAccountDirectory(AccountDirectoryQuery),
    #[cfg_attr(not(test), allow(dead_code))]
    QueryMailbox(MailboxQuery),
    #[cfg_attr(not(test), allow(dead_code))]
    OpenMessage(MessageQuery),
    #[cfg_attr(not(test), allow(dead_code))]
    Mutate(MutationRequest),
    #[cfg(test)]
    Barrier(std::sync::mpsc::Sender<()>),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Event {
    AccountsLoaded(Tagged<AccountDirectory>),
    AccountsLoadRejected {
        request_id: RequestId,
        generation: Generation,
        reason: AccountDirectoryLoadError,
    },
    MailboxLoaded(Tagged<MailboxPage>),
    MailboxLoadRejected {
        request_id: RequestId,
        generation: Generation,
        reason: MailboxLoadError,
    },
    MessageLoaded(Tagged<Option<MessageDetail>>),
    MessageLoadRejected {
        request_id: RequestId,
        generation: Generation,
        reason: MessageLoadError,
    },
    MutationFinished(Tagged<MutationOutcome>),
    MutationRejected {
        request_id: RequestId,
        generation: Generation,
        reason: MutationSubmitError,
    },
    OutboxStatus(OutboxStatus),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct AccountDirectoryQuery {
    pub(super) request_id: RequestId,
    pub(super) generation: Generation,
}

impl AccountDirectoryQuery {
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn new(request_id: RequestId, generation: Generation) -> Self {
        Self {
            request_id,
            generation,
        }
    }

    pub(super) fn key(&self) -> AccountDirectoryRequestKey {
        AccountDirectoryRequestKey {
            request_id: self.request_id,
            generation: self.generation,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct AccountDirectoryRequestKey {
    pub(super) request_id: RequestId,
    pub(super) generation: Generation,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct MailboxQuery {
    pub(super) request_id: RequestId,
    pub(super) generation: Generation,
    pub(super) spec: PageSpec,
}

impl MailboxQuery {
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn new(request_id: RequestId, generation: Generation, spec: PageSpec) -> Self {
        Self {
            request_id,
            generation,
            spec,
        }
    }

    pub(super) fn key(&self) -> MailboxRequestKey {
        MailboxRequestKey {
            request_id: self.request_id,
            generation: self.generation,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct MailboxRequestKey {
    pub(super) request_id: RequestId,
    pub(super) generation: Generation,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct MessageQuery {
    pub(super) request_id: RequestId,
    pub(super) generation: Generation,
    pub(super) message_id: MessageId,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct MutationRequest {
    pub(super) request_id: RequestId,
    pub(super) generation: Generation,
    pub(super) mutation: MessageMutation,
}

impl MutationRequest {
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn new(
        request_id: RequestId,
        generation: Generation,
        mutation: MessageMutation,
    ) -> Self {
        Self {
            request_id,
            generation,
            mutation,
        }
    }
}

impl MessageQuery {
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn new(
        request_id: RequestId,
        generation: Generation,
        message_id: MessageId,
    ) -> Self {
        Self {
            request_id,
            generation,
            message_id,
        }
    }

    pub(super) fn key(&self) -> MessageRequestKey {
        MessageRequestKey {
            request_id: self.request_id,
            generation: self.generation,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct MessageRequestKey {
    pub(super) request_id: RequestId,
    pub(super) generation: Generation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AccountDirectoryLoadError {
    Busy,
    Unavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MailboxLoadError {
    Busy,
    Unavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MessageLoadError {
    Busy,
    Unavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MutationSubmitError {
    Busy,
    Unavailable,
}

enum EventEnvelope {
    Control(Event),
    AccountsReady,
    MailboxReady,
    MessageReady,
}

#[derive(Default)]
struct LatestEvent {
    event: Option<Event>,
    notification_queued: bool,
}

pub(super) struct EventSender {
    events: mpsc::Sender<EventEnvelope>,
    accounts: Arc<Mutex<LatestEvent>>,
    mailbox: Arc<Mutex<LatestEvent>>,
    message: Arc<Mutex<LatestEvent>>,
}

impl EventSender {
    pub(super) async fn send(&self, event: Event) -> Result<(), ()> {
        let envelope = if matches!(
            &event,
            Event::AccountsLoaded(_) | Event::AccountsLoadRejected { .. }
        ) {
            replace_latest(&self.accounts, event, EventEnvelope::AccountsReady)
        } else if matches!(
            &event,
            Event::MailboxLoaded(_) | Event::MailboxLoadRejected { .. }
        ) {
            replace_latest(&self.mailbox, event, EventEnvelope::MailboxReady)
        } else if matches!(
            &event,
            Event::MessageLoaded(_) | Event::MessageLoadRejected { .. }
        ) {
            replace_latest(&self.message, event, EventEnvelope::MessageReady)
        } else {
            Some(EventEnvelope::Control(event))
        };

        let Some(envelope) = envelope else {
            return Ok(());
        };

        self.events.send(envelope).await.map_err(|_| ())
    }
}

pub(crate) struct EventReceiver {
    events: mpsc::Receiver<EventEnvelope>,
    accounts: Arc<Mutex<LatestEvent>>,
    mailbox: Arc<Mutex<LatestEvent>>,
    message: Arc<Mutex<LatestEvent>>,
}

impl EventReceiver {
    pub(crate) async fn recv(&mut self) -> Option<Event> {
        loop {
            match self.events.recv().await? {
                EventEnvelope::Control(event) => return Some(event),
                EventEnvelope::AccountsReady => {
                    if let Some(event) = take_latest(&self.accounts) {
                        return Some(event);
                    }
                }
                EventEnvelope::MailboxReady => {
                    if let Some(event) = take_latest(&self.mailbox) {
                        return Some(event);
                    }
                }
                EventEnvelope::MessageReady => {
                    if let Some(event) = take_latest(&self.message) {
                        return Some(event);
                    }
                }
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn blocking_recv(&mut self) -> Option<Event> {
        loop {
            match self.events.blocking_recv()? {
                EventEnvelope::Control(event) => return Some(event),
                EventEnvelope::AccountsReady => {
                    if let Some(event) = take_latest(&self.accounts) {
                        return Some(event);
                    }
                }
                EventEnvelope::MailboxReady => {
                    if let Some(event) = take_latest(&self.mailbox) {
                        return Some(event);
                    }
                }
                EventEnvelope::MessageReady => {
                    if let Some(event) = take_latest(&self.message) {
                        return Some(event);
                    }
                }
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.events.len()
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

pub(super) fn event_channel(capacity: usize) -> (EventSender, EventReceiver) {
    let (sender, receiver) = mpsc::channel(capacity);
    let accounts = Arc::new(Mutex::new(LatestEvent::default()));
    let mailbox = Arc::new(Mutex::new(LatestEvent::default()));
    let message = Arc::new(Mutex::new(LatestEvent::default()));
    (
        EventSender {
            events: sender,
            accounts: accounts.clone(),
            mailbox: mailbox.clone(),
            message: message.clone(),
        },
        EventReceiver {
            events: receiver,
            accounts,
            mailbox,
            message,
        },
    )
}

fn replace_latest(
    slot: &Mutex<LatestEvent>,
    event: Event,
    notification: EventEnvelope,
) -> Option<EventEnvelope> {
    let mut slot = lock_latest(slot);
    slot.event = Some(event);
    if slot.notification_queued {
        None
    } else {
        slot.notification_queued = true;
        Some(notification)
    }
}

fn take_latest(slot: &Mutex<LatestEvent>) -> Option<Event> {
    let mut slot = lock_latest(slot);
    slot.notification_queued = false;
    slot.event.take()
}

fn lock_latest(slot: &Mutex<LatestEvent>) -> MutexGuard<'_, LatestEvent> {
    slot.lock().unwrap_or_else(|poison| poison.into_inner())
}

#[derive(Clone)]
pub(crate) struct CoreHandle {
    commands: mpsc::Sender<Command>,
    account_commands: mpsc::Sender<AccountCommand>,
    compose_commands: mpsc::Sender<ComposeCommand>,
}

impl CoreHandle {
    pub(super) fn new(
        commands: mpsc::Sender<Command>,
        account_commands: mpsc::Sender<AccountCommand>,
        compose_commands: mpsc::Sender<ComposeCommand>,
    ) -> Self {
        Self {
            commands,
            account_commands,
            compose_commands,
        }
    }

    pub(crate) fn try_compose_operation(
        &self,
        operation: ComposeOperation,
    ) -> Result<ComposeOperationResponse, ComposeOperationSubmitFailure> {
        let (reply, receiver) = oneshot::channel();
        match self
            .compose_commands
            .try_send(ComposeCommand { operation, reply })
        {
            Ok(()) => Ok(ComposeOperationResponse { receiver }),
            Err(mpsc::error::TrySendError::Full(command)) => Err(ComposeOperationSubmitFailure {
                reason: ComposeOperationSubmitError::Busy,
                operation: Box::new(command.operation),
            }),
            Err(mpsc::error::TrySendError::Closed(command)) => Err(ComposeOperationSubmitFailure {
                reason: ComposeOperationSubmitError::Closed,
                operation: Box::new(command.operation),
            }),
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn try_account_operation(
        &self,
        operation: AccountOperation,
    ) -> Result<AccountOperationResponse, AccountOperationSubmitFailure> {
        let (reply, receiver) = oneshot::channel();
        match self
            .account_commands
            .try_send(AccountCommand { operation, reply })
        {
            Ok(()) => Ok(AccountOperationResponse { receiver }),
            Err(mpsc::error::TrySendError::Full(command)) => Err(AccountOperationSubmitFailure {
                reason: AccountOperationSubmitError::Busy,
                operation: Box::new(command.operation),
            }),
            Err(mpsc::error::TrySendError::Closed(command)) => Err(AccountOperationSubmitFailure {
                reason: AccountOperationSubmitError::Closed,
                operation: Box::new(command.operation),
            }),
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn try_query_account_directory(
        &self,
        query: AccountDirectoryQuery,
    ) -> Result<(), SubmitError> {
        self.commands
            .try_send(Command::QueryAccountDirectory(query))
            .map_err(SubmitError::from)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn try_query_mailbox(&self, query: MailboxQuery) -> Result<(), SubmitError> {
        self.commands
            .try_send(Command::QueryMailbox(query))
            .map_err(SubmitError::from)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn try_open_message(&self, query: MessageQuery) -> Result<(), SubmitError> {
        self.commands
            .try_send(Command::OpenMessage(query))
            .map_err(SubmitError::from)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn try_mutate(&self, request: MutationRequest) -> Result<(), SubmitError> {
        self.commands
            .try_send(Command::Mutate(request))
            .map_err(SubmitError::from)
    }

    #[cfg(test)]
    pub(crate) fn try_barrier(
        &self,
        reached: std::sync::mpsc::Sender<()>,
    ) -> Result<(), SubmitError> {
        self.commands
            .try_send(Command::Barrier(reached))
            .map_err(SubmitError::from)
    }
}

pub(crate) struct ComposeOperationResponse {
    receiver: oneshot::Receiver<ComposeReply>,
}

impl fmt::Debug for ComposeOperationResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ComposeOperationResponse(..)")
    }
}

impl Future for ComposeOperationResponse {
    type Output = Result<ComposeReply, ComposeOperationResponseError>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        match Pin::new(&mut self.receiver).poll(context) {
            Poll::Ready(Ok(reply)) => Poll::Ready(Ok(reply)),
            Poll::Ready(Err(_)) => Poll::Ready(Err(ComposeOperationResponseError::CoreClosed)),
            Poll::Pending => Poll::Pending,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ComposeOperationResponseError {
    CoreClosed,
}

impl fmt::Display for ComposeOperationResponseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("the compose operation stopped before producing a result")
    }
}

impl std::error::Error for ComposeOperationResponseError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ComposeOperationSubmitError {
    Busy,
    Closed,
}

pub(crate) struct ComposeOperationSubmitFailure {
    reason: ComposeOperationSubmitError,
    operation: Box<ComposeOperation>,
}

impl ComposeOperationSubmitFailure {
    pub(crate) fn reason(&self) -> ComposeOperationSubmitError {
        self.reason
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn into_operation(self) -> ComposeOperation {
        *self.operation
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn into_parts(self) -> (ComposeOperationSubmitError, ComposeOperation) {
        (self.reason, *self.operation)
    }
}

impl fmt::Debug for ComposeOperationSubmitFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ComposeOperationSubmitFailure")
            .field("reason", &self.reason)
            .field("operation", self.operation.as_ref())
            .finish()
    }
}

pub(crate) struct AccountOperationResponse {
    receiver: oneshot::Receiver<AccountOperationReply>,
}

impl fmt::Debug for AccountOperationResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AccountOperationResponse(..)")
    }
}

impl Future for AccountOperationResponse {
    type Output = Result<AccountOperationReply, AccountOperationResponseError>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        match Pin::new(&mut self.receiver).poll(context) {
            Poll::Ready(Ok(reply)) => Poll::Ready(Ok(reply)),
            Poll::Ready(Err(_)) => Poll::Ready(Err(AccountOperationResponseError::CoreClosed)),
            Poll::Pending => Poll::Pending,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AccountOperationResponseError {
    CoreClosed,
}

impl fmt::Display for AccountOperationResponseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("the account operation stopped before producing a result")
    }
}

impl std::error::Error for AccountOperationResponseError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AccountOperationSubmitError {
    Busy,
    Closed,
}

pub(crate) struct AccountOperationSubmitFailure {
    reason: AccountOperationSubmitError,
    operation: Box<AccountOperation>,
}

impl AccountOperationSubmitFailure {
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn reason(&self) -> AccountOperationSubmitError {
        self.reason
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn into_operation(self) -> AccountOperation {
        *self.operation
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn into_parts(self) -> (AccountOperationSubmitError, AccountOperation) {
        (self.reason, *self.operation)
    }
}

impl fmt::Debug for AccountOperationSubmitFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AccountOperationSubmitFailure")
            .field("reason", &self.reason)
            .field("operation", self.operation.as_ref())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SubmitError {
    Busy,
    Closed,
}

impl From<mpsc::error::TrySendError<Command>> for SubmitError {
    fn from(error: mpsc::error::TrySendError<Command>) -> Self {
        match error {
            mpsc::error::TrySendError::Full(_) => Self::Busy,
            mpsc::error::TrySendError::Closed(_) => Self::Closed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::account::{AccountConfigDraft, AccountOperationSuccess, AccountSetupMode};
    use super::*;
    use crate::{
        credentials::Secret,
        store::sqlite::{AccountGeneration, AccountId},
    };

    fn account_directory_query(request_id: u64) -> AccountDirectoryQuery {
        AccountDirectoryQuery::new(RequestId::new(request_id).unwrap(), Generation::new(0))
    }

    fn remove_account_operation(request_id: u64) -> AccountOperation {
        AccountOperation::Remove {
            request_id: RequestId::new(request_id).unwrap(),
            account_id: AccountId::new(1).unwrap(),
            expected_generation: AccountGeneration::new(1).unwrap(),
        }
    }

    fn setup_account_operation(request_id: u64, secret: &[u8]) -> AccountOperation {
        AccountOperation::Setup {
            request_id: RequestId::new(request_id).unwrap(),
            mode: AccountSetupMode::Create,
            draft: AccountConfigDraft::new(
                "Personal",
                "user@example.test",
                "user@example.test",
                "imap.example.test",
                993,
                "smtp.example.test",
                465,
                0x123456,
            )
            .unwrap(),
            secret: Secret::new(secret.to_vec()).unwrap(),
        }
    }

    fn load_compose_operation(account_id: i64) -> ComposeOperation {
        ComposeOperation::LoadLatest {
            account_id: AccountId::new(account_id).unwrap(),
            expected_generation: AccountGeneration::new(1).unwrap(),
        }
    }

    fn handle_with_account_channel(
        commands: mpsc::Sender<Command>,
        account_capacity: usize,
    ) -> (
        CoreHandle,
        mpsc::Receiver<AccountCommand>,
        mpsc::Receiver<ComposeCommand>,
    ) {
        let (account_commands, account_receiver) = mpsc::channel(account_capacity);
        let (compose_commands, compose_receiver) = mpsc::channel(COMPOSE_COMMAND_CAPACITY);
        (
            CoreHandle::new(commands, account_commands, compose_commands),
            account_receiver,
            compose_receiver,
        )
    }

    #[test]
    fn full_command_queue_is_reported_as_busy() {
        let (sender, _receiver) = mpsc::channel(1);
        let (handle, _account_receiver, _compose_receiver) = handle_with_account_channel(sender, 1);

        assert_eq!(
            handle.try_query_account_directory(account_directory_query(1)),
            Ok(())
        );
        assert_eq!(
            handle.try_query_account_directory(account_directory_query(2)),
            Err(SubmitError::Busy)
        );
    }

    #[test]
    fn closed_command_queue_is_reported() {
        let (sender, receiver) = mpsc::channel(1);
        let (handle, _account_receiver, _compose_receiver) = handle_with_account_channel(sender, 1);
        drop(receiver);

        assert_eq!(
            handle.try_query_account_directory(account_directory_query(1)),
            Err(SubmitError::Closed)
        );
    }

    #[test]
    fn account_commands_use_an_independent_bounded_queue() {
        let (sender, _receiver) = mpsc::channel(1);
        let (handle, mut account_receiver, _compose_receiver) =
            handle_with_account_channel(sender, 1);
        handle
            .try_query_account_directory(account_directory_query(1))
            .unwrap();

        let _response = handle
            .try_account_operation(remove_account_operation(2))
            .unwrap();

        let command = account_receiver.try_recv().unwrap();
        assert_eq!(command.operation.request_id(), RequestId::new(2).unwrap());
    }

    #[test]
    fn full_account_queue_returns_the_original_operation() {
        let (sender, _receiver) = mpsc::channel(1);
        let (handle, _account_receiver, _compose_receiver) = handle_with_account_channel(sender, 1);
        let _first = handle
            .try_account_operation(remove_account_operation(1))
            .unwrap();

        let failure = handle
            .try_account_operation(setup_account_operation(2, b"retry-secret"))
            .unwrap_err();

        assert_eq!(failure.reason(), AccountOperationSubmitError::Busy);
        let (reason, operation) = failure.into_parts();
        assert_eq!(reason, AccountOperationSubmitError::Busy);
        let AccountOperation::Setup { secret, .. } = operation else {
            panic!("expected the original setup operation");
        };
        assert_eq!(secret.expose(), b"retry-secret");
    }

    #[test]
    fn closed_account_queue_returns_the_original_operation() {
        let (sender, _receiver) = mpsc::channel(1);
        let (handle, account_receiver, _compose_receiver) = handle_with_account_channel(sender, 1);
        drop(account_receiver);

        let failure = handle
            .try_account_operation(remove_account_operation(7))
            .unwrap_err();

        assert_eq!(failure.reason(), AccountOperationSubmitError::Closed);
        assert_eq!(
            failure.into_operation().request_id(),
            RequestId::new(7).unwrap()
        );
    }

    #[test]
    fn account_operation_response_maps_reply_and_channel_close() {
        let (sender, _receiver) = mpsc::channel(1);
        let (handle, mut account_receiver, _compose_receiver) =
            handle_with_account_channel(sender, 2);
        let response = handle
            .try_account_operation(remove_account_operation(8))
            .unwrap();
        let command = account_receiver.try_recv().unwrap();
        let expected = AccountOperationReply {
            request_id: RequestId::new(8).unwrap(),
            result: Ok(AccountOperationSuccess::Removed {
                account_id: AccountId::new(1).unwrap(),
            }),
        };
        command.reply.send(expected).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        assert_eq!(runtime.block_on(response).unwrap(), expected);

        let closed = handle
            .try_account_operation(remove_account_operation(9))
            .unwrap();
        drop(account_receiver.try_recv().unwrap());
        assert_eq!(
            runtime.block_on(closed),
            Err(AccountOperationResponseError::CoreClosed)
        );
    }

    #[test]
    fn full_compose_queue_returns_the_original_operation() {
        let (sender, _receiver) = mpsc::channel(1);
        let (handle, _account_receiver, _compose_receiver) = handle_with_account_channel(sender, 1);
        let _first = handle
            .try_compose_operation(load_compose_operation(1))
            .unwrap();

        let failure = handle
            .try_compose_operation(load_compose_operation(2))
            .unwrap_err();

        assert_eq!(failure.reason(), ComposeOperationSubmitError::Busy);
        let ComposeOperation::LoadLatest { account_id, .. } = failure.into_operation() else {
            panic!("expected original load operation");
        };
        assert_eq!(account_id, AccountId::new(2).unwrap());
    }

    #[test]
    fn closed_compose_queue_returns_the_original_operation() {
        let (sender, _receiver) = mpsc::channel(1);
        let (handle, _account_receiver, compose_receiver) = handle_with_account_channel(sender, 1);
        drop(compose_receiver);

        let failure = handle
            .try_compose_operation(load_compose_operation(3))
            .unwrap_err();
        let (reason, operation) = failure.into_parts();

        assert_eq!(reason, ComposeOperationSubmitError::Closed);
        let ComposeOperation::LoadLatest { account_id, .. } = operation else {
            panic!("expected original load operation");
        };
        assert_eq!(account_id, AccountId::new(3).unwrap());
    }

    #[test]
    fn mailbox_event_slot_keeps_only_the_latest_result() {
        let (sender, mut receiver) = event_channel(4);
        let first = Event::MailboxLoadRejected {
            request_id: RequestId::new(1).unwrap(),
            generation: Generation::new(1),
            reason: MailboxLoadError::Busy,
        };
        let latest = Event::MailboxLoadRejected {
            request_id: RequestId::new(2).unwrap(),
            generation: Generation::new(2),
            reason: MailboxLoadError::Unavailable,
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();

        runtime.block_on(sender.send(first)).unwrap();
        runtime.block_on(sender.send(latest)).unwrap();

        assert_eq!(receiver.len(), 1);
        assert_eq!(
            receiver.blocking_recv(),
            Some(Event::MailboxLoadRejected {
                request_id: RequestId::new(2).unwrap(),
                generation: Generation::new(2),
                reason: MailboxLoadError::Unavailable,
            })
        );
        assert!(receiver.is_empty());
    }

    #[test]
    fn account_directory_event_slot_keeps_only_the_latest_result() {
        let (sender, mut receiver) = event_channel(4);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();

        runtime
            .block_on(sender.send(Event::AccountsLoadRejected {
                request_id: RequestId::new(1).unwrap(),
                generation: Generation::new(1),
                reason: AccountDirectoryLoadError::Busy,
            }))
            .unwrap();
        runtime
            .block_on(sender.send(Event::AccountsLoadRejected {
                request_id: RequestId::new(2).unwrap(),
                generation: Generation::new(2),
                reason: AccountDirectoryLoadError::Unavailable,
            }))
            .unwrap();

        assert_eq!(receiver.len(), 1);
        assert_eq!(
            receiver.blocking_recv(),
            Some(Event::AccountsLoadRejected {
                request_id: RequestId::new(2).unwrap(),
                generation: Generation::new(2),
                reason: AccountDirectoryLoadError::Unavailable,
            })
        );
        assert!(receiver.is_empty());
    }

    #[test]
    fn account_mailbox_and_message_results_use_independent_slots() {
        let (sender, mut receiver) = event_channel(4);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime
            .block_on(sender.send(Event::AccountsLoadRejected {
                request_id: RequestId::new(1).unwrap(),
                generation: Generation::new(1),
                reason: AccountDirectoryLoadError::Busy,
            }))
            .unwrap();
        runtime
            .block_on(sender.send(Event::MailboxLoadRejected {
                request_id: RequestId::new(2).unwrap(),
                generation: Generation::new(2),
                reason: MailboxLoadError::Busy,
            }))
            .unwrap();
        runtime
            .block_on(sender.send(Event::MessageLoadRejected {
                request_id: RequestId::new(3).unwrap(),
                generation: Generation::new(3),
                reason: MessageLoadError::Busy,
            }))
            .unwrap();

        assert_eq!(receiver.len(), 3);
        assert!(matches!(
            receiver.blocking_recv(),
            Some(Event::AccountsLoadRejected { .. })
        ));
        assert!(matches!(
            receiver.blocking_recv(),
            Some(Event::MailboxLoadRejected { .. })
        ));
        assert!(matches!(
            receiver.blocking_recv(),
            Some(Event::MessageLoadRejected { .. })
        ));
    }

    #[test]
    fn mutation_results_are_never_coalesced() {
        let (sender, mut receiver) = event_channel(2);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        for request_id in [1, 2] {
            runtime
                .block_on(sender.send(Event::MutationRejected {
                    request_id: RequestId::new(request_id).unwrap(),
                    generation: Generation::new(0),
                    reason: MutationSubmitError::Busy,
                }))
                .unwrap();
        }

        assert_eq!(receiver.len(), 2);
        for request_id in [1, 2] {
            assert_eq!(
                receiver.blocking_recv(),
                Some(Event::MutationRejected {
                    request_id: RequestId::new(request_id).unwrap(),
                    generation: Generation::new(0),
                    reason: MutationSubmitError::Busy,
                })
            );
        }
    }
}
