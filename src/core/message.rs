use crate::store::sqlite::{
    AccountDirectory, Generation, MailboxPage, MessageDetail, MessageId, MessageMutation,
    MutationOutcome, PageSpec, RequestId, Tagged,
};
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::sync::mpsc;

pub(crate) const COMMAND_CAPACITY: usize = 64;
pub(crate) const EVENT_CAPACITY: usize = 128;

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
}

impl CoreHandle {
    pub(super) fn new(commands: mpsc::Sender<Command>) -> Self {
        Self { commands }
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
    use super::*;

    fn account_directory_query(request_id: u64) -> AccountDirectoryQuery {
        AccountDirectoryQuery::new(RequestId::new(request_id).unwrap(), Generation::new(0))
    }

    #[test]
    fn full_command_queue_is_reported_as_busy() {
        let (sender, _receiver) = mpsc::channel(1);
        let handle = CoreHandle::new(sender);

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
        let handle = CoreHandle::new(sender);
        drop(receiver);

        assert_eq!(
            handle.try_query_account_directory(account_directory_query(1)),
            Err(SubmitError::Closed)
        );
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
