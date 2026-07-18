mod actor;
mod domain;
mod migrations;
mod query;

pub(crate) use actor::{
    DatabaseClient, DatabaseInfo, DatabaseReplies, DatabaseRuntime, StartError,
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
