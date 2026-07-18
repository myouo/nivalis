mod message;
mod runtime;

pub(crate) use message::{CoreHandle, Event, EventReceiver, OperationId, SubmitError};
pub(crate) use runtime::{CoreRuntime, StartError};

#[allow(unused_imports)]
pub(crate) use crate::store::sqlite::{
    AccountScope, AccountStatsDelta, FolderScope, Generation, MessageId, MessageMutation,
    MutationOutcome, PageSpec, RequestId, UndoToken,
};
#[allow(unused_imports)]
pub(crate) use message::{
    MailboxLoadError, MailboxQuery, MessageLoadError, MessageQuery, MutationRequest,
    MutationSubmitError,
};

pub(crate) fn spawn(
    database_path: std::path::PathBuf,
) -> Result<(CoreHandle, EventReceiver, CoreRuntime), StartError> {
    runtime::spawn(database_path)
}
