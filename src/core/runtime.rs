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

#[cfg(test)]
type SyncStarted = Option<std::sync::mpsc::SyncSender<()>>;
#[cfg(not(test))]
type SyncStarted = ();

pub(crate) fn spawn() -> std::io::Result<(CoreHandle, EventReceiver, CoreRuntime)> {
    spawn_with_delay(SYNC_DELAY)
}

fn spawn_with_delay(
    sync_delay: Duration,
) -> std::io::Result<(CoreHandle, EventReceiver, CoreRuntime)> {
    #[cfg(test)]
    {
        spawn_with_options(sync_delay, EVENT_CAPACITY, None)
    }
    #[cfg(not(test))]
    {
        spawn_with_options(sync_delay, EVENT_CAPACITY, ())
    }
}

fn spawn_with_options(
    sync_delay: Duration,
    event_capacity: usize,
    sync_started: SyncStarted,
) -> std::io::Result<(CoreHandle, EventReceiver, CoreRuntime)> {
    let (command_tx, command_rx) = mpsc::channel(COMMAND_CAPACITY);
    let (event_tx, event_rx) = mpsc::channel(event_capacity);
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let runtime = Builder::new_current_thread()
        .enable_time()
        .max_blocking_threads(2)
        .build()?;
    let worker = thread::Builder::new()
        .name("nivalis-core".into())
        .spawn(move || {
            runtime.block_on(run_core(
                command_rx,
                event_tx,
                shutdown_rx,
                sync_delay,
                sync_started,
            ))
        })?;

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
    _sync_started: SyncStarted,
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
                #[cfg(test)]
                if let Some(sync_started) = &_sync_started {
                    let _ = sync_started.try_send(());
                }
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

                let mut delivery = Box::pin(events.send(Event::SyncFinished { operation_id }));
                let delivery = poll_fn(|context| {
                    if shutdown.as_mut().poll(context).is_ready() {
                        Poll::Ready(None)
                    } else {
                        delivery.as_mut().poll(context).map(Some)
                    }
                })
                .await;

                match delivery {
                    Some(Ok(())) => {}
                    Some(Err(_)) | None => return Ok(()),
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
    ThreadPanicked { message: Arc<str> },
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
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
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let (core, _events, runtime) =
            spawn_with_options(Duration::from_secs(5), EVENT_CAPACITY, Some(started_tx)).unwrap();
        core.try_send_sync(OperationId::new(1)).unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let started = Instant::now();

        runtime.shutdown().unwrap();

        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn full_event_queue_applies_backpressure_without_stopping_core() {
        let (core, mut events, runtime) =
            spawn_with_options(Duration::from_millis(1), 1, None).unwrap();
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
}
