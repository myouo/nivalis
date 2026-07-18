mod message;
mod runtime;

pub(crate) use message::{CoreHandle, Event, EventReceiver, OperationId, SubmitError};
pub(crate) use runtime::{CoreRuntime, StartError};

#[allow(unused_imports)]
pub(crate) use crate::store::sqlite::{AccountScope, FolderScope, Generation, PageSpec, RequestId};
#[allow(unused_imports)]
pub(crate) use message::{MailboxLoadError, MailboxQuery};

pub(crate) fn spawn(
    database_path: std::path::PathBuf,
) -> Result<(CoreHandle, EventReceiver, CoreRuntime), StartError> {
    runtime::spawn(database_path)
}
