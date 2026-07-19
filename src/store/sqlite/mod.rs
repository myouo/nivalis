mod actor;
mod domain;
mod journal;
mod migrations;
mod mutation;
mod query;
pub(crate) mod remote;
mod stats;

#[cfg(feature = "bench-harness")]
pub(crate) use query::{MailboxQueryCounts, mailbox_query_counts};

pub(crate) use actor::{
    DatabaseClient, DatabaseInfo, DatabaseReplies, DatabaseRuntime, StartError,
    SubmitError as DatabaseSubmitError,
};
#[allow(unused_imports)]
pub(crate) use actor::{
    RemoteReportExecutionFailure, RemoteReportReply, RemoteReportSubmitFailure,
};
#[allow(unused_imports)]
pub(crate) use domain::{
    AccountDirectory, AccountSummaryDto, AccountUnreadDto, MailSummaryDto, MailboxStatsDto,
    PageBoundary, PageCursor,
};
pub(crate) use domain::{AccountScope, FolderScope};
pub(crate) use domain::{
    AccountStatsDelta, DbFailure, DbReply, FailureKind, Generation, MailboxPage, MessageDetail,
    MessageId, MessageMutation, MutationOutcome, PageSpec, RequestId, Tagged, UndoToken,
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
