mod actor;
mod domain;
mod migrations;
mod query;

pub(crate) use actor::{
    DatabaseClient, DatabaseInfo, DatabaseReplies, DatabaseRuntime, StartError,
    SubmitError as DatabaseSubmitError,
};
pub(crate) use domain::{AccountScope, FolderScope};
pub(crate) use domain::{
    DbReply, Generation, MailboxPage, MessageDetail, MessageId, PageSpec, RequestId, Tagged,
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
