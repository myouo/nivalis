mod account;
mod actor;
mod content;
mod domain;
mod draft;
mod file_gc;
mod journal;
mod migrations;
mod mutation;
mod outbox;
mod query;
pub(crate) mod remote;
mod stats;
mod sync;

#[cfg(feature = "bench-harness")]
pub(crate) use query::{MailboxQueryCounts, mailbox_query_counts};

#[allow(unused_imports)]
pub(crate) use account::{
    AccountAuthKind, AccountConfigInput, AccountConfiguration, AccountDiagnostic,
    AccountDiagnosticKind, AccountGeneration, AccountLifecycle, AccountPurgeOutcome, AccountRecord,
    AccountRemovalTicket, AccountSetupTarget, AccountValidationError, AccountWrite,
    AccountWriteOutcome, DiagnosticCommit, DiagnosticEpoch, DiagnosticRecord, DiagnosticTicket,
    PendingCacheRemoval, PendingCredentialRemoval, RemovedAccount, SmtpSecurity,
};
#[allow(unused_imports)]
pub(crate) use actor::{
    AccountWriteExecutionFailure, AccountWriteReply, AccountWriteSubmitFailure,
    ContentImportExecutionFailure, ContentImportOutcome, ContentImportReply,
    ContentImportSubmission, ContentImportSubmitFailure, RemoteReportExecutionFailure,
    RemoteReportReply, RemoteReportSubmitFailure,
};
#[allow(unused_imports)]
pub(crate) use actor::{
    ComposeDbExecutionFailure, ComposeDbOperation, ComposeDbOutcome, ComposeDbReply,
    ComposeDbSubmitFailure,
};
pub(crate) use actor::{
    DatabaseClient, DatabaseInfo, DatabaseReplies, DatabaseRuntime, StartError,
    SubmitError as DatabaseSubmitError,
};
#[allow(unused_imports)]
pub(crate) use actor::{
    InboxCursorCommitExecutionFailure, InboxCursorCommitReply, InboxCursorCommitSubmitFailure,
    InboxStageExecutionFailure, InboxStageReply, InboxStageSubmitFailure,
};
#[allow(unused_imports)]
pub(crate) use domain::{
    AccountDirectory, AccountSummaryDto, AccountUnreadDto, MailSummaryDto, MailboxStatsDto,
    PageBoundary, PageCursor,
};
#[allow(unused_imports)]
pub(crate) use domain::{AccountId, AccountScope, FolderScope};
pub(crate) use domain::{
    AccountStatsDelta, DbFailure, DbReply, FailureKind, Generation, MailboxPage, MessageDetail,
    MessageId, MessageMutation, MutationOutcome, PageSpec, RequestId, Tagged, UndoToken,
};
#[allow(unused_imports)]
pub(crate) use draft::{
    DraftRecipient, DraftSnapshot, DraftUpdate, NewDraft, create_draft, load_draft,
    load_latest_draft, update_draft,
};
#[allow(unused_imports)]
pub(crate) use file_gc::FileGcOutcome;
#[allow(unused_imports)]
pub(crate) use outbox::{
    ArtifactObservation, OutboxClaim, OutboxClaimOutcome, OutboxErrorClass, OutboxLease,
    OutboxRecipient, OutboxRecoveryOutcome, OutboxReport, OutboxReportOutcome, OutboxReservation,
    OutboxReservationToken, OutboxReserveRequest, OutboxState, RecipientKind, ReservationRecovery,
    claim_next_outbox, claim_outbox, finalize_outbox, load_outbox_state, mark_outbox_data_started,
    recover_outbox, recover_reservation, release_failed_outbox, report_outbox, reserve_outbox,
    retry_outbox,
};
#[allow(unused_imports)]
pub(crate) use sync::{
    InboxCheckpoint, InboxCheckpointOutcome, InboxCursorCommit, InboxCursorOutcome,
    InboxCursorTicket, InboxEnvelope, InboxFlags, InboxReceivePage, InboxStageOutcome,
    InboxValidationError, StagedInboxMessage,
};

#[cfg(test)]
pub(crate) use domain::{MessageState, UndoReceipt};

#[cfg(test)]
pub(crate) fn undo_token_for_test(value: i64) -> UndoToken {
    UndoToken::from_database(value).expect("test undo tokens are positive")
}

pub(crate) fn spawn(
    path: impl Into<std::path::PathBuf>,
) -> Result<
    (
        DatabaseClient,
        DatabaseReplies,
        DatabaseRuntime,
        DatabaseInfo,
    ),
    StartError,
> {
    actor::spawn(path.into())
}

#[cfg(test)]
pub(crate) fn rebuild_account_stats_for_test(
    connection: &rusqlite::Connection,
    account_id: i64,
) -> Result<(), domain::DbFailure> {
    stats::rebuild_account(connection, account_id)
}

#[cfg(test)]
pub(crate) fn spawn_in_memory() -> Result<
    (
        DatabaseClient,
        DatabaseReplies,
        DatabaseRuntime,
        DatabaseInfo,
    ),
    StartError,
> {
    actor::spawn_in_memory()
}
