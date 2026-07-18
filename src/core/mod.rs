mod message;
mod runtime;

pub(crate) use message::{CoreHandle, Event, EventReceiver, OperationId, SubmitError};
pub(crate) use runtime::CoreRuntime;

pub(crate) fn spawn() -> std::io::Result<(CoreHandle, EventReceiver, CoreRuntime)> {
    runtime::spawn()
}
