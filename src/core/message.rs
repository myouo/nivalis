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
    SyncNow { operation_id: OperationId },
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Event {
    SyncFinished { operation_id: OperationId },
}

pub(crate) type EventReceiver = mpsc::Receiver<Event>;

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
}
