mod account;
mod compose;
mod message;
#[allow(dead_code)]
mod outbound;
mod outbox_driver;
mod runtime;
mod scheduler;
mod sync;

#[allow(unused_imports)]
pub(crate) use account::{
    AccountConfigDraft, AccountOperation, AccountOperationFailure, AccountOperationReply,
    AccountOperationSuccess, AccountSetupMode, AccountSyncStatus, AccountWorkflowFailureKind,
    AccountWorkflowStage, InboxSyncFailureKind,
};
pub(crate) use message::{CoreHandle, Event, EventReceiver, OutboxCancelOutcome};
pub(crate) use outbox_driver::{OutboxDriverFault, OutboxStatus};
pub(crate) use runtime::{CoreRuntime, StartError};

pub(crate) use compose::{
    COMPOSE_BODY_BYTE_LIMIT, COMPOSE_SUBJECT_BYTE_LIMIT, COMPOSE_TO_FIELD_BYTE_LIMIT,
    ComposeDraftIdentity, ComposeDraftInput, ComposeFailure, ComposeFailureKind, ComposeOperation,
    ComposeSuccess,
};

#[allow(unused_imports)]
pub(crate) use crate::store::sqlite::{
    AccountScope, AccountStatsDelta, FolderScope, Generation, MessageId, MessageMutation,
    MutationOutcome, OutboxActionFence, OutboxErrorClass, OutboxRecipientSummary, OutboxState,
    OutboxSummary, OutboxSummaryPage, PageBoundary, PageSpec, RequestId, UncertainResolution,
    UndoToken,
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
    auto_sync_enabled: bool,
) -> Result<
    (
        CoreHandle,
        EventReceiver,
        CoreRuntime,
        crate::store::sqlite::DatabaseClient,
    ),
    StartError,
> {
    runtime::spawn_with_database(database_path, auto_sync_enabled)
}
