mod account;
mod compose;
mod message;
#[allow(dead_code)]
mod outbound;
mod outbox_driver;
mod runtime;
mod sync;

#[allow(unused_imports)]
pub(crate) use account::{
    AccountConfigDraft, AccountOperation, AccountOperationFailure, AccountOperationReply,
    AccountOperationSuccess, AccountSetupMode, AccountWorkflowFailureKind, AccountWorkflowStage,
    InboxSyncFailureKind,
};
pub(crate) use message::{CoreHandle, Event, EventReceiver};
pub(crate) use outbox_driver::{OutboxDriverFault, OutboxStatus};
pub(crate) use runtime::{CoreRuntime, StartError};

pub(crate) use compose::{
    ComposeDraftIdentity, ComposeDraftInput, ComposeFailure, ComposeFailureKind, ComposeOperation,
    ComposeSuccess,
};

#[allow(unused_imports)]
pub(crate) use crate::store::sqlite::{
    AccountScope, AccountStatsDelta, FolderScope, Generation, MessageId, MessageMutation,
    MutationOutcome, PageBoundary, PageSpec, RequestId, UndoToken,
};
#[allow(unused_imports)]
pub(crate) use message::{
    AccountDirectoryLoadError, AccountDirectoryQuery, AccountOperationResponse,
    AccountOperationResponseError, AccountOperationSubmitError, AccountOperationSubmitFailure,
    ComposeOperationResponse, ComposeOperationResponseError, ComposeOperationSubmitError,
    ComposeOperationSubmitFailure, MailboxLoadError, MailboxQuery, MessageLoadError, MessageQuery,
    MutationRequest, MutationSubmitError, SubmitError,
};

#[cfg_attr(feature = "bench-harness", allow(dead_code))]
pub(crate) fn spawn(
    database_path: std::path::PathBuf,
) -> Result<(CoreHandle, EventReceiver, CoreRuntime), StartError> {
    runtime::spawn(database_path)
}

#[cfg(feature = "bench-harness")]
pub(crate) fn spawn_with_database(
    database_path: std::path::PathBuf,
) -> Result<
    (
        CoreHandle,
        EventReceiver,
        CoreRuntime,
        crate::store::sqlite::DatabaseClient,
    ),
    StartError,
> {
    runtime::spawn_with_database(database_path)
}
