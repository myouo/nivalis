use super::message::{COMMAND_CAPACITY, Command, CoreHandle, EVENT_CAPACITY, Event, EventReceiver};
use std::{
    fmt,
    future::{Future, poll_fn},
    sync::Arc,
    task::Poll,
    thread,
    time::Duration,
};
use tokio::{runtime::Builder, sync::mpsc, sync::oneshot, time};

const SYNC_DELAY: Duration = Duration::from_millis(900);

pub(crate) fn spawn() -> std::io::Result<(CoreHandle, EventReceiver, CoreRuntime)> {
    spawn_with_delay(SYNC_DELAY)
}

fn spawn_with_delay(
    sync_delay: Duration,
) -> std::io::Result<(CoreHandle, EventReceiver, CoreRuntime)> {
    let (command_tx, command_rx) = mpsc::channel(COMMAND_CAPACITY);
    let (event_tx, event_rx) = mpsc::channel(EVENT_CAPACITY);
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let runtime = Builder::new_current_thread()
        .enable_time()
        .max_blocking_threads(2)
        .build()?;
    let worker = thread::Builder::new()
        .name("nivalis-core".into())
        .spawn(move || runtime.block_on(run_core(command_rx, event_tx, shutdown_rx, sync_delay)))?;

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
    events: mpsc::Sender<Event>,
    shutdown: oneshot::Receiver<()>,
    sync_delay: Duration,
) -> Result<(), RuntimeError> {
    let mut shutdown = Box::pin(shutdown);

    loop {
        let command = poll_fn(|context| {
            if shutdown.as_mut().poll(context).is_ready() {
                Poll::Ready(None)
            } else {
                commands.poll_recv(context)
            }
        })
        .await;

        let Some(command) = command else {
            return Ok(());
        };

        match command {
            Command::SyncNow { operation_id } => {
                let mut delay = Box::pin(time::sleep(sync_delay));
                let interrupted = poll_fn(|context| {
                    if shutdown.as_mut().poll(context).is_ready() {
                        Poll::Ready(true)
                    } else if delay.as_mut().poll(context).is_ready() {
                        Poll::Ready(false)
                    } else {
                        Poll::Pending
                    }
                })
                .await;

                if interrupted {
                    return Ok(());
                }

                match events.try_send(Event::SyncFinished { operation_id }) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Closed(_)) => return Ok(()),
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        return Err(RuntimeError::EventQueueFull);
                    }
                }
            }
        }
    }
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
    EventQueueFull,
    ThreadPanicked { message: Arc<str> },
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EventQueueFull => formatter.write_str("core event queue is full"),
            Self::ThreadPanicked { message } => {
                write!(formatter, "core worker panicked: {message}")
            }
        }
    }
}

impl std::error::Error for RuntimeError {}

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
    use crate::core::OperationId;
    use std::time::Instant;

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
        let (core, _events, runtime) = spawn_with_delay(Duration::from_secs(5)).unwrap();
        core.try_send_sync(OperationId::new(1)).unwrap();
        let started = Instant::now();

        runtime.shutdown().unwrap();

        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
