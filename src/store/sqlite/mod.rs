mod actor;
mod domain;
mod journal;
mod migrations;
mod mutation;
mod query;
mod stats;

pub(crate) use actor::{
    DatabaseClient, DatabaseInfo, DatabaseReplies, DatabaseRuntime, StartError,
    SubmitError as DatabaseSubmitError,
};
pub(crate) use domain::{AccountScope, FolderScope};
pub(crate) use domain::{
    AccountStatsDelta, DbReply, Generation, MailboxPage, MessageDetail, MessageId, MessageMutation,
    MutationOutcome, PageSpec, RequestId, Tagged, UndoToken,
};

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
