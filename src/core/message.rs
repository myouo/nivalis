use crate::store::sqlite::{Generation, MailboxPage, PageSpec, RequestId, Tagged};
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::sync::mpsc;

pub(crate) const COMMAND_CAPACITY: usize = 64;
pub(crate) const EVENT_CAPACITY: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OperationId(u64);

impl OperationId {
    pub(crate) fn new(value: u64) -> Self {
        Self(value)
    }
}

#[derive(Debug)]
pub(super) enum Command {
    SyncNow {
        operation_id: OperationId,
    },
    #[cfg_attr(not(test), allow(dead_code))]
    QueryMailbox(MailboxQuery),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Event {
    SyncFinished {
        operation_id: OperationId,
    },
    MailboxLoaded(Tagged<MailboxPage>),
    MailboxLoadRejected {
        request_id: RequestId,
        generation: Generation,
        reason: MailboxLoadError,
    },
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MailboxLoadError {
    Busy,
    Unavailable,
}

enum EventEnvelope {
    Control(Event),
    MailboxReady,
}

#[derive(Default)]
struct LatestMailboxEvent {
    event: Option<Event>,
    notification_queued: bool,
}

pub(super) struct EventSender {
    events: mpsc::Sender<EventEnvelope>,
    mailbox: Arc<Mutex<LatestMailboxEvent>>,
}

impl EventSender {
    pub(super) async fn send(&self, event: Event) -> Result<(), ()> {
        let envelope = if matches!(
            &event,
            Event::MailboxLoaded(_) | Event::MailboxLoadRejected { .. }
        ) {
            let mut mailbox = lock_mailbox(&self.mailbox);
            mailbox.event = Some(event);
            if mailbox.notification_queued {
                return Ok(());
            }
            mailbox.notification_queued = true;
            EventEnvelope::MailboxReady
        } else {
            EventEnvelope::Control(event)
        };

        self.events.send(envelope).await.map_err(|_| ())
    }
}

pub(crate) struct EventReceiver {
    events: mpsc::Receiver<EventEnvelope>,
    mailbox: Arc<Mutex<LatestMailboxEvent>>,
}

impl EventReceiver {
    pub(crate) async fn recv(&mut self) -> Option<Event> {
        loop {
            match self.events.recv().await? {
                EventEnvelope::Control(event) => return Some(event),
                EventEnvelope::MailboxReady => {
                    let mut mailbox = lock_mailbox(&self.mailbox);
                    mailbox.notification_queued = false;
                    if let Some(event) = mailbox.event.take() {
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
                EventEnvelope::MailboxReady => {
                    let mut mailbox = lock_mailbox(&self.mailbox);
                    mailbox.notification_queued = false;
                    if let Some(event) = mailbox.event.take() {
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
    let mailbox = Arc::new(Mutex::new(LatestMailboxEvent::default()));
    (
        EventSender {
            events: sender,
            mailbox: mailbox.clone(),
        },
        EventReceiver {
            events: receiver,
            mailbox,
        },
    )
}

fn lock_mailbox(mailbox: &Mutex<LatestMailboxEvent>) -> MutexGuard<'_, LatestMailboxEvent> {
    mailbox.lock().unwrap_or_else(|poison| poison.into_inner())
}

#[derive(Clone)]
pub(crate) struct CoreHandle {
    commands: mpsc::Sender<Command>,
}

impl CoreHandle {
    pub(super) fn new(commands: mpsc::Sender<Command>) -> Self {
        Self { commands }
    }

    pub(crate) fn try_send_sync(&self, operation_id: OperationId) -> Result<(), SubmitError> {
        self.commands
            .try_send(Command::SyncNow { operation_id })
            .map_err(SubmitError::from)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn try_query_mailbox(&self, query: MailboxQuery) -> Result<(), SubmitError> {
        self.commands
            .try_send(Command::QueryMailbox(query))
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

    #[test]
    fn full_command_queue_is_reported_as_busy() {
        let (sender, _receiver) = mpsc::channel(1);
        let handle = CoreHandle::new(sender);

        assert_eq!(handle.try_send_sync(OperationId::new(1)), Ok(()));
        assert_eq!(
            handle.try_send_sync(OperationId::new(2)),
            Err(SubmitError::Busy)
        );
    }

    #[test]
    fn closed_command_queue_is_reported() {
        let (sender, receiver) = mpsc::channel(1);
        let handle = CoreHandle::new(sender);
        drop(receiver);

        assert_eq!(
            handle.try_send_sync(OperationId::new(1)),
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
}
