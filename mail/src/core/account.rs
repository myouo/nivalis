use std::{
    collections::VecDeque,
    fmt,
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use tokio::{
    sync::oneshot,
    time::{self, Sleep},
};

use crate::{
    credentials::{
        CredentialClient, CredentialDeleteOutcome, CredentialFailureKind, CredentialLocator,
        CredentialOperation, CredentialOutcome, CredentialResponse, CredentialSubmitError, Secret,
    },
    network::imap::{
        ImapDiagnosticFailure, ImapDiagnosticFailureKind, ImapDiagnosticRequest, ImapIdleOutcome,
        ImapIdleRequest, diagnose_app_password, wait_for_cached_inbox_change,
    },
    store::sqlite::{
        AccountAuthKind, AccountConfigInput, AccountConfiguration, AccountDiagnosticKind,
        AccountGeneration, AccountId, AccountLifecycle, AccountPurgeOutcome, AccountRecord,
        AccountRemovalTicket, AccountSyncTarget, AccountValidationError, AccountWrite,
        AccountWriteOutcome, AccountWriteReply, DatabaseClient, DatabaseSubmitError,
        DiagnosticCommit, DiagnosticRecord, DiagnosticTicket, FailureKind, MessageId,
        PendingCacheRemoval, PendingCredentialRemoval, RequestId, SmtpSecurity,
    },
};

#[cfg(test)]
use super::sync::production_imap_inbox_fetch;
use super::{
    scheduler::{SchedulerError, SyncCompletion, SyncScheduler, SyncToken},
    sync::{
        FetchMessageContentFuture, FetchMessageContentOutcome, FetchMessagePreviewsFuture,
        FetchMessagePreviewsOutcome, ImapInboxFetchProbe, SyncInboxFuture, SyncInboxOutcome,
        start_inbox_sync, start_message_content_fetch, start_message_preview_fetch,
    },
};

#[cfg_attr(not(test), allow(dead_code))]
const PLACEHOLDER_CREDENTIAL_KEY: &str = "00000000000000000000000000000000";
const RECOVERY_SUBMISSION_RETRY: Duration = Duration::from_millis(10);
const SCHEDULER_SUBMISSION_RETRY: Duration = Duration::from_millis(10);
const IDLE_RECONNECT_DELAY: Duration = Duration::from_secs(2);
const MAX_IDLE_WATCHES: usize = 10;

pub(super) type ImapDiagnosticFuture =
    Pin<Box<dyn Future<Output = Result<(), ImapDiagnosticFailure>> + 'static>>;
pub(super) type ImapDiagnosticProbe = fn(ImapDiagnosticRequest) -> ImapDiagnosticFuture;
type ImapIdleFuture = Pin<Box<dyn Future<Output = ImapIdleOutcome> + 'static>>;

pub(super) fn production_imap_diagnostic(request: ImapDiagnosticRequest) -> ImapDiagnosticFuture {
    Box::pin(diagnose_app_password(request))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AccountConfigDraft {
    name: Box<str>,
    address: Box<str>,
    login_name: Box<str>,
    imap_host: Box<str>,
    imap_port: u16,
    smtp_host: Box<str>,
    smtp_port: u16,
    smtp_security: SmtpSecurity,
    accent_rgb: u32,
}

impl AccountConfigDraft {
    #[cfg_attr(not(test), allow(dead_code))]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        name: &str,
        address: &str,
        login_name: &str,
        imap_host: &str,
        imap_port: u16,
        smtp_host: &str,
        smtp_port: u16,
        accent_rgb: u32,
    ) -> Result<Self, AccountValidationError> {
        let smtp_security = if smtp_port == 465 {
            SmtpSecurity::ImplicitTls
        } else {
            SmtpSecurity::StartTls
        };
        let validated = AccountConfigInput::new_with_smtp(
            PLACEHOLDER_CREDENTIAL_KEY,
            name,
            address,
            AccountAuthKind::AppPassword,
            login_name,
            imap_host,
            imap_port,
            smtp_host,
            smtp_port,
            smtp_security,
            true,
            accent_rgb,
        )?;
        Ok(Self {
            name: validated.name,
            address: validated.address,
            login_name: validated.login_name,
            imap_host: validated.imap_host,
            imap_port: validated.imap_port,
            smtp_host: validated.smtp_host,
            smtp_port: validated.smtp_port,
            smtp_security: validated.smtp_security,
            accent_rgb: validated.accent_rgb,
        })
    }

    fn into_input(self, locator: &CredentialLocator) -> AccountConfigInput {
        AccountConfigInput {
            credential_key: locator.as_str().into(),
            name: self.name,
            address: self.address,
            auth_kind: AccountAuthKind::AppPassword,
            login_name: self.login_name,
            imap_host: self.imap_host,
            imap_port: self.imap_port,
            smtp_host: self.smtp_host,
            smtp_port: self.smtp_port,
            smtp_security: self.smtp_security,
            smtp_explicit: true,
            accent_rgb: self.accent_rgb,
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AccountSetupMode {
    Create,
    #[allow(dead_code)]
    ConfigureExisting {
        account_id: AccountId,
        expected_generation: AccountGeneration,
    },
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) enum AccountOperation {
    Setup {
        request_id: RequestId,
        mode: AccountSetupMode,
        draft: AccountConfigDraft,
        secret: Secret,
    },
    RetryCredential {
        request_id: RequestId,
        account_id: AccountId,
        expected_generation: AccountGeneration,
        secret: Secret,
    },
    Diagnose {
        request_id: RequestId,
        account_id: AccountId,
        expected_generation: AccountGeneration,
    },
    SyncInbox {
        request_id: RequestId,
        account_id: AccountId,
        expected_generation: AccountGeneration,
        allow_history: bool,
    },
    FetchMessageContent {
        request_id: RequestId,
        message_id: MessageId,
        account_id: AccountId,
        expected_generation: AccountGeneration,
    },
    Remove {
        request_id: RequestId,
        account_id: AccountId,
        expected_generation: AccountGeneration,
    },
}

impl AccountOperation {
    pub(crate) fn request_id(&self) -> RequestId {
        match self {
            Self::Setup { request_id, .. }
            | Self::RetryCredential { request_id, .. }
            | Self::Diagnose { request_id, .. }
            | Self::SyncInbox { request_id, .. }
            | Self::FetchMessageContent { request_id, .. }
            | Self::Remove { request_id, .. } => *request_id,
        }
    }
}

impl fmt::Debug for AccountOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Setup {
                request_id,
                mode,
                draft,
                ..
            } => formatter
                .debug_struct("Setup")
                .field("request_id", request_id)
                .field("mode", mode)
                .field("draft", draft)
                .field("secret", &"[REDACTED]")
                .finish(),
            Self::RetryCredential {
                request_id,
                account_id,
                expected_generation,
                ..
            } => formatter
                .debug_struct("RetryCredential")
                .field("request_id", request_id)
                .field("account_id", account_id)
                .field("expected_generation", expected_generation)
                .field("secret", &"[REDACTED]")
                .finish(),
            Self::Diagnose {
                request_id,
                account_id,
                expected_generation,
            } => formatter
                .debug_struct("Diagnose")
                .field("request_id", request_id)
                .field("account_id", account_id)
                .field("expected_generation", expected_generation)
                .finish(),
            Self::SyncInbox {
                request_id,
                account_id,
                expected_generation,
                allow_history,
            } => formatter
                .debug_struct("SyncInbox")
                .field("request_id", request_id)
                .field("account_id", account_id)
                .field("expected_generation", expected_generation)
                .field("allow_history", allow_history)
                .finish(),
            Self::FetchMessageContent {
                request_id,
                message_id,
                account_id,
                expected_generation,
            } => formatter
                .debug_struct("FetchMessageContent")
                .field("request_id", request_id)
                .field("message_id", message_id)
                .field("account_id", account_id)
                .field("expected_generation", expected_generation)
                .finish(),
            Self::Remove {
                request_id,
                account_id,
                expected_generation,
            } => formatter
                .debug_struct("Remove")
                .field("request_id", request_id)
                .field("account_id", account_id)
                .field("expected_generation", expected_generation)
                .finish(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AccountOperationSuccess {
    Configured {
        account_id: AccountId,
        generation: AccountGeneration,
    },
    Diagnosed {
        account_id: AccountId,
        generation: AccountGeneration,
    },
    Synced {
        account_id: AccountId,
        generation: AccountGeneration,
        imported: u8,
        has_more: bool,
        historical: bool,
    },
    MessageContentFetched {
        message_id: MessageId,
        account_id: AccountId,
        generation: AccountGeneration,
    },
    Removed {
        account_id: AccountId,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AccountOperationFailure {
    pub(crate) account_id: Option<AccountId>,
    pub(crate) generation: Option<AccountGeneration>,
    pub(crate) stage: AccountWorkflowStage,
    pub(crate) kind: AccountWorkflowFailureKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AccountOperationReply {
    pub(crate) request_id: RequestId,
    pub(crate) result: Result<AccountOperationSuccess, AccountOperationFailure>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AccountSyncStatus {
    Synced {
        account_id: AccountId,
        generation: AccountGeneration,
        imported: u8,
        has_more: bool,
        historical: bool,
    },
    PreviewsUpdated {
        account_id: AccountId,
        generation: AccountGeneration,
        updated: u8,
    },
    Failed(AccountOperationFailure),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AccountWorkflowStage {
    LoadConfiguration,
    PersistLocator,
    StoreCredential,
    BeginDiagnostic,
    LoadCredential,
    ConnectImap,
    LoadInboxCheckpoint,
    LoadMessageTarget,
    FetchInbox,
    StageInbox,
    ParseContent,
    ImportContent,
    CommitInbox,
    RecordDiagnostic,
    BeginRemoval,
    DeleteCredential,
    ConfirmRemoval,
    PurgeRemoval,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AccountWorkflowFailureKind {
    Busy,
    Database(FailureKind),
    Credential(CredentialFailureKind),
    CredentialReplyClosed,
    Diagnostic(AccountDiagnosticKind),
    InboxSync(InboxSyncFailureKind),
    InvalidLocator,
    UnexpectedReply,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InboxSyncFailureKind {
    Authentication,
    Permission,
    Certificate,
    Timeout,
    Offline,
    Protocol,
    ResourceLimit,
    Cancelled,
    UidValidityChanged,
    MalformedContent,
    Storage,
}

pub(crate) enum AccountWorkflowRequest {
    PersistAndStore {
        write: Box<AccountWrite>,
        secret: Secret,
    },
    RetryStore {
        configuration: AccountConfiguration,
        secret: Secret,
    },
    BeginRemove {
        account_id: AccountId,
        expected_generation: AccountGeneration,
    },
    ResumeRemove(AccountRemovalTicket),
    ResumePurge {
        account_id: AccountId,
        expected_generation: AccountGeneration,
    },
}

impl fmt::Debug for AccountWorkflowRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PersistAndStore { write, .. } => formatter
                .debug_struct("PersistAndStore")
                .field("write", &account_write_kind(write))
                .field("secret", &"[REDACTED]")
                .finish(),
            Self::RetryStore { configuration, .. } => formatter
                .debug_struct("RetryStore")
                .field("account_id", &configuration.account_id)
                .field("generation", &configuration.generation)
                .field("secret", &"[REDACTED]")
                .finish(),
            Self::BeginRemove {
                account_id,
                expected_generation,
            } => formatter
                .debug_struct("BeginRemove")
                .field("account_id", account_id)
                .field("expected_generation", expected_generation)
                .finish(),
            Self::ResumeRemove(ticket) => formatter
                .debug_struct("ResumeRemove")
                .field("account_id", &ticket.account_id)
                .field("generation", &ticket.generation)
                .field("has_credential", &ticket.credential_key.is_some())
                .finish(),
            Self::ResumePurge {
                account_id,
                expected_generation,
            } => formatter
                .debug_struct("ResumePurge")
                .field("account_id", account_id)
                .field("expected_generation", expected_generation)
                .finish(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AccountWorkflowStartFailureKind {
    Busy,
    UnsupportedWrite,
    InvalidLocator,
}

pub(crate) struct AccountWorkflowStartFailure {
    kind: AccountWorkflowStartFailureKind,
    request: Box<AccountWorkflowRequest>,
}

impl AccountWorkflowStartFailure {
    #[cfg(test)]
    pub(crate) fn kind(&self) -> AccountWorkflowStartFailureKind {
        self.kind
    }

    #[cfg(test)]
    pub(crate) fn into_request(self) -> AccountWorkflowRequest {
        *self.request
    }
}

impl fmt::Debug for AccountWorkflowStartFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AccountWorkflowStartFailure")
            .field("kind", &self.kind)
            .field("request", &self.request)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) enum AccountWorkflowOutcome {
    CredentialStored(AccountConfiguration),
    CredentialPending {
        configuration: AccountConfiguration,
        failure: AccountWorkflowFailureKind,
    },
    AccountRemoved {
        account_id: AccountId,
    },
    Diagnostic {
        account_id: AccountId,
        generation: AccountGeneration,
        result: Result<(), AccountDiagnosticKind>,
    },
    InboxSynced {
        account_id: AccountId,
        generation: AccountGeneration,
        imported: u8,
        has_more: bool,
        historical: bool,
        bootstrap: bool,
        idle: ImapIdleRequest,
    },
    MessageContentFetched {
        message_id: MessageId,
        account_id: AccountId,
        generation: AccountGeneration,
    },
    MessagePreviewsFetched {
        account_id: AccountId,
        generation: AccountGeneration,
        updated: u8,
        has_more: bool,
    },
    RemovalPending {
        account_id: AccountId,
        generation: AccountGeneration,
        stage: AccountWorkflowStage,
        failure: AccountWorkflowFailureKind,
    },
    Failed {
        stage: AccountWorkflowStage,
        failure: AccountWorkflowFailureKind,
    },
}

impl fmt::Debug for AccountWorkflowOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CredentialStored(configuration) => formatter
                .debug_struct("CredentialStored")
                .field("account_id", &configuration.account_id)
                .field("generation", &configuration.generation)
                .finish(),
            Self::CredentialPending {
                configuration,
                failure,
            } => formatter
                .debug_struct("CredentialPending")
                .field("account_id", &configuration.account_id)
                .field("generation", &configuration.generation)
                .field("failure", failure)
                .finish(),
            Self::AccountRemoved { account_id } => formatter
                .debug_struct("AccountRemoved")
                .field("account_id", account_id)
                .finish(),
            Self::Diagnostic {
                account_id,
                generation,
                result,
            } => formatter
                .debug_struct("Diagnostic")
                .field("account_id", account_id)
                .field("generation", generation)
                .field("result", result)
                .finish(),
            Self::InboxSynced {
                account_id,
                generation,
                imported,
                has_more,
                historical,
                bootstrap,
                idle: _,
            } => formatter
                .debug_struct("InboxSynced")
                .field("account_id", account_id)
                .field("generation", generation)
                .field("imported", imported)
                .field("has_more", has_more)
                .field("historical", historical)
                .field("bootstrap", bootstrap)
                .finish(),
            Self::MessageContentFetched {
                message_id,
                account_id,
                generation,
            } => formatter
                .debug_struct("MessageContentFetched")
                .field("message_id", message_id)
                .field("account_id", account_id)
                .field("generation", generation)
                .finish(),
            Self::MessagePreviewsFetched {
                account_id,
                generation,
                updated,
                has_more,
            } => formatter
                .debug_struct("MessagePreviewsFetched")
                .field("account_id", account_id)
                .field("generation", generation)
                .field("updated", updated)
                .field("has_more", has_more)
                .finish(),
            Self::RemovalPending {
                account_id,
                generation,
                stage,
                failure,
            } => formatter
                .debug_struct("RemovalPending")
                .field("account_id", account_id)
                .field("generation", generation)
                .field("stage", stage)
                .field("failure", failure)
                .finish(),
            Self::Failed { stage, failure } => formatter
                .debug_struct("Failed")
                .field("stage", stage)
                .field("failure", failure)
                .finish(),
        }
    }
}

pub(crate) enum AccountWorkflowAction {
    Database {
        stage: AccountWorkflowStage,
        write: Box<AccountWrite>,
    },
    Credential {
        stage: AccountWorkflowStage,
        operation: CredentialOperation,
    },
    Finished(AccountWorkflowOutcome),
}

impl fmt::Debug for AccountWorkflowAction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database { stage, write } => formatter
                .debug_struct("Database")
                .field("stage", stage)
                .field("write", &account_write_kind(write))
                .finish(),
            Self::Credential { stage, operation } => formatter
                .debug_struct("Credential")
                .field("stage", stage)
                .field("operation", operation)
                .finish(),
            Self::Finished(outcome) => formatter.debug_tuple("Finished").field(outcome).finish(),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) enum AccountDatabaseCompletion {
    Saved(AccountConfiguration),
    RemovalStarted(AccountRemovalTicket),
    Purged(AccountPurgeOutcome),
    Failed(FailureKind),
    Unexpected,
}

impl fmt::Debug for AccountDatabaseCompletion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Saved(_) => formatter.write_str("Saved([ACCOUNT])"),
            Self::RemovalStarted(_) => formatter.write_str("RemovalStarted([TICKET])"),
            Self::Purged(AccountPurgeOutcome::Pending { .. }) => {
                formatter.write_str("Purged(Pending)")
            }
            Self::Purged(AccountPurgeOutcome::Complete(_)) => {
                formatter.write_str("Purged(Complete)")
            }
            Self::Failed(kind) => formatter.debug_tuple("Failed").field(kind).finish(),
            Self::Unexpected => formatter.write_str("Unexpected"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AccountCredentialCompletion {
    Stored,
    Deleted(CredentialDeleteOutcome),
    Failed(CredentialFailureKind),
    ReplyClosed,
    Unexpected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AccountTransitionError {
    Idle,
    ExpectedDatabase(AccountWorkflowStage),
    ExpectedCredential(AccountWorkflowStage),
}

#[derive(Default)]
pub(crate) struct AccountCoordinator {
    active: Option<ActiveWorkflow>,
}

impl AccountCoordinator {
    pub(crate) fn try_start(
        &mut self,
        request: AccountWorkflowRequest,
    ) -> Result<AccountWorkflowAction, AccountWorkflowStartFailure> {
        if self.active.is_some() {
            return Err(AccountWorkflowStartFailure {
                kind: AccountWorkflowStartFailureKind::Busy,
                request: Box::new(request),
            });
        }

        match request {
            AccountWorkflowRequest::PersistAndStore { write, secret } => {
                let Some(key) = persisted_credential_key(&write) else {
                    return Err(AccountWorkflowStartFailure {
                        kind: AccountWorkflowStartFailureKind::UnsupportedWrite,
                        request: Box::new(AccountWorkflowRequest::PersistAndStore {
                            write,
                            secret,
                        }),
                    });
                };
                let locator = match CredentialLocator::parse(key) {
                    Ok(locator) => locator,
                    Err(_) => {
                        return Err(AccountWorkflowStartFailure {
                            kind: AccountWorkflowStartFailureKind::InvalidLocator,
                            request: Box::new(AccountWorkflowRequest::PersistAndStore {
                                write,
                                secret,
                            }),
                        });
                    }
                };
                self.active = Some(ActiveWorkflow::Persisting { locator, secret });
                Ok(AccountWorkflowAction::Database {
                    stage: AccountWorkflowStage::PersistLocator,
                    write,
                })
            }
            AccountWorkflowRequest::RetryStore {
                configuration,
                secret,
            } => {
                let locator = match CredentialLocator::parse(&configuration.credential_key) {
                    Ok(locator) => locator,
                    Err(_) => {
                        return Err(AccountWorkflowStartFailure {
                            kind: AccountWorkflowStartFailureKind::InvalidLocator,
                            request: Box::new(AccountWorkflowRequest::RetryStore {
                                configuration,
                                secret,
                            }),
                        });
                    }
                };
                self.active = Some(ActiveWorkflow::Storing { configuration });
                Ok(AccountWorkflowAction::Credential {
                    stage: AccountWorkflowStage::StoreCredential,
                    operation: CredentialOperation::Store { locator, secret },
                })
            }
            AccountWorkflowRequest::BeginRemove {
                account_id,
                expected_generation,
            } => {
                self.active = Some(ActiveWorkflow::BeginningRemoval { account_id });
                Ok(AccountWorkflowAction::Database {
                    stage: AccountWorkflowStage::BeginRemoval,
                    write: Box::new(AccountWrite::BeginRemove {
                        account_id,
                        expected_generation,
                    }),
                })
            }
            AccountWorkflowRequest::ResumeRemove(ticket) => self.resume_removal(ticket),
            AccountWorkflowRequest::ResumePurge {
                account_id,
                expected_generation,
            } => Ok(self.purge(account_id, expected_generation)),
        }
    }

    #[cfg(test)]
    pub(crate) fn stage(&self) -> Option<AccountWorkflowStage> {
        self.active.as_ref().map(ActiveWorkflow::stage)
    }

    pub(crate) fn database_completed(
        &mut self,
        completion: AccountDatabaseCompletion,
    ) -> Result<AccountWorkflowAction, AccountTransitionError> {
        let Some(active) = self.active.as_ref() else {
            return Err(AccountTransitionError::Idle);
        };
        if !active.expects_database() {
            return Err(AccountTransitionError::ExpectedCredential(active.stage()));
        }
        let active = self.active.take().expect("active workflow was checked");
        Ok(match active {
            ActiveWorkflow::Persisting { locator, secret } => {
                self.complete_locator_persist(locator, secret, completion)
            }
            ActiveWorkflow::BeginningRemoval { account_id } => {
                self.complete_begin_removal(account_id, completion)
            }
            ActiveWorkflow::Confirming { ticket } => {
                self.complete_removal_confirmation(ticket, completion)
            }
            ActiveWorkflow::Purging {
                account_id,
                generation,
            } => self.complete_purge(account_id, generation, completion),
            ActiveWorkflow::Storing { .. } | ActiveWorkflow::Deleting { .. } => {
                unreachable!("credential stages were rejected before taking the workflow")
            }
        })
    }

    pub(crate) fn credential_completed(
        &mut self,
        completion: AccountCredentialCompletion,
    ) -> Result<AccountWorkflowAction, AccountTransitionError> {
        let Some(active) = self.active.as_ref() else {
            return Err(AccountTransitionError::Idle);
        };
        if active.expects_database() {
            return Err(AccountTransitionError::ExpectedDatabase(active.stage()));
        }
        let active = self.active.take().expect("active workflow was checked");
        Ok(match active {
            ActiveWorkflow::Storing { configuration } => complete_store(configuration, completion),
            ActiveWorkflow::Deleting { ticket } => self.complete_delete(ticket, completion),
            ActiveWorkflow::Persisting { .. }
            | ActiveWorkflow::BeginningRemoval { .. }
            | ActiveWorkflow::Confirming { .. }
            | ActiveWorkflow::Purging { .. } => {
                unreachable!("database stages were rejected before taking the workflow")
            }
        })
    }

    fn resume_removal(
        &mut self,
        ticket: AccountRemovalTicket,
    ) -> Result<AccountWorkflowAction, AccountWorkflowStartFailure> {
        let Some(key) = ticket.credential_key.as_deref() else {
            return Ok(self.purge(ticket.account_id, ticket.generation));
        };
        let locator = match CredentialLocator::parse(key) {
            Ok(locator) => locator,
            Err(_) => {
                return Err(AccountWorkflowStartFailure {
                    kind: AccountWorkflowStartFailureKind::InvalidLocator,
                    request: Box::new(AccountWorkflowRequest::ResumeRemove(ticket)),
                });
            }
        };
        self.active = Some(ActiveWorkflow::Deleting { ticket });
        Ok(AccountWorkflowAction::Credential {
            stage: AccountWorkflowStage::DeleteCredential,
            operation: CredentialOperation::Delete { locator },
        })
    }

    fn complete_locator_persist(
        &mut self,
        locator: CredentialLocator,
        secret: Secret,
        completion: AccountDatabaseCompletion,
    ) -> AccountWorkflowAction {
        match completion {
            AccountDatabaseCompletion::Saved(configuration)
                if configuration.credential_key.as_ref() == locator.as_str() =>
            {
                self.active = Some(ActiveWorkflow::Storing { configuration });
                AccountWorkflowAction::Credential {
                    stage: AccountWorkflowStage::StoreCredential,
                    operation: CredentialOperation::Store { locator, secret },
                }
            }
            AccountDatabaseCompletion::Failed(kind) => failed(
                AccountWorkflowStage::PersistLocator,
                AccountWorkflowFailureKind::Database(kind),
            ),
            AccountDatabaseCompletion::Saved(_)
            | AccountDatabaseCompletion::RemovalStarted(_)
            | AccountDatabaseCompletion::Purged(_)
            | AccountDatabaseCompletion::Unexpected => failed(
                AccountWorkflowStage::PersistLocator,
                AccountWorkflowFailureKind::UnexpectedReply,
            ),
        }
    }

    fn complete_begin_removal(
        &mut self,
        account_id: AccountId,
        completion: AccountDatabaseCompletion,
    ) -> AccountWorkflowAction {
        match completion {
            AccountDatabaseCompletion::RemovalStarted(ticket)
                if ticket.account_id == account_id && ticket.credential_key.is_none() =>
            {
                self.purge(ticket.account_id, ticket.generation)
            }
            AccountDatabaseCompletion::RemovalStarted(ticket)
                if ticket.account_id == account_id =>
            {
                let Some(key) = ticket.credential_key.as_deref() else {
                    unreachable!("credential-free removal was handled above")
                };
                let locator = match CredentialLocator::parse(key) {
                    Ok(locator) => locator,
                    Err(_) => {
                        return removal_pending(
                            ticket,
                            AccountWorkflowStage::DeleteCredential,
                            AccountWorkflowFailureKind::InvalidLocator,
                        );
                    }
                };
                self.active = Some(ActiveWorkflow::Deleting { ticket });
                AccountWorkflowAction::Credential {
                    stage: AccountWorkflowStage::DeleteCredential,
                    operation: CredentialOperation::Delete { locator },
                }
            }
            AccountDatabaseCompletion::Failed(kind) => failed(
                AccountWorkflowStage::BeginRemoval,
                AccountWorkflowFailureKind::Database(kind),
            ),
            AccountDatabaseCompletion::Saved(_)
            | AccountDatabaseCompletion::RemovalStarted(_)
            | AccountDatabaseCompletion::Purged(_)
            | AccountDatabaseCompletion::Unexpected => failed(
                AccountWorkflowStage::BeginRemoval,
                AccountWorkflowFailureKind::UnexpectedReply,
            ),
        }
    }

    fn complete_delete(
        &mut self,
        ticket: AccountRemovalTicket,
        completion: AccountCredentialCompletion,
    ) -> AccountWorkflowAction {
        match completion {
            AccountCredentialCompletion::Deleted(
                CredentialDeleteOutcome::Deleted | CredentialDeleteOutcome::AlreadyMissing,
            ) => {
                let account_id = ticket.account_id;
                let expected_generation = ticket.generation;
                self.active = Some(ActiveWorkflow::Confirming { ticket });
                AccountWorkflowAction::Database {
                    stage: AccountWorkflowStage::ConfirmRemoval,
                    write: Box::new(AccountWrite::ConfirmCredentialsRemoved {
                        account_id,
                        expected_generation,
                    }),
                }
            }
            AccountCredentialCompletion::Failed(kind) => removal_pending(
                ticket,
                AccountWorkflowStage::DeleteCredential,
                AccountWorkflowFailureKind::Credential(kind),
            ),
            AccountCredentialCompletion::ReplyClosed => removal_pending(
                ticket,
                AccountWorkflowStage::DeleteCredential,
                AccountWorkflowFailureKind::CredentialReplyClosed,
            ),
            AccountCredentialCompletion::Stored | AccountCredentialCompletion::Unexpected => {
                removal_pending(
                    ticket,
                    AccountWorkflowStage::DeleteCredential,
                    AccountWorkflowFailureKind::UnexpectedReply,
                )
            }
        }
    }

    fn complete_removal_confirmation(
        &mut self,
        ticket: AccountRemovalTicket,
        completion: AccountDatabaseCompletion,
    ) -> AccountWorkflowAction {
        match completion {
            AccountDatabaseCompletion::Saved(configuration)
                if configuration.account_id == ticket.account_id
                    && configuration.lifecycle == AccountLifecycle::RemovingCache =>
            {
                self.purge(configuration.account_id, configuration.generation)
            }
            AccountDatabaseCompletion::Failed(kind) => removal_pending(
                ticket,
                AccountWorkflowStage::ConfirmRemoval,
                AccountWorkflowFailureKind::Database(kind),
            ),
            AccountDatabaseCompletion::Saved(_)
            | AccountDatabaseCompletion::RemovalStarted(_)
            | AccountDatabaseCompletion::Purged(_)
            | AccountDatabaseCompletion::Unexpected => removal_pending(
                ticket,
                AccountWorkflowStage::ConfirmRemoval,
                AccountWorkflowFailureKind::UnexpectedReply,
            ),
        }
    }

    fn purge(
        &mut self,
        account_id: AccountId,
        generation: AccountGeneration,
    ) -> AccountWorkflowAction {
        self.active = Some(ActiveWorkflow::Purging {
            account_id,
            generation,
        });
        AccountWorkflowAction::Database {
            stage: AccountWorkflowStage::PurgeRemoval,
            write: Box::new(AccountWrite::PurgeRemovedAccount {
                account_id,
                expected_generation: generation,
            }),
        }
    }

    fn complete_purge(
        &mut self,
        account_id: AccountId,
        generation: AccountGeneration,
        completion: AccountDatabaseCompletion,
    ) -> AccountWorkflowAction {
        match completion {
            AccountDatabaseCompletion::Purged(AccountPurgeOutcome::Pending { .. }) => {
                self.purge(account_id, generation)
            }
            AccountDatabaseCompletion::Purged(AccountPurgeOutcome::Complete(removed))
                if removed.account_id == account_id =>
            {
                AccountWorkflowAction::Finished(AccountWorkflowOutcome::AccountRemoved {
                    account_id,
                })
            }
            AccountDatabaseCompletion::Failed(kind) => removal_pending_at(
                account_id,
                generation,
                AccountWorkflowStage::PurgeRemoval,
                AccountWorkflowFailureKind::Database(kind),
            ),
            AccountDatabaseCompletion::Saved(_)
            | AccountDatabaseCompletion::RemovalStarted(_)
            | AccountDatabaseCompletion::Purged(_)
            | AccountDatabaseCompletion::Unexpected => removal_pending_at(
                account_id,
                generation,
                AccountWorkflowStage::PurgeRemoval,
                AccountWorkflowFailureKind::UnexpectedReply,
            ),
        }
    }
}

enum ActiveWorkflow {
    Persisting {
        locator: CredentialLocator,
        secret: Secret,
    },
    Storing {
        configuration: AccountConfiguration,
    },
    BeginningRemoval {
        account_id: AccountId,
    },
    Deleting {
        ticket: AccountRemovalTicket,
    },
    Confirming {
        ticket: AccountRemovalTicket,
    },
    Purging {
        account_id: AccountId,
        generation: AccountGeneration,
    },
}

impl ActiveWorkflow {
    fn stage(&self) -> AccountWorkflowStage {
        match self {
            Self::Persisting { .. } => AccountWorkflowStage::PersistLocator,
            Self::Storing { .. } => AccountWorkflowStage::StoreCredential,
            Self::BeginningRemoval { .. } => AccountWorkflowStage::BeginRemoval,
            Self::Deleting { .. } => AccountWorkflowStage::DeleteCredential,
            Self::Confirming { .. } => AccountWorkflowStage::ConfirmRemoval,
            Self::Purging { .. } => AccountWorkflowStage::PurgeRemoval,
        }
    }

    fn expects_database(&self) -> bool {
        matches!(
            self,
            Self::Persisting { .. }
                | Self::BeginningRemoval { .. }
                | Self::Confirming { .. }
                | Self::Purging { .. }
        )
    }
}

fn persisted_credential_key(write: &AccountWrite) -> Option<&str> {
    match write {
        AccountWrite::Create(input) | AccountWrite::ConfigureExisting { input, .. } => {
            Some(&input.credential_key)
        }
        AccountWrite::Update { .. }
        | AccountWrite::SetEnabled { .. }
        | AccountWrite::BeginDiagnostic { .. }
        | AccountWrite::RecordDiagnostic { .. }
        | AccountWrite::BeginRemove { .. }
        | AccountWrite::ConfirmCredentialsRemoved { .. }
        | AccountWrite::PurgeRemovedAccount { .. } => None,
    }
}

fn account_write_kind(write: &AccountWrite) -> &'static str {
    match write {
        AccountWrite::Create(_) => "Create",
        AccountWrite::ConfigureExisting { .. } => "ConfigureExisting",
        AccountWrite::Update { .. } => "Update",
        AccountWrite::SetEnabled { .. } => "SetEnabled",
        AccountWrite::BeginDiagnostic { .. } => "BeginDiagnostic",
        AccountWrite::RecordDiagnostic { .. } => "RecordDiagnostic",
        AccountWrite::BeginRemove { .. } => "BeginRemove",
        AccountWrite::ConfirmCredentialsRemoved { .. } => "ConfirmCredentialsRemoved",
        AccountWrite::PurgeRemovedAccount { .. } => "PurgeRemovedAccount",
    }
}

fn complete_store(
    configuration: AccountConfiguration,
    completion: AccountCredentialCompletion,
) -> AccountWorkflowAction {
    match completion {
        AccountCredentialCompletion::Stored => {
            AccountWorkflowAction::Finished(AccountWorkflowOutcome::CredentialStored(configuration))
        }
        AccountCredentialCompletion::Failed(kind) => {
            AccountWorkflowAction::Finished(AccountWorkflowOutcome::CredentialPending {
                configuration,
                failure: AccountWorkflowFailureKind::Credential(kind),
            })
        }
        AccountCredentialCompletion::ReplyClosed => {
            AccountWorkflowAction::Finished(AccountWorkflowOutcome::CredentialPending {
                configuration,
                failure: AccountWorkflowFailureKind::CredentialReplyClosed,
            })
        }
        AccountCredentialCompletion::Deleted(_) | AccountCredentialCompletion::Unexpected => {
            AccountWorkflowAction::Finished(AccountWorkflowOutcome::CredentialPending {
                configuration,
                failure: AccountWorkflowFailureKind::UnexpectedReply,
            })
        }
    }
}

fn removal_pending(
    ticket: AccountRemovalTicket,
    stage: AccountWorkflowStage,
    failure: AccountWorkflowFailureKind,
) -> AccountWorkflowAction {
    removal_pending_at(ticket.account_id, ticket.generation, stage, failure)
}

fn removal_pending_at(
    account_id: AccountId,
    generation: AccountGeneration,
    stage: AccountWorkflowStage,
    failure: AccountWorkflowFailureKind,
) -> AccountWorkflowAction {
    AccountWorkflowAction::Finished(AccountWorkflowOutcome::RemovalPending {
        account_id,
        generation,
        stage,
        failure,
    })
}

fn failed(
    stage: AccountWorkflowStage,
    failure: AccountWorkflowFailureKind,
) -> AccountWorkflowAction {
    AccountWorkflowAction::Finished(AccountWorkflowOutcome::Failed { stage, failure })
}

pub(super) struct AccountWorkflows {
    database: DatabaseClient,
    credentials: CredentialClient,
    diagnostic_probe: ImapDiagnosticProbe,
    inbox_fetch_probe: ImapInboxFetchProbe,
    content_root: Option<std::path::PathBuf>,
    recovery: RecoveryState,
    active: Option<ActiveTask>,
    scheduler: SyncScheduler,
    auto_sync: AutoSyncState,
    background_status: Option<AccountSyncStatus>,
    preview_queue: VecDeque<(AccountId, AccountGeneration)>,
    history_queue: VecDeque<(AccountId, AccountGeneration)>,
    idle_watches: Vec<IdleWatch>,
}

impl AccountWorkflows {
    #[cfg(test)]
    pub(super) fn new(
        database: DatabaseClient,
        credentials: CredentialClient,
        diagnostic_probe: ImapDiagnosticProbe,
    ) -> Self {
        Self::with_sync(
            database,
            credentials,
            diagnostic_probe,
            production_imap_inbox_fetch,
            None,
            false,
        )
    }

    pub(super) fn with_sync(
        database: DatabaseClient,
        credentials: CredentialClient,
        diagnostic_probe: ImapDiagnosticProbe,
        inbox_fetch_probe: ImapInboxFetchProbe,
        content_root: Option<std::path::PathBuf>,
        auto_sync_enabled: bool,
    ) -> Self {
        Self {
            database,
            credentials,
            diagnostic_probe,
            inbox_fetch_probe,
            content_root,
            recovery: RecoveryState::SubmitCredentialScan,
            active: None,
            scheduler: SyncScheduler::new(),
            auto_sync: if auto_sync_enabled {
                AutoSyncState::SubmitTargets
            } else {
                AutoSyncState::Disabled
            },
            background_status: None,
            preview_queue: VecDeque::with_capacity(MAX_IDLE_WATCHES),
            history_queue: VecDeque::with_capacity(MAX_IDLE_WATCHES),
            idle_watches: Vec::with_capacity(MAX_IDLE_WATCHES),
        }
    }

    pub(super) fn take_background_status(&mut self) -> Option<AccountSyncStatus> {
        self.background_status.take()
    }

    pub(super) fn can_start_user_operation(&self) -> bool {
        matches!(self.recovery, RecoveryState::Complete)
            && (self.active.is_none()
                || self
                    .active
                    .as_ref()
                    .is_some_and(ActiveTask::is_preemptible_background))
    }

    pub(super) fn start_user_operation(
        &mut self,
        operation: AccountOperation,
        reply: oneshot::Sender<AccountOperationReply>,
    ) -> Result<(), AccountDriverError> {
        debug_assert!(self.can_start_user_operation());
        if self
            .active
            .as_ref()
            .is_some_and(ActiveTask::is_preemptible_background)
        {
            let interrupted = self
                .active
                .take()
                .expect("preemptible background sync is active");
            let interrupted_history = interrupted.reply.is_none()
                && interrupted.sync_token.is_none()
                && matches!(&interrupted.state, ActiveTaskState::Syncing(_));
            if matches!(
                &interrupted.state,
                ActiveTaskState::FetchingMessagePreviews(_)
            ) && let (Some(account_id), Some(generation)) = (
                interrupted.identity.account_id,
                interrupted.identity.generation,
            ) {
                self.queue_preview_fetch(account_id, generation);
            }
            if interrupted_history
                && let (Some(account_id), Some(generation)) = (
                    interrupted.identity.account_id,
                    interrupted.identity.generation,
                )
            {
                self.queue_history_fetch(account_id, generation);
            }
            if let Some(token) = interrupted.sync_token {
                let _ = self
                    .scheduler
                    .complete(token, SyncCompletion::Failed, Instant::now());
            }
        }
        let request_id = operation.request_id();
        if let Some((account_id, generation)) = foreground_identity(&operation) {
            self.cancel_idle_watch(account_id, generation);
        }
        match operation {
            AccountOperation::Setup {
                mode,
                draft,
                secret,
                ..
            } => {
                let locator = match CredentialLocator::generate() {
                    Ok(locator) => locator,
                    Err(failure) => {
                        let identity = setup_identity(mode);
                        let _ = reply.send(AccountOperationReply {
                            request_id,
                            result: Err(AccountOperationFailure {
                                account_id: identity.map(|identity| identity.0),
                                generation: identity.map(|identity| identity.1),
                                stage: AccountWorkflowStage::PersistLocator,
                                kind: AccountWorkflowFailureKind::Credential(failure.kind),
                            }),
                        });
                        return Ok(());
                    }
                };
                let input = draft.into_input(&locator);
                let (write, identity) = match mode {
                    AccountSetupMode::Create => (AccountWrite::Create(input), Identity::default()),
                    AccountSetupMode::ConfigureExisting {
                        account_id,
                        expected_generation,
                    } => (
                        AccountWrite::ConfigureExisting {
                            account_id,
                            expected_generation,
                            input,
                        },
                        Identity::new(account_id, expected_generation),
                    ),
                };
                self.active = Some(ActiveTask::start_workflow(
                    AccountWorkflowRequest::PersistAndStore {
                        write: Box::new(write),
                        secret,
                    },
                    identity,
                    Some(UserReply { request_id, reply }),
                    &self.database,
                    &self.credentials,
                )?);
            }
            AccountOperation::RetryCredential {
                account_id,
                expected_generation,
                secret,
                ..
            } => {
                let receiver = match self.database.try_load_account(account_id) {
                    Ok(receiver) => receiver,
                    Err(DatabaseSubmitError::Busy) => {
                        let _ = reply.send(AccountOperationReply {
                            request_id,
                            result: Err(AccountOperationFailure {
                                account_id: Some(account_id),
                                generation: Some(expected_generation),
                                stage: AccountWorkflowStage::LoadConfiguration,
                                kind: AccountWorkflowFailureKind::Busy,
                            }),
                        });
                        return Ok(());
                    }
                    Err(DatabaseSubmitError::Closed) => {
                        return Err(AccountDriverError::DatabaseClosed);
                    }
                };
                self.active = Some(ActiveTask {
                    identity: Identity::new(account_id, expected_generation),
                    reply: Some(UserReply { request_id, reply }),
                    sync_token: None,
                    state: ActiveTaskState::LoadingRetry {
                        account_id,
                        expected_generation,
                        secret: Some(secret),
                        receiver,
                    },
                });
            }
            AccountOperation::Diagnose {
                account_id,
                expected_generation,
                ..
            } => {
                let receiver = match self.database.try_load_account(account_id) {
                    Ok(receiver) => receiver,
                    Err(DatabaseSubmitError::Busy) => {
                        let _ = reply.send(AccountOperationReply {
                            request_id,
                            result: Err(AccountOperationFailure {
                                account_id: Some(account_id),
                                generation: Some(expected_generation),
                                stage: AccountWorkflowStage::LoadConfiguration,
                                kind: AccountWorkflowFailureKind::Busy,
                            }),
                        });
                        return Ok(());
                    }
                    Err(DatabaseSubmitError::Closed) => {
                        return Err(AccountDriverError::DatabaseClosed);
                    }
                };
                self.active = Some(ActiveTask {
                    identity: Identity::new(account_id, expected_generation),
                    reply: Some(UserReply { request_id, reply }),
                    sync_token: None,
                    state: ActiveTaskState::LoadingDiagnostic {
                        account_id,
                        expected_generation,
                        receiver,
                    },
                });
            }
            AccountOperation::SyncInbox {
                account_id,
                expected_generation,
                allow_history,
                ..
            } => {
                let Some(content_root) = self.content_root.clone() else {
                    let _ = reply.send(AccountOperationReply {
                        request_id,
                        result: Err(AccountOperationFailure {
                            account_id: Some(account_id),
                            generation: Some(expected_generation),
                            stage: AccountWorkflowStage::ParseContent,
                            kind: AccountWorkflowFailureKind::InboxSync(
                                InboxSyncFailureKind::Storage,
                            ),
                        }),
                    });
                    return Ok(());
                };
                let token = match self.scheduler.take_manual(
                    account_id,
                    expected_generation,
                    Instant::now(),
                ) {
                    Ok(Some(token)) => token,
                    Ok(None) => {
                        let _ = reply.send(AccountOperationReply {
                            request_id,
                            result: Err(AccountOperationFailure {
                                account_id: Some(account_id),
                                generation: Some(expected_generation),
                                stage: AccountWorkflowStage::FetchInbox,
                                kind: AccountWorkflowFailureKind::Busy,
                            }),
                        });
                        return Ok(());
                    }
                    Err(SchedulerError::TooManyTargets) => {
                        let _ = reply.send(AccountOperationReply {
                            request_id,
                            result: Err(AccountOperationFailure {
                                account_id: Some(account_id),
                                generation: Some(expected_generation),
                                stage: AccountWorkflowStage::FetchInbox,
                                kind: AccountWorkflowFailureKind::InboxSync(
                                    InboxSyncFailureKind::ResourceLimit,
                                ),
                            }),
                        });
                        return Ok(());
                    }
                    Err(SchedulerError::TokenExhausted) => {
                        return Err(AccountDriverError::WorkflowRejected);
                    }
                };
                self.active = Some(ActiveTask {
                    identity: Identity::new(account_id, expected_generation),
                    reply: Some(UserReply { request_id, reply }),
                    sync_token: Some(token),
                    state: ActiveTaskState::Syncing(start_inbox_sync(
                        self.database.clone(),
                        self.credentials.clone(),
                        self.inbox_fetch_probe,
                        content_root,
                        account_id,
                        expected_generation,
                        allow_history,
                    )),
                });
            }
            AccountOperation::FetchMessageContent {
                message_id,
                account_id,
                expected_generation,
                ..
            } => {
                let Some(content_root) = self.content_root.clone() else {
                    let _ = reply.send(AccountOperationReply {
                        request_id,
                        result: Err(AccountOperationFailure {
                            account_id: Some(account_id),
                            generation: Some(expected_generation),
                            stage: AccountWorkflowStage::ParseContent,
                            kind: AccountWorkflowFailureKind::InboxSync(
                                InboxSyncFailureKind::Storage,
                            ),
                        }),
                    });
                    return Ok(());
                };
                self.active = Some(ActiveTask {
                    identity: Identity::new(account_id, expected_generation),
                    reply: Some(UserReply { request_id, reply }),
                    sync_token: None,
                    state: ActiveTaskState::FetchingMessageContent(start_message_content_fetch(
                        self.database.clone(),
                        self.credentials.clone(),
                        content_root,
                        message_id,
                        account_id,
                        expected_generation,
                    )),
                });
            }
            AccountOperation::Remove {
                account_id,
                expected_generation,
                ..
            } => {
                let receiver = match self.database.try_load_account(account_id) {
                    Ok(receiver) => receiver,
                    Err(DatabaseSubmitError::Busy) => {
                        let _ = reply.send(AccountOperationReply {
                            request_id,
                            result: Err(AccountOperationFailure {
                                account_id: Some(account_id),
                                generation: Some(expected_generation),
                                stage: AccountWorkflowStage::LoadConfiguration,
                                kind: AccountWorkflowFailureKind::Busy,
                            }),
                        });
                        return Ok(());
                    }
                    Err(DatabaseSubmitError::Closed) => {
                        return Err(AccountDriverError::DatabaseClosed);
                    }
                };
                self.active = Some(ActiveTask {
                    identity: Identity::new(account_id, expected_generation),
                    reply: Some(UserReply { request_id, reply }),
                    sync_token: None,
                    state: ActiveTaskState::LoadingRemoval {
                        account_id,
                        expected_generation,
                        receiver,
                    },
                });
            }
        }
        Ok(())
    }

    pub(super) fn poll_progress(
        &mut self,
        context: &mut Context<'_>,
    ) -> Poll<Result<(), AccountDriverError>> {
        if self.poll_idle_watches(context) {
            return Poll::Ready(Ok(()));
        }
        if self.idle_handoff_pending() {
            return Poll::Pending;
        }
        if let Some(active) = self.active.as_mut() {
            match active.poll_one(
                &self.database,
                &self.credentials,
                self.diagnostic_probe,
                context,
            ) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Ok(TaskProgress::Advanced)) => return Poll::Ready(Ok(())),
                Poll::Ready(Ok(TaskProgress::Cancelled)) => {
                    let active = self.active.take().expect("active account task was polled");
                    if let Some(token) = active.sync_token {
                        let completed =
                            self.scheduler
                                .complete(token, SyncCompletion::Failed, Instant::now());
                        debug_assert!(completed, "cancelled sync token must still be current");
                    }
                    return Poll::Ready(Ok(()));
                }
                Poll::Ready(Ok(TaskProgress::Finished(outcome))) => {
                    let mut active = self.active.take().expect("active account task was polled");
                    let was_background_preview = active.reply.is_none()
                        && matches!(&active.state, ActiveTaskState::FetchingMessagePreviews(_));
                    let was_background_history = active.reply.is_none()
                        && active.sync_token.is_none()
                        && matches!(&active.state, ActiveTaskState::Syncing(_));
                    let reload_targets = outcome_changes_sync_targets(&outcome);
                    let preview_work = match &outcome {
                        AccountWorkflowOutcome::InboxSynced {
                            account_id,
                            generation,
                            ..
                        } => Some((*account_id, *generation, None)),
                        AccountWorkflowOutcome::MessagePreviewsFetched {
                            account_id,
                            generation,
                            updated,
                            has_more,
                        } => Some((*account_id, *generation, Some((*updated, *has_more)))),
                        _ => None,
                    };
                    let idle_watch = match &outcome {
                        AccountWorkflowOutcome::InboxSynced {
                            account_id,
                            generation,
                            historical,
                            has_more,
                            idle,
                            ..
                        } if !historical || !has_more => {
                            Some((*account_id, *generation, idle.clone()))
                        }
                        _ => None,
                    };
                    let history_work = match &outcome {
                        AccountWorkflowOutcome::InboxSynced {
                            account_id,
                            generation,
                            has_more: true,
                            ..
                        } => Some((*account_id, *generation)),
                        _ => None,
                    };
                    if let Some(token) = active.sync_token {
                        let completion = sync_completion(&outcome);
                        let completed = self.scheduler.complete(token, completion, Instant::now());
                        debug_assert!(completed, "finished sync token must still be current");
                        if active.reply.is_none() {
                            self.background_status =
                                Some(project_background_sync(outcome.clone(), active.identity));
                        }
                    }
                    if let Some((account_id, generation, preview_result)) = preview_work {
                        match preview_result {
                            None => self.queue_preview_fetch(account_id, generation),
                            Some((updated, has_more)) => {
                                if has_more {
                                    self.queue_preview_fetch(account_id, generation);
                                }
                                if updated > 0 {
                                    self.background_status =
                                        Some(AccountSyncStatus::PreviewsUpdated {
                                            account_id,
                                            generation,
                                            updated,
                                        });
                                }
                            }
                        }
                    }
                    if let Some((account_id, generation)) = history_work {
                        self.queue_history_fetch(account_id, generation);
                    }
                    if was_background_history {
                        self.background_status =
                            Some(project_background_sync(outcome.clone(), active.identity));
                    }
                    if was_background_preview
                        && matches!(&outcome, AccountWorkflowOutcome::Failed { .. })
                    {
                        self.background_status =
                            Some(project_background_sync(outcome.clone(), active.identity));
                    }
                    if let Some((account_id, generation, request)) = idle_watch {
                        self.register_idle_watch(account_id, generation, request);
                    }
                    if let Some(user) = active.reply.take() {
                        let _ = user.reply.send(AccountOperationReply {
                            request_id: user.request_id,
                            result: project_outcome(outcome, active.identity),
                        });
                    }
                    if reload_targets {
                        self.request_sync_target_reload();
                    }
                    return Poll::Ready(Ok(()));
                }
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
            }
        }

        match &mut self.recovery {
            RecoveryState::SubmitCredentialScan => {
                match self.database.try_load_pending_credential_removals() {
                    Ok(receiver) => {
                        self.recovery = RecoveryState::LoadingCredentialScan(receiver);
                        context.waker().wake_by_ref();
                        Poll::Ready(Ok(()))
                    }
                    Err(DatabaseSubmitError::Busy) => {
                        self.recovery = RecoveryState::WaitingCredentialSubmit(Box::pin(
                            time::sleep(RECOVERY_SUBMISSION_RETRY),
                        ));
                        Poll::Ready(Ok(()))
                    }
                    Err(DatabaseSubmitError::Closed) => {
                        Poll::Ready(Err(AccountDriverError::DatabaseClosed))
                    }
                }
            }
            RecoveryState::WaitingCredentialSubmit(delay) => match delay.as_mut().poll(context) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(()) => {
                    self.recovery = RecoveryState::SubmitCredentialScan;
                    context.waker().wake_by_ref();
                    Poll::Ready(Ok(()))
                }
            },
            RecoveryState::SubmitCacheScan(requests) => {
                match self.database.try_load_pending_cache_removals() {
                    Ok(receiver) => {
                        self.recovery = RecoveryState::LoadingCacheScan {
                            requests: std::mem::take(requests),
                            receiver,
                        };
                        context.waker().wake_by_ref();
                        Poll::Ready(Ok(()))
                    }
                    Err(DatabaseSubmitError::Busy) => {
                        self.recovery = RecoveryState::WaitingCacheSubmit {
                            requests: std::mem::take(requests),
                            delay: Box::pin(time::sleep(RECOVERY_SUBMISSION_RETRY)),
                        };
                        Poll::Ready(Ok(()))
                    }
                    Err(DatabaseSubmitError::Closed) => {
                        Poll::Ready(Err(AccountDriverError::DatabaseClosed))
                    }
                }
            }
            RecoveryState::WaitingCacheSubmit { requests, delay } => {
                match delay.as_mut().poll(context) {
                    Poll::Pending => Poll::Pending,
                    Poll::Ready(()) => {
                        self.recovery = RecoveryState::SubmitCacheScan(std::mem::take(requests));
                        context.waker().wake_by_ref();
                        Poll::Ready(Ok(()))
                    }
                }
            }
            RecoveryState::LoadingCredentialScan(receiver) => {
                let removals = match Pin::new(receiver).poll(context) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Ok(Ok(removals))) => removals,
                    Poll::Ready(Ok(Err(failure))) => {
                        return Poll::Ready(Err(AccountDriverError::Recovery(failure.kind)));
                    }
                    Poll::Ready(Err(_)) => {
                        return Poll::Ready(Err(AccountDriverError::DatabaseClosed));
                    }
                };
                let mut requests = VecDeque::with_capacity(removals.len());
                for removal in removals {
                    requests.push_back(AccountWorkflowRequest::ResumeRemove(
                        AccountRemovalTicket {
                            account_id: removal.account_id,
                            generation: removal.configuration_generation,
                            credential_key: Some(removal.credential_key),
                        },
                    ));
                }
                self.recovery = RecoveryState::SubmitCacheScan(requests);
                context.waker().wake_by_ref();
                Poll::Ready(Ok(()))
            }
            RecoveryState::LoadingCacheScan { requests, receiver } => {
                let removals = match Pin::new(receiver).poll(context) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Ok(Ok(removals))) => removals,
                    Poll::Ready(Ok(Err(failure))) => {
                        return Poll::Ready(Err(AccountDriverError::Recovery(failure.kind)));
                    }
                    Poll::Ready(Err(_)) => {
                        return Poll::Ready(Err(AccountDriverError::DatabaseClosed));
                    }
                };
                let mut queued = std::mem::take(requests);
                for removal in removals {
                    queued.push_back(AccountWorkflowRequest::ResumePurge {
                        account_id: removal.account_id,
                        expected_generation: removal.configuration_generation,
                    });
                }
                self.recovery = RecoveryState::Queued(queued);
                context.waker().wake_by_ref();
                Poll::Ready(Ok(()))
            }
            RecoveryState::Queued(requests) => {
                let Some(request) = requests.pop_front() else {
                    self.recovery = RecoveryState::Complete;
                    return Poll::Ready(Ok(()));
                };
                let identity = request_identity(&request);
                match ActiveTask::start_workflow(
                    request,
                    identity,
                    None,
                    &self.database,
                    &self.credentials,
                ) {
                    Ok(active) => self.active = Some(active),
                    Err(AccountDriverError::WorkflowRejected) => {}
                    Err(error) => return Poll::Ready(Err(error)),
                }
                context.waker().wake_by_ref();
                Poll::Ready(Ok(()))
            }
            RecoveryState::Complete => match self.poll_auto_sync(context) {
                ready @ Poll::Ready(_) => ready,
                Poll::Pending if self.start_next_preview_fetch() => {
                    context.waker().wake_by_ref();
                    Poll::Ready(Ok(()))
                }
                Poll::Pending if self.start_next_history_fetch() => {
                    context.waker().wake_by_ref();
                    Poll::Ready(Ok(()))
                }
                Poll::Pending => Poll::Pending,
            },
        }
    }

    fn queue_preview_fetch(&mut self, account_id: AccountId, generation: AccountGeneration) {
        if self
            .preview_queue
            .iter()
            .any(|queued| queued.0 == account_id && queued.1 == generation)
        {
            return;
        }
        if self.preview_queue.len() == MAX_IDLE_WATCHES {
            self.preview_queue.pop_back();
        }
        self.preview_queue.push_back((account_id, generation));
    }

    fn start_next_preview_fetch(&mut self) -> bool {
        let Some((account_id, generation)) = self.preview_queue.pop_front() else {
            return false;
        };
        self.active = Some(ActiveTask {
            identity: Identity::new(account_id, generation),
            reply: None,
            sync_token: None,
            state: ActiveTaskState::FetchingMessagePreviews(start_message_preview_fetch(
                self.database.clone(),
                self.credentials.clone(),
                account_id,
                generation,
            )),
        });
        true
    }

    fn queue_history_fetch(&mut self, account_id: AccountId, generation: AccountGeneration) {
        if self
            .history_queue
            .iter()
            .any(|queued| queued.0 == account_id && queued.1 == generation)
        {
            return;
        }
        if self.history_queue.len() == MAX_IDLE_WATCHES {
            self.history_queue.pop_back();
        }
        self.history_queue.push_back((account_id, generation));
    }

    fn start_next_history_fetch(&mut self) -> bool {
        let Some(content_root) = self.content_root.clone() else {
            return false;
        };
        let Some((account_id, generation)) = self.history_queue.pop_front() else {
            return false;
        };
        self.active = Some(ActiveTask {
            identity: Identity::new(account_id, generation),
            reply: None,
            sync_token: None,
            state: ActiveTaskState::Syncing(start_inbox_sync(
                self.database.clone(),
                self.credentials.clone(),
                self.inbox_fetch_probe,
                content_root,
                account_id,
                generation,
                true,
            )),
        });
        true
    }

    fn request_sync_target_reload(&mut self) {
        if !matches!(self.auto_sync, AutoSyncState::Disabled) {
            self.auto_sync = AutoSyncState::SubmitTargets;
        }
    }

    fn register_idle_watch(
        &mut self,
        account_id: AccountId,
        generation: AccountGeneration,
        request: ImapIdleRequest,
    ) {
        self.idle_watches
            .retain(|watch| watch.account_id != account_id || watch.generation != generation);
        if self.idle_watches.len() == MAX_IDLE_WATCHES {
            return;
        }
        let (cancellation, cancelled) = oneshot::channel();
        self.idle_watches.push(IdleWatch {
            account_id,
            generation,
            cancellation: Some(cancellation),
            future: Box::pin(wait_for_cached_inbox_change(request, cancelled)),
        });
    }

    fn cancel_idle_watch(&mut self, account_id: AccountId, generation: AccountGeneration) {
        if let Some(watch) = self
            .idle_watches
            .iter_mut()
            .find(|watch| watch.account_id == account_id && watch.generation == generation)
            && let Some(cancellation) = watch.cancellation.take()
        {
            let _ = cancellation.send(());
        }
    }

    fn idle_handoff_pending(&self) -> bool {
        self.idle_watches
            .iter()
            .any(|watch| watch.cancellation.is_none())
    }

    fn poll_idle_watches(&mut self, context: &mut Context<'_>) -> bool {
        let mut index = 0;
        while index < self.idle_watches.len() {
            let outcome = match self.idle_watches[index].future.as_mut().poll(context) {
                Poll::Pending => {
                    index += 1;
                    continue;
                }
                Poll::Ready(outcome) => outcome,
            };
            let watch = self.idle_watches.swap_remove(index);
            let now = Instant::now();
            let promotion_deadline = match outcome {
                ImapIdleOutcome::Changed => Some(now),
                ImapIdleOutcome::Disconnected(_) => now.checked_add(IDLE_RECONNECT_DELAY),
                ImapIdleOutcome::TimedOut
                | ImapIdleOutcome::Cancelled
                | ImapIdleOutcome::Unavailable => None,
            };
            if let Some(deadline) = promotion_deadline
                && self
                    .scheduler
                    .promote(watch.account_id, watch.generation, deadline)
                && matches!(self.auto_sync, AutoSyncState::WaitingForDeadline(_))
            {
                self.auto_sync = if deadline <= now {
                    AutoSyncState::Ready
                } else {
                    AutoSyncState::WaitingForDeadline(Box::pin(time::sleep_until(deadline.into())))
                };
            }
            context.waker().wake_by_ref();
            return true;
        }
        false
    }

    fn poll_auto_sync(
        &mut self,
        context: &mut Context<'_>,
    ) -> Poll<Result<(), AccountDriverError>> {
        match &mut self.auto_sync {
            AutoSyncState::Disabled => Poll::Pending,
            AutoSyncState::SubmitTargets => match self.database.try_load_sync_targets() {
                Ok(receiver) => {
                    self.auto_sync = AutoSyncState::LoadingTargets(receiver);
                    context.waker().wake_by_ref();
                    Poll::Ready(Ok(()))
                }
                Err(DatabaseSubmitError::Busy) => {
                    self.auto_sync = AutoSyncState::WaitingToSubmit(Box::pin(time::sleep(
                        SCHEDULER_SUBMISSION_RETRY,
                    )));
                    Poll::Ready(Ok(()))
                }
                Err(DatabaseSubmitError::Closed) => {
                    Poll::Ready(Err(AccountDriverError::DatabaseClosed))
                }
            },
            AutoSyncState::LoadingTargets(receiver) => {
                let targets = match Pin::new(receiver).poll(context) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Ok(Ok(targets))) => targets,
                    Poll::Ready(Ok(Err(failure))) if failure.kind == FailureKind::ResourceLimit => {
                        self.auto_sync = AutoSyncState::Paused;
                        self.background_status =
                            Some(AccountSyncStatus::Failed(AccountOperationFailure {
                                account_id: None,
                                generation: None,
                                stage: AccountWorkflowStage::LoadConfiguration,
                                kind: AccountWorkflowFailureKind::Database(
                                    FailureKind::ResourceLimit,
                                ),
                            }));
                        return Poll::Ready(Ok(()));
                    }
                    Poll::Ready(Ok(Err(failure))) => {
                        return Poll::Ready(Err(AccountDriverError::Recovery(failure.kind)));
                    }
                    Poll::Ready(Err(_)) => {
                        return Poll::Ready(Err(AccountDriverError::DatabaseClosed));
                    }
                };
                let targets = targets
                    .iter()
                    .map(|target| (target.account_id, target.generation))
                    .collect::<Vec<_>>();
                self.idle_watches.retain(|watch| {
                    targets.iter().any(|(account_id, generation)| {
                        watch.account_id == *account_id && watch.generation == *generation
                    })
                });
                self.scheduler
                    .replace_targets(targets, Instant::now())
                    .map_err(|_| AccountDriverError::WorkflowRejected)?;
                self.auto_sync = AutoSyncState::Ready;
                context.waker().wake_by_ref();
                Poll::Ready(Ok(()))
            }
            AutoSyncState::WaitingToSubmit(delay) => match delay.as_mut().poll(context) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(()) => {
                    self.auto_sync = AutoSyncState::SubmitTargets;
                    context.waker().wake_by_ref();
                    Poll::Ready(Ok(()))
                }
            },
            AutoSyncState::WaitingForDeadline(delay) => match delay.as_mut().poll(context) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(()) => {
                    self.auto_sync = AutoSyncState::Ready;
                    context.waker().wake_by_ref();
                    Poll::Ready(Ok(()))
                }
            },
            AutoSyncState::Paused => Poll::Pending,
            AutoSyncState::Ready => {
                let Some(content_root) = self.content_root.clone() else {
                    self.auto_sync = AutoSyncState::Disabled;
                    return Poll::Pending;
                };
                match self.scheduler.take_next(Instant::now()) {
                    Ok(Some(token)) => {
                        self.active = Some(ActiveTask {
                            identity: Identity::new(token.account_id(), token.generation()),
                            reply: None,
                            sync_token: Some(token),
                            state: ActiveTaskState::Syncing(start_inbox_sync(
                                self.database.clone(),
                                self.credentials.clone(),
                                self.inbox_fetch_probe,
                                content_root,
                                token.account_id(),
                                token.generation(),
                                false,
                            )),
                        });
                        Poll::Ready(Ok(()))
                    }
                    Ok(None) => {
                        if let Some(deadline) = self.scheduler.wake_deadline() {
                            self.auto_sync = AutoSyncState::WaitingForDeadline(Box::pin(
                                time::sleep_until(deadline.into()),
                            ));
                            Poll::Ready(Ok(()))
                        } else {
                            Poll::Pending
                        }
                    }
                    Err(_) => Poll::Ready(Err(AccountDriverError::WorkflowRejected)),
                }
            }
        }
    }
}

fn foreground_identity(operation: &AccountOperation) -> Option<(AccountId, AccountGeneration)> {
    match operation {
        AccountOperation::SyncInbox {
            account_id,
            expected_generation,
            ..
        }
        | AccountOperation::FetchMessageContent {
            account_id,
            expected_generation,
            ..
        } => Some((*account_id, *expected_generation)),
        _ => None,
    }
}

fn setup_identity(mode: AccountSetupMode) -> Option<(AccountId, AccountGeneration)> {
    match mode {
        AccountSetupMode::Create => None,
        AccountSetupMode::ConfigureExisting {
            account_id,
            expected_generation,
        } => Some((account_id, expected_generation)),
    }
}

enum RecoveryState {
    SubmitCredentialScan,
    WaitingCredentialSubmit(Pin<Box<Sleep>>),
    LoadingCredentialScan(
        oneshot::Receiver<Result<Box<[PendingCredentialRemoval]>, crate::store::sqlite::DbFailure>>,
    ),
    SubmitCacheScan(VecDeque<AccountWorkflowRequest>),
    WaitingCacheSubmit {
        requests: VecDeque<AccountWorkflowRequest>,
        delay: Pin<Box<Sleep>>,
    },
    LoadingCacheScan {
        requests: VecDeque<AccountWorkflowRequest>,
        receiver:
            oneshot::Receiver<Result<Box<[PendingCacheRemoval]>, crate::store::sqlite::DbFailure>>,
    },
    Queued(VecDeque<AccountWorkflowRequest>),
    Complete,
}

enum AutoSyncState {
    Disabled,
    SubmitTargets,
    LoadingTargets(
        oneshot::Receiver<Result<Box<[AccountSyncTarget]>, crate::store::sqlite::DbFailure>>,
    ),
    WaitingToSubmit(Pin<Box<Sleep>>),
    WaitingForDeadline(Pin<Box<Sleep>>),
    Paused,
    Ready,
}

struct IdleWatch {
    account_id: AccountId,
    generation: AccountGeneration,
    cancellation: Option<oneshot::Sender<()>>,
    future: ImapIdleFuture,
}

struct UserReply {
    request_id: RequestId,
    reply: oneshot::Sender<AccountOperationReply>,
}

#[derive(Clone, Copy, Debug, Default)]
struct Identity {
    account_id: Option<AccountId>,
    generation: Option<AccountGeneration>,
}

impl Identity {
    fn new(account_id: AccountId, generation: AccountGeneration) -> Self {
        Self {
            account_id: Some(account_id),
            generation: Some(generation),
        }
    }
}

struct ActiveTask {
    identity: Identity,
    reply: Option<UserReply>,
    sync_token: Option<SyncToken>,
    state: ActiveTaskState,
}

enum ActiveTaskState {
    Syncing(SyncInboxFuture),
    FetchingMessageContent(FetchMessageContentFuture),
    FetchingMessagePreviews(FetchMessagePreviewsFuture),
    LoadingRetry {
        account_id: AccountId,
        expected_generation: AccountGeneration,
        secret: Option<Secret>,
        receiver: oneshot::Receiver<Result<AccountRecord, crate::store::sqlite::DbFailure>>,
    },
    LoadingRemoval {
        account_id: AccountId,
        expected_generation: AccountGeneration,
        receiver: oneshot::Receiver<Result<AccountRecord, crate::store::sqlite::DbFailure>>,
    },
    LoadingDiagnostic {
        account_id: AccountId,
        expected_generation: AccountGeneration,
        receiver: oneshot::Receiver<Result<AccountRecord, crate::store::sqlite::DbFailure>>,
    },
    BeginningDiagnostic {
        configuration: Option<AccountConfiguration>,
        receiver: oneshot::Receiver<AccountWriteReply>,
    },
    LoadingDiagnosticCredential {
        configuration: Option<AccountConfiguration>,
        ticket: DiagnosticTicket,
        response: CredentialResponse,
    },
    ProbingDiagnostic {
        ticket: DiagnosticTicket,
        probe: ImapDiagnosticFuture,
    },
    RecordingDiagnostic {
        ticket: DiagnosticTicket,
        probe_result: Result<(), AccountDiagnosticKind>,
        receiver: oneshot::Receiver<AccountWriteReply>,
    },
    Workflow(Box<DrivenWorkflow>),
}

impl ActiveTask {
    fn is_preemptible_background(&self) -> bool {
        self.reply.is_none()
            && matches!(
                &self.state,
                ActiveTaskState::Syncing(_) | ActiveTaskState::FetchingMessagePreviews(_)
            )
    }

    fn start_workflow(
        request: AccountWorkflowRequest,
        identity: Identity,
        reply: Option<UserReply>,
        database: &DatabaseClient,
        credentials: &CredentialClient,
    ) -> Result<Self, AccountDriverError> {
        let workflow = DrivenWorkflow::start(request, database, credentials)?;
        Ok(Self {
            identity,
            reply,
            sync_token: None,
            state: ActiveTaskState::Workflow(Box::new(workflow)),
        })
    }

    fn poll_one(
        &mut self,
        database: &DatabaseClient,
        credentials: &CredentialClient,
        diagnostic_probe: ImapDiagnosticProbe,
        context: &mut Context<'_>,
    ) -> Poll<Result<TaskProgress, AccountDriverError>> {
        if matches!(
            self.state,
            ActiveTaskState::ProbingDiagnostic { .. }
                | ActiveTaskState::Syncing(_)
                | ActiveTaskState::FetchingMessageContent(_)
                | ActiveTaskState::FetchingMessagePreviews(_)
        ) && let Some(user) = self.reply.as_mut()
            && user.reply.poll_closed(context).is_ready()
        {
            return Poll::Ready(Ok(TaskProgress::Cancelled));
        }

        match &mut self.state {
            ActiveTaskState::Syncing(sync) => match sync.as_mut().poll(context) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(SyncInboxOutcome::Synced {
                    account_id,
                    generation,
                    imported,
                    has_more,
                    historical,
                    bootstrap,
                    idle,
                }) => Poll::Ready(Ok(TaskProgress::Finished(
                    AccountWorkflowOutcome::InboxSynced {
                        account_id,
                        generation,
                        imported,
                        has_more,
                        historical,
                        bootstrap,
                        idle,
                    },
                ))),
                Poll::Ready(SyncInboxOutcome::Failed { stage, failure }) => {
                    Poll::Ready(Ok(TaskProgress::Finished(AccountWorkflowOutcome::Failed {
                        stage,
                        failure,
                    })))
                }
                Poll::Ready(SyncInboxOutcome::DatabaseClosed) => {
                    Poll::Ready(Err(AccountDriverError::DatabaseClosed))
                }
            },
            ActiveTaskState::FetchingMessageContent(fetch) => match fetch.as_mut().poll(context) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(FetchMessageContentOutcome::Fetched {
                    account_id,
                    generation,
                    message_id,
                }) => Poll::Ready(Ok(TaskProgress::Finished(
                    AccountWorkflowOutcome::MessageContentFetched {
                        message_id,
                        account_id,
                        generation,
                    },
                ))),
                Poll::Ready(FetchMessageContentOutcome::Failed { stage, failure }) => {
                    Poll::Ready(Ok(TaskProgress::Finished(AccountWorkflowOutcome::Failed {
                        stage,
                        failure,
                    })))
                }
                Poll::Ready(FetchMessageContentOutcome::DatabaseClosed) => {
                    Poll::Ready(Err(AccountDriverError::DatabaseClosed))
                }
            },
            ActiveTaskState::FetchingMessagePreviews(fetch) => match fetch.as_mut().poll(context) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(FetchMessagePreviewsOutcome::Fetched {
                    account_id,
                    generation,
                    updated,
                    has_more,
                }) => Poll::Ready(Ok(TaskProgress::Finished(
                    AccountWorkflowOutcome::MessagePreviewsFetched {
                        account_id,
                        generation,
                        updated,
                        has_more,
                    },
                ))),
                Poll::Ready(FetchMessagePreviewsOutcome::Failed { stage, failure }) => {
                    Poll::Ready(Ok(TaskProgress::Finished(AccountWorkflowOutcome::Failed {
                        stage,
                        failure,
                    })))
                }
                Poll::Ready(FetchMessagePreviewsOutcome::DatabaseClosed) => {
                    Poll::Ready(Err(AccountDriverError::DatabaseClosed))
                }
            },
            ActiveTaskState::LoadingRetry {
                account_id,
                expected_generation,
                secret,
                receiver,
            } => {
                let record = match Pin::new(receiver).poll(context) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Ok(Ok(record))) => record,
                    Poll::Ready(Ok(Err(failure))) => {
                        return Poll::Ready(Ok(TaskProgress::Finished(
                            AccountWorkflowOutcome::Failed {
                                stage: AccountWorkflowStage::LoadConfiguration,
                                failure: AccountWorkflowFailureKind::Database(failure.kind),
                            },
                        )));
                    }
                    Poll::Ready(Err(_)) => {
                        return Poll::Ready(Err(AccountDriverError::DatabaseClosed));
                    }
                };
                let AccountRecord::Configured(configuration) = record else {
                    return Poll::Ready(Ok(TaskProgress::Finished(
                        AccountWorkflowOutcome::Failed {
                            stage: AccountWorkflowStage::LoadConfiguration,
                            failure: AccountWorkflowFailureKind::Database(FailureKind::Conflict),
                        },
                    )));
                };
                if configuration.account_id != *account_id
                    || configuration.generation != *expected_generation
                    || configuration.auth_kind != AccountAuthKind::AppPassword
                    || !matches!(
                        configuration.lifecycle,
                        AccountLifecycle::Active | AccountLifecycle::Disabled
                    )
                {
                    return Poll::Ready(Ok(TaskProgress::Finished(
                        AccountWorkflowOutcome::Failed {
                            stage: AccountWorkflowStage::LoadConfiguration,
                            failure: AccountWorkflowFailureKind::Database(FailureKind::Conflict),
                        },
                    )));
                }
                self.identity = Identity::new(configuration.account_id, configuration.generation);
                let secret = secret.take().expect("retry secret is consumed once");
                let workflow = DrivenWorkflow::start(
                    AccountWorkflowRequest::RetryStore {
                        configuration,
                        secret,
                    },
                    database,
                    credentials,
                )?;
                self.state = ActiveTaskState::Workflow(Box::new(workflow));
                context.waker().wake_by_ref();
                Poll::Ready(Ok(TaskProgress::Advanced))
            }
            ActiveTaskState::LoadingRemoval {
                account_id,
                expected_generation,
                receiver,
            } => {
                let record = match Pin::new(receiver).poll(context) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Ok(Ok(record))) => record,
                    Poll::Ready(Ok(Err(failure))) => {
                        return Poll::Ready(Ok(TaskProgress::Finished(
                            AccountWorkflowOutcome::Failed {
                                stage: AccountWorkflowStage::LoadConfiguration,
                                failure: AccountWorkflowFailureKind::Database(failure.kind),
                            },
                        )));
                    }
                    Poll::Ready(Err(_)) => {
                        return Poll::Ready(Err(AccountDriverError::DatabaseClosed));
                    }
                };
                let request = match record {
                    AccountRecord::Configured(configuration)
                        if configuration.account_id == *account_id
                            && configuration.generation == *expected_generation =>
                    {
                        match configuration.lifecycle {
                            AccountLifecycle::Active | AccountLifecycle::Disabled => {
                                AccountWorkflowRequest::BeginRemove {
                                    account_id: *account_id,
                                    expected_generation: *expected_generation,
                                }
                            }
                            AccountLifecycle::RemovingCredentials => {
                                AccountWorkflowRequest::ResumeRemove(AccountRemovalTicket {
                                    account_id: configuration.account_id,
                                    generation: configuration.generation,
                                    credential_key: Some(configuration.credential_key),
                                })
                            }
                            AccountLifecycle::RemovingCache => {
                                AccountWorkflowRequest::ResumePurge {
                                    account_id: configuration.account_id,
                                    expected_generation: configuration.generation,
                                }
                            }
                        }
                    }
                    AccountRecord::NeedsSetup(target)
                        if target.account_id == *account_id
                            && target.generation == *expected_generation =>
                    {
                        if target.removal_pending {
                            AccountWorkflowRequest::ResumePurge {
                                account_id: target.account_id,
                                expected_generation: target.generation,
                            }
                        } else {
                            AccountWorkflowRequest::BeginRemove {
                                account_id: target.account_id,
                                expected_generation: target.generation,
                            }
                        }
                    }
                    AccountRecord::Configured(_) | AccountRecord::NeedsSetup(_) => {
                        return Poll::Ready(Ok(TaskProgress::Finished(
                            AccountWorkflowOutcome::Failed {
                                stage: AccountWorkflowStage::LoadConfiguration,
                                failure: AccountWorkflowFailureKind::Database(
                                    FailureKind::Conflict,
                                ),
                            },
                        )));
                    }
                };
                let workflow = DrivenWorkflow::start(request, database, credentials)?;
                self.state = ActiveTaskState::Workflow(Box::new(workflow));
                context.waker().wake_by_ref();
                Poll::Ready(Ok(TaskProgress::Advanced))
            }
            ActiveTaskState::LoadingDiagnostic {
                account_id,
                expected_generation,
                receiver,
            } => {
                let record = match Pin::new(receiver).poll(context) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Ok(Ok(record))) => record,
                    Poll::Ready(Ok(Err(failure))) => {
                        return Poll::Ready(Ok(TaskProgress::Finished(
                            AccountWorkflowOutcome::Failed {
                                stage: AccountWorkflowStage::LoadConfiguration,
                                failure: AccountWorkflowFailureKind::Database(failure.kind),
                            },
                        )));
                    }
                    Poll::Ready(Err(_)) => {
                        return Poll::Ready(Err(AccountDriverError::DatabaseClosed));
                    }
                };
                let AccountRecord::Configured(configuration) = record else {
                    return Poll::Ready(Ok(TaskProgress::Finished(
                        AccountWorkflowOutcome::Failed {
                            stage: AccountWorkflowStage::LoadConfiguration,
                            failure: AccountWorkflowFailureKind::Database(FailureKind::Conflict),
                        },
                    )));
                };
                if configuration.account_id != *account_id
                    || configuration.generation != *expected_generation
                    || configuration.auth_kind != AccountAuthKind::AppPassword
                    || configuration.lifecycle != AccountLifecycle::Active
                {
                    return Poll::Ready(Ok(TaskProgress::Finished(
                        AccountWorkflowOutcome::Failed {
                            stage: AccountWorkflowStage::LoadConfiguration,
                            failure: AccountWorkflowFailureKind::Database(FailureKind::Conflict),
                        },
                    )));
                }
                let receiver =
                    match database.try_write_account(Box::new(AccountWrite::BeginDiagnostic {
                        account_id: *account_id,
                        expected_generation: *expected_generation,
                    })) {
                        Ok(receiver) => receiver,
                        Err(failure) if failure.reason() == DatabaseSubmitError::Busy => {
                            return Poll::Ready(Ok(TaskProgress::Finished(
                                AccountWorkflowOutcome::Failed {
                                    stage: AccountWorkflowStage::BeginDiagnostic,
                                    failure: AccountWorkflowFailureKind::Busy,
                                },
                            )));
                        }
                        Err(_) => return Poll::Ready(Err(AccountDriverError::DatabaseClosed)),
                    };
                self.state = ActiveTaskState::BeginningDiagnostic {
                    configuration: Some(configuration),
                    receiver,
                };
                context.waker().wake_by_ref();
                Poll::Ready(Ok(TaskProgress::Advanced))
            }
            ActiveTaskState::BeginningDiagnostic {
                configuration,
                receiver,
            } => {
                let ticket = match Pin::new(receiver).poll(context) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Ok(Ok(AccountWriteOutcome::DiagnosticStarted(ticket)))) => ticket,
                    Poll::Ready(Ok(Ok(_))) => {
                        return Poll::Ready(Ok(TaskProgress::Finished(
                            AccountWorkflowOutcome::Failed {
                                stage: AccountWorkflowStage::BeginDiagnostic,
                                failure: AccountWorkflowFailureKind::UnexpectedReply,
                            },
                        )));
                    }
                    Poll::Ready(Ok(Err(failure))) => {
                        return Poll::Ready(Ok(TaskProgress::Finished(
                            AccountWorkflowOutcome::Failed {
                                stage: AccountWorkflowStage::BeginDiagnostic,
                                failure: AccountWorkflowFailureKind::Database(
                                    failure.failure().kind,
                                ),
                            },
                        )));
                    }
                    Poll::Ready(Err(_)) => {
                        return Poll::Ready(Err(AccountDriverError::DatabaseClosed));
                    }
                };
                let configuration = configuration
                    .take()
                    .expect("diagnostic configuration is consumed once");
                if ticket.account_id != configuration.account_id
                    || ticket.configuration_generation != configuration.generation
                {
                    return Poll::Ready(Ok(TaskProgress::Finished(
                        AccountWorkflowOutcome::Failed {
                            stage: AccountWorkflowStage::BeginDiagnostic,
                            failure: AccountWorkflowFailureKind::UnexpectedReply,
                        },
                    )));
                }
                let locator = match CredentialLocator::parse(&configuration.credential_key) {
                    Ok(locator) => locator,
                    Err(_) => {
                        return Poll::Ready(Ok(TaskProgress::Finished(
                            AccountWorkflowOutcome::Failed {
                                stage: AccountWorkflowStage::LoadCredential,
                                failure: AccountWorkflowFailureKind::InvalidLocator,
                            },
                        )));
                    }
                };
                let response = match credentials.try_submit(CredentialOperation::Load { locator }) {
                    Ok(response) => response,
                    Err(failure) if failure.reason() == CredentialSubmitError::Busy => {
                        return Poll::Ready(Ok(TaskProgress::Finished(
                            AccountWorkflowOutcome::Failed {
                                stage: AccountWorkflowStage::LoadCredential,
                                failure: AccountWorkflowFailureKind::Busy,
                            },
                        )));
                    }
                    Err(_) => {
                        return Poll::Ready(Ok(TaskProgress::Finished(
                            AccountWorkflowOutcome::Failed {
                                stage: AccountWorkflowStage::LoadCredential,
                                failure: AccountWorkflowFailureKind::CredentialReplyClosed,
                            },
                        )));
                    }
                };
                self.state = ActiveTaskState::LoadingDiagnosticCredential {
                    configuration: Some(configuration),
                    ticket,
                    response,
                };
                context.waker().wake_by_ref();
                Poll::Ready(Ok(TaskProgress::Advanced))
            }
            ActiveTaskState::LoadingDiagnosticCredential {
                configuration,
                ticket,
                response,
            } => {
                let secret = match Pin::new(response).poll(context) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Ok(Ok(CredentialOutcome::Loaded(secret)))) => secret,
                    Poll::Ready(Ok(Ok(_))) => {
                        return Poll::Ready(Ok(TaskProgress::Finished(
                            AccountWorkflowOutcome::Failed {
                                stage: AccountWorkflowStage::LoadCredential,
                                failure: AccountWorkflowFailureKind::UnexpectedReply,
                            },
                        )));
                    }
                    Poll::Ready(Ok(Err(failure))) => {
                        return Poll::Ready(Ok(TaskProgress::Finished(
                            AccountWorkflowOutcome::Failed {
                                stage: AccountWorkflowStage::LoadCredential,
                                failure: AccountWorkflowFailureKind::Credential(failure.kind),
                            },
                        )));
                    }
                    Poll::Ready(Err(_)) => {
                        return Poll::Ready(Ok(TaskProgress::Finished(
                            AccountWorkflowOutcome::Failed {
                                stage: AccountWorkflowStage::LoadCredential,
                                failure: AccountWorkflowFailureKind::CredentialReplyClosed,
                            },
                        )));
                    }
                };
                let configuration = configuration
                    .take()
                    .expect("diagnostic configuration is consumed once");
                let request = match ImapDiagnosticRequest::new(
                    &configuration.imap_host,
                    configuration.imap_port,
                    &configuration.login_name,
                    secret,
                ) {
                    Ok(request) => request,
                    Err(_) => {
                        let result = Err(AccountDiagnosticKind::Protocol);
                        let next = match start_diagnostic_record(database, *ticket, result) {
                            Ok(next) => next,
                            Err(DatabaseSubmitError::Busy) => {
                                return Poll::Ready(Ok(TaskProgress::Finished(
                                    AccountWorkflowOutcome::Failed {
                                        stage: AccountWorkflowStage::RecordDiagnostic,
                                        failure: AccountWorkflowFailureKind::Busy,
                                    },
                                )));
                            }
                            Err(DatabaseSubmitError::Closed) => {
                                return Poll::Ready(Err(AccountDriverError::DatabaseClosed));
                            }
                        };
                        self.state = next;
                        context.waker().wake_by_ref();
                        return Poll::Ready(Ok(TaskProgress::Advanced));
                    }
                };
                self.state = ActiveTaskState::ProbingDiagnostic {
                    ticket: *ticket,
                    probe: diagnostic_probe(request),
                };
                context.waker().wake_by_ref();
                Poll::Ready(Ok(TaskProgress::Advanced))
            }
            ActiveTaskState::ProbingDiagnostic { ticket, probe } => {
                let result = match probe.as_mut().poll(context) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Ok(())) => Ok(()),
                    Poll::Ready(Err(failure)) => Err(map_imap_failure(failure)),
                };
                if self
                    .reply
                    .as_ref()
                    .is_some_and(|user| user.reply.is_closed())
                {
                    return Poll::Ready(Ok(TaskProgress::Cancelled));
                }
                let next = match start_diagnostic_record(database, *ticket, result) {
                    Ok(next) => next,
                    Err(DatabaseSubmitError::Busy) => {
                        return Poll::Ready(Ok(TaskProgress::Finished(
                            AccountWorkflowOutcome::Failed {
                                stage: AccountWorkflowStage::RecordDiagnostic,
                                failure: AccountWorkflowFailureKind::Busy,
                            },
                        )));
                    }
                    Err(DatabaseSubmitError::Closed) => {
                        return Poll::Ready(Err(AccountDriverError::DatabaseClosed));
                    }
                };
                self.state = next;
                context.waker().wake_by_ref();
                Poll::Ready(Ok(TaskProgress::Advanced))
            }
            ActiveTaskState::RecordingDiagnostic {
                ticket,
                probe_result,
                receiver,
            } => {
                let commit = match Pin::new(receiver).poll(context) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Ok(Ok(AccountWriteOutcome::Diagnostic(commit)))) => commit,
                    Poll::Ready(Ok(Ok(_))) => {
                        return Poll::Ready(Ok(TaskProgress::Finished(
                            AccountWorkflowOutcome::Failed {
                                stage: AccountWorkflowStage::RecordDiagnostic,
                                failure: AccountWorkflowFailureKind::UnexpectedReply,
                            },
                        )));
                    }
                    Poll::Ready(Ok(Err(failure))) => {
                        return Poll::Ready(Ok(TaskProgress::Finished(
                            AccountWorkflowOutcome::Failed {
                                stage: AccountWorkflowStage::RecordDiagnostic,
                                failure: AccountWorkflowFailureKind::Database(
                                    failure.failure().kind,
                                ),
                            },
                        )));
                    }
                    Poll::Ready(Err(_)) => {
                        return Poll::Ready(Err(AccountDriverError::DatabaseClosed));
                    }
                };
                match commit {
                    DiagnosticCommit::Recorded => Poll::Ready(Ok(TaskProgress::Finished(
                        AccountWorkflowOutcome::Diagnostic {
                            account_id: ticket.account_id,
                            generation: ticket.configuration_generation,
                            result: *probe_result,
                        },
                    ))),
                    DiagnosticCommit::Stale => {
                        Poll::Ready(Ok(TaskProgress::Finished(AccountWorkflowOutcome::Failed {
                            stage: AccountWorkflowStage::RecordDiagnostic,
                            failure: AccountWorkflowFailureKind::Database(FailureKind::Conflict),
                        })))
                    }
                }
            }
            ActiveTaskState::Workflow(workflow) => {
                let progress = workflow.poll_one(database, credentials, context);
                if let Poll::Ready(Ok(TaskProgress::Advanced)) = &progress
                    && let Some(identity) = workflow.identity()
                {
                    self.identity = identity;
                }
                progress
            }
        }
    }
}

fn start_diagnostic_record(
    database: &DatabaseClient,
    ticket: DiagnosticTicket,
    probe_result: Result<(), AccountDiagnosticKind>,
) -> Result<ActiveTaskState, DatabaseSubmitError> {
    let checked_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(0);
    let record = match probe_result {
        Ok(()) => DiagnosticRecord::ready(checked_at_ms),
        Err(kind) => DiagnosticRecord::failed(kind, checked_at_ms),
    }
    .expect("the bounded current timestamp is valid");
    let receiver = database
        .try_write_account(Box::new(AccountWrite::RecordDiagnostic {
            account_id: ticket.account_id,
            expected_generation: ticket.configuration_generation,
            epoch: ticket.epoch,
            record,
        }))
        .map_err(|failure| failure.reason())?;
    Ok(ActiveTaskState::RecordingDiagnostic {
        ticket,
        probe_result,
        receiver,
    })
}

fn map_imap_failure(failure: ImapDiagnosticFailure) -> AccountDiagnosticKind {
    match failure.kind {
        ImapDiagnosticFailureKind::Authentication => AccountDiagnosticKind::Authentication,
        ImapDiagnosticFailureKind::Permission => AccountDiagnosticKind::Permission,
        ImapDiagnosticFailureKind::Certificate => AccountDiagnosticKind::Certificate,
        ImapDiagnosticFailureKind::Timeout => AccountDiagnosticKind::Timeout,
        ImapDiagnosticFailureKind::Offline => AccountDiagnosticKind::Offline,
        ImapDiagnosticFailureKind::Protocol => AccountDiagnosticKind::Protocol,
    }
}

enum TaskProgress {
    Advanced,
    Cancelled,
    Finished(AccountWorkflowOutcome),
}

struct DrivenWorkflow {
    coordinator: AccountCoordinator,
    pending: PendingAction,
    identity: Identity,
}

enum PendingAction {
    Database(oneshot::Receiver<AccountWriteReply>),
    Credential(CredentialResponse),
    Finished(Option<AccountWorkflowOutcome>),
}

impl DrivenWorkflow {
    fn start(
        request: AccountWorkflowRequest,
        database: &DatabaseClient,
        credentials: &CredentialClient,
    ) -> Result<Self, AccountDriverError> {
        let identity = request_identity(&request);
        let mut coordinator = AccountCoordinator::default();
        let action = coordinator
            .try_start(request)
            .map_err(|_| AccountDriverError::WorkflowRejected)?;
        let pending = dispatch_action(action, database, credentials)?;
        Ok(Self {
            coordinator,
            pending,
            identity,
        })
    }

    fn identity(&self) -> Option<Identity> {
        self.identity.account_id.map(|_| self.identity)
    }

    fn poll_one(
        &mut self,
        database: &DatabaseClient,
        credentials: &CredentialClient,
        context: &mut Context<'_>,
    ) -> Poll<Result<TaskProgress, AccountDriverError>> {
        let action = match &mut self.pending {
            PendingAction::Database(receiver) => {
                let completion = match Pin::new(receiver).poll(context) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Ok(reply)) => map_database_reply(reply, &mut self.identity),
                    Poll::Ready(Err(_)) => {
                        return Poll::Ready(Err(AccountDriverError::DatabaseClosed));
                    }
                };
                self.coordinator
                    .database_completed(completion)
                    .map_err(|_| AccountDriverError::WorkflowRejected)?
            }
            PendingAction::Credential(response) => {
                let completion = match Pin::new(response).poll(context) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Ok(reply)) => map_credential_reply(reply),
                    Poll::Ready(Err(_)) => AccountCredentialCompletion::ReplyClosed,
                };
                self.coordinator
                    .credential_completed(completion)
                    .map_err(|_| AccountDriverError::WorkflowRejected)?
            }
            PendingAction::Finished(outcome) => {
                return Poll::Ready(Ok(TaskProgress::Finished(
                    outcome.take().expect("finished workflow is consumed once"),
                )));
            }
        };

        match action {
            AccountWorkflowAction::Finished(outcome) => {
                Poll::Ready(Ok(TaskProgress::Finished(outcome)))
            }
            action => {
                self.pending = dispatch_action(action, database, credentials)?;
                context.waker().wake_by_ref();
                Poll::Ready(Ok(TaskProgress::Advanced))
            }
        }
    }
}

fn dispatch_action(
    action: AccountWorkflowAction,
    database: &DatabaseClient,
    credentials: &CredentialClient,
) -> Result<PendingAction, AccountDriverError> {
    match action {
        AccountWorkflowAction::Database { stage, write } => {
            match database.try_write_account(write) {
                Ok(receiver) => Ok(PendingAction::Database(receiver)),
                Err(failure) if failure.reason() == DatabaseSubmitError::Busy => Ok(
                    PendingAction::Finished(Some(AccountWorkflowOutcome::Failed {
                        stage,
                        failure: AccountWorkflowFailureKind::Busy,
                    })),
                ),
                Err(_) => Err(AccountDriverError::DatabaseClosed),
            }
        }
        AccountWorkflowAction::Credential { stage, operation } => {
            match credentials.try_submit(operation) {
                Ok(response) => Ok(PendingAction::Credential(response)),
                Err(failure) if failure.reason() == CredentialSubmitError::Busy => Ok(
                    PendingAction::Finished(Some(AccountWorkflowOutcome::Failed {
                        stage,
                        failure: AccountWorkflowFailureKind::Busy,
                    })),
                ),
                Err(_) => Ok(PendingAction::Finished(Some(
                    AccountWorkflowOutcome::Failed {
                        stage,
                        failure: AccountWorkflowFailureKind::CredentialReplyClosed,
                    },
                ))),
            }
        }
        AccountWorkflowAction::Finished(outcome) => Ok(PendingAction::Finished(Some(outcome))),
    }
}

fn map_database_reply(
    reply: AccountWriteReply,
    identity: &mut Identity,
) -> AccountDatabaseCompletion {
    match reply {
        Ok(AccountWriteOutcome::Saved(configuration)) => {
            *identity = Identity::new(configuration.account_id, configuration.generation);
            AccountDatabaseCompletion::Saved(configuration)
        }
        Ok(AccountWriteOutcome::RemovalStarted(ticket)) => {
            *identity = Identity::new(ticket.account_id, ticket.generation);
            AccountDatabaseCompletion::RemovalStarted(ticket)
        }
        Ok(AccountWriteOutcome::Purged(outcome)) => AccountDatabaseCompletion::Purged(outcome),
        Ok(AccountWriteOutcome::DiagnosticStarted(_) | AccountWriteOutcome::Diagnostic(_)) => {
            AccountDatabaseCompletion::Unexpected
        }
        Err(failure) => AccountDatabaseCompletion::Failed(failure.failure().kind),
    }
}

fn map_credential_reply(
    reply: Result<CredentialOutcome, crate::credentials::CredentialFailure>,
) -> AccountCredentialCompletion {
    match reply {
        Ok(CredentialOutcome::Stored) => AccountCredentialCompletion::Stored,
        Ok(CredentialOutcome::Deleted(outcome)) => AccountCredentialCompletion::Deleted(outcome),
        Ok(CredentialOutcome::Loaded(_)) => AccountCredentialCompletion::Unexpected,
        Err(failure) => AccountCredentialCompletion::Failed(failure.kind),
    }
}

fn request_identity(request: &AccountWorkflowRequest) -> Identity {
    match request {
        AccountWorkflowRequest::PersistAndStore { .. } => Identity::default(),
        AccountWorkflowRequest::RetryStore { configuration, .. } => {
            Identity::new(configuration.account_id, configuration.generation)
        }
        AccountWorkflowRequest::BeginRemove {
            account_id,
            expected_generation,
        }
        | AccountWorkflowRequest::ResumePurge {
            account_id,
            expected_generation,
        } => Identity::new(*account_id, *expected_generation),
        AccountWorkflowRequest::ResumeRemove(ticket) => {
            Identity::new(ticket.account_id, ticket.generation)
        }
    }
}

fn outcome_changes_sync_targets(outcome: &AccountWorkflowOutcome) -> bool {
    matches!(
        outcome,
        AccountWorkflowOutcome::CredentialStored(_)
            | AccountWorkflowOutcome::CredentialPending { .. }
            | AccountWorkflowOutcome::AccountRemoved { .. }
            | AccountWorkflowOutcome::RemovalPending { .. }
    )
}

fn sync_completion(outcome: &AccountWorkflowOutcome) -> SyncCompletion {
    match outcome {
        AccountWorkflowOutcome::InboxSynced { .. } => SyncCompletion::Complete,
        _ => SyncCompletion::Failed,
    }
}

fn project_background_sync(
    outcome: AccountWorkflowOutcome,
    identity: Identity,
) -> AccountSyncStatus {
    match project_outcome(outcome, identity) {
        Ok(AccountOperationSuccess::Synced {
            account_id,
            generation,
            imported,
            has_more,
            historical,
        }) => AccountSyncStatus::Synced {
            account_id,
            generation,
            imported,
            has_more,
            historical,
        },
        Err(failure) => AccountSyncStatus::Failed(failure),
        Ok(_) => AccountSyncStatus::Failed(AccountOperationFailure {
            account_id: identity.account_id,
            generation: identity.generation,
            stage: AccountWorkflowStage::FetchInbox,
            kind: AccountWorkflowFailureKind::UnexpectedReply,
        }),
    }
}

fn project_outcome(
    outcome: AccountWorkflowOutcome,
    identity: Identity,
) -> Result<AccountOperationSuccess, AccountOperationFailure> {
    match outcome {
        AccountWorkflowOutcome::CredentialStored(configuration) => {
            Ok(AccountOperationSuccess::Configured {
                account_id: configuration.account_id,
                generation: configuration.generation,
            })
        }
        AccountWorkflowOutcome::CredentialPending {
            configuration,
            failure,
        } => Err(AccountOperationFailure {
            account_id: Some(configuration.account_id),
            generation: Some(configuration.generation),
            stage: AccountWorkflowStage::StoreCredential,
            kind: failure,
        }),
        AccountWorkflowOutcome::AccountRemoved { account_id } => {
            Ok(AccountOperationSuccess::Removed { account_id })
        }
        AccountWorkflowOutcome::Diagnostic {
            account_id,
            generation,
            result: Ok(()),
        } => Ok(AccountOperationSuccess::Diagnosed {
            account_id,
            generation,
        }),
        AccountWorkflowOutcome::Diagnostic {
            account_id,
            generation,
            result: Err(kind),
        } => Err(AccountOperationFailure {
            account_id: Some(account_id),
            generation: Some(generation),
            stage: AccountWorkflowStage::ConnectImap,
            kind: AccountWorkflowFailureKind::Diagnostic(kind),
        }),
        AccountWorkflowOutcome::InboxSynced {
            account_id,
            generation,
            imported,
            has_more,
            historical,
            bootstrap: _,
            idle: _,
        } => Ok(AccountOperationSuccess::Synced {
            account_id,
            generation,
            imported,
            has_more,
            historical,
        }),
        AccountWorkflowOutcome::MessageContentFetched {
            message_id,
            account_id,
            generation,
        } => Ok(AccountOperationSuccess::MessageContentFetched {
            message_id,
            account_id,
            generation,
        }),
        AccountWorkflowOutcome::MessagePreviewsFetched {
            account_id,
            generation,
            ..
        } => Err(AccountOperationFailure {
            account_id: Some(account_id),
            generation: Some(generation),
            stage: AccountWorkflowStage::FetchInbox,
            kind: AccountWorkflowFailureKind::UnexpectedReply,
        }),
        AccountWorkflowOutcome::RemovalPending {
            account_id,
            generation,
            stage,
            failure,
        } => Err(AccountOperationFailure {
            account_id: Some(account_id),
            generation: Some(generation),
            stage,
            kind: failure,
        }),
        AccountWorkflowOutcome::Failed { stage, failure } => Err(AccountOperationFailure {
            account_id: identity.account_id,
            generation: identity.generation,
            stage,
            kind: failure,
        }),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AccountDriverError {
    DatabaseClosed,
    Recovery(FailureKind),
    WorkflowRejected,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::sqlite::{
        AccountAuthKind, AccountConfigInput, AccountDiagnostic, RemovedAccount,
    };

    const KEY: &str = "0123456789abcdef0123456789abcdef";

    fn account_id() -> AccountId {
        AccountId::new(7).unwrap()
    }

    fn generation(value: i64) -> AccountGeneration {
        AccountGeneration::new(value).unwrap()
    }

    fn input(key: &str) -> AccountConfigInput {
        AccountConfigInput::new_with_smtp(
            key,
            "Personal",
            "person@example.test",
            AccountAuthKind::AppPassword,
            "person@example.test",
            "imap.example.test",
            993,
            "smtp.example.test",
            465,
            SmtpSecurity::ImplicitTls,
            true,
            0x336699,
        )
        .unwrap()
    }

    fn configuration(generation: i64, lifecycle: AccountLifecycle) -> AccountConfiguration {
        AccountConfiguration {
            account_id: account_id(),
            generation: self::generation(generation),
            credential_key: KEY.into(),
            name: "Personal".into(),
            address: "person@example.test".into(),
            auth_kind: AccountAuthKind::AppPassword,
            login_name: "person@example.test".into(),
            imap_host: "imap.example.test".into(),
            imap_port: 993,
            smtp_host: "smtp.example.test".into(),
            smtp_port: 465,
            smtp_security: SmtpSecurity::ImplicitTls,
            smtp_configured: true,
            accent_rgb: 0x336699,
            lifecycle,
            diagnostic: AccountDiagnostic::Never,
        }
    }

    fn ticket() -> AccountRemovalTicket {
        AccountRemovalTicket {
            account_id: account_id(),
            generation: generation(2),
            credential_key: Some(KEY.into()),
        }
    }

    fn secret() -> Secret {
        Secret::new(b"not-a-real-password".to_vec()).unwrap()
    }

    #[test]
    fn idle_watch_tasks_are_bounded_and_ready_disconnects_are_removed() {
        let (database, _replies, database_runtime, _) =
            crate::store::sqlite::spawn_in_memory().unwrap();
        let (credentials, credential_runtime) = crate::credentials::spawn();
        let mut workflows =
            AccountWorkflows::new(database, credentials, production_imap_diagnostic);
        for raw_id in 1..=MAX_IDLE_WATCHES + 1 {
            let account_id = AccountId::new(i64::try_from(raw_id).unwrap()).unwrap();
            let request = ImapIdleRequest::new(
                "imap.example.test",
                993,
                &format!("person-{raw_id}@example.test"),
                std::num::NonZeroU32::new(1).unwrap(),
            )
            .unwrap();
            workflows.register_idle_watch(account_id, generation(1), request);
        }
        assert_eq!(workflows.idle_watches.len(), MAX_IDLE_WATCHES);

        workflows.cancel_idle_watch(AccountId::new(1).unwrap(), generation(1));
        assert!(workflows.idle_handoff_pending());
        workflows.idle_watches[0].future = Box::pin(async {
            ImapIdleOutcome::Disconnected(crate::network::imap::ImapInboxFetchFailure::Offline)
        });
        let waker = std::task::Waker::noop();
        let mut context = Context::from_waker(waker);
        assert!(workflows.poll_idle_watches(&mut context));
        assert_eq!(workflows.idle_watches.len(), MAX_IDLE_WATCHES - 1);

        drop(workflows);
        credential_runtime.shutdown().unwrap();
        database_runtime.shutdown().unwrap();
    }

    #[test]
    fn draft_preserves_explicit_smtp_transport() {
        for (port, expected_security) in [
            (465, SmtpSecurity::ImplicitTls),
            (587, SmtpSecurity::StartTls),
        ] {
            let draft = AccountConfigDraft::new(
                "Personal",
                "person@example.test",
                "person@example.test",
                "imap.example.test",
                993,
                "smtp.example.test",
                port,
                0x336699,
            )
            .unwrap();
            let input = draft.into_input(&CredentialLocator::parse(KEY).unwrap());

            assert_eq!(&*input.smtp_host, "smtp.example.test");
            assert_eq!(input.smtp_port, port);
            assert_eq!(input.smtp_security, expected_security);
            assert!(input.smtp_explicit);
        }
    }

    #[test]
    fn setup_persists_locator_before_storing_secret() {
        let mut coordinator = AccountCoordinator::default();
        let action = coordinator
            .try_start(AccountWorkflowRequest::PersistAndStore {
                write: Box::new(AccountWrite::Create(input(KEY))),
                secret: secret(),
            })
            .unwrap();
        assert!(matches!(
            action,
            AccountWorkflowAction::Database {
                stage: AccountWorkflowStage::PersistLocator,
                write
            } if matches!(*write, AccountWrite::Create(_))
        ));

        let action = coordinator
            .database_completed(AccountDatabaseCompletion::Saved(configuration(
                1,
                AccountLifecycle::Active,
            )))
            .unwrap();
        assert!(matches!(
            action,
            AccountWorkflowAction::Credential {
                stage: AccountWorkflowStage::StoreCredential,
                operation: CredentialOperation::Store { locator, secret }
            } if locator.as_str() == KEY && secret.expose() == b"not-a-real-password"
        ));

        let action = coordinator
            .credential_completed(AccountCredentialCompletion::Stored)
            .unwrap();
        assert!(matches!(
            action,
            AccountWorkflowAction::Finished(AccountWorkflowOutcome::CredentialStored(_))
        ));
        assert_eq!(coordinator.stage(), None);
    }

    #[test]
    fn failed_store_keeps_configuration_for_explicit_retry() {
        let mut coordinator = AccountCoordinator::default();
        let saved = configuration(1, AccountLifecycle::Active);
        let _ = coordinator
            .try_start(AccountWorkflowRequest::RetryStore {
                configuration: saved.clone(),
                secret: secret(),
            })
            .unwrap();
        let action = coordinator
            .credential_completed(AccountCredentialCompletion::Failed(
                CredentialFailureKind::LockedOrDenied,
            ))
            .unwrap();
        let AccountWorkflowAction::Finished(AccountWorkflowOutcome::CredentialPending {
            configuration,
            failure,
        }) = action
        else {
            panic!("store failure must leave a retryable configuration")
        };
        assert_eq!(configuration, saved);
        assert_eq!(
            failure,
            AccountWorkflowFailureKind::Credential(CredentialFailureKind::LockedOrDenied)
        );

        assert!(matches!(
            coordinator
                .try_start(AccountWorkflowRequest::RetryStore {
                    configuration,
                    secret: secret(),
                })
                .unwrap(),
            AccountWorkflowAction::Credential {
                stage: AccountWorkflowStage::StoreCredential,
                ..
            }
        ));
    }

    #[test]
    fn delete_success_and_missing_are_the_only_confirmation_gate() {
        for delete_outcome in [
            CredentialDeleteOutcome::Deleted,
            CredentialDeleteOutcome::AlreadyMissing,
        ] {
            let mut coordinator = AccountCoordinator::default();
            let _ = coordinator
                .try_start(AccountWorkflowRequest::BeginRemove {
                    account_id: account_id(),
                    expected_generation: generation(1),
                })
                .unwrap();
            let action = coordinator
                .database_completed(AccountDatabaseCompletion::RemovalStarted(ticket()))
                .unwrap();
            assert!(matches!(
                action,
                AccountWorkflowAction::Credential {
                    stage: AccountWorkflowStage::DeleteCredential,
                    operation: CredentialOperation::Delete { .. }
                }
            ));

            let action = coordinator
                .credential_completed(AccountCredentialCompletion::Deleted(delete_outcome))
                .unwrap();
            assert!(matches!(
                action,
                AccountWorkflowAction::Database {
                    stage: AccountWorkflowStage::ConfirmRemoval,
                    write
                } if matches!(
                    *write,
                    AccountWrite::ConfirmCredentialsRemoved {
                        account_id: id,
                        expected_generation
                    } if id == account_id() && expected_generation == generation(2)
                )
            ));
        }
    }

    #[test]
    fn credential_failures_and_wrong_replies_never_confirm_removal() {
        for completion in [
            AccountCredentialCompletion::Failed(CredentialFailureKind::Unavailable),
            AccountCredentialCompletion::ReplyClosed,
            AccountCredentialCompletion::Stored,
            AccountCredentialCompletion::Unexpected,
        ] {
            let mut coordinator = AccountCoordinator::default();
            let _ = coordinator
                .try_start(AccountWorkflowRequest::ResumeRemove(ticket()))
                .unwrap();
            let action = coordinator.credential_completed(completion).unwrap();
            assert!(matches!(
                action,
                AccountWorkflowAction::Finished(AccountWorkflowOutcome::RemovalPending { .. })
            ));
            assert_eq!(coordinator.stage(), None);
        }
    }

    #[test]
    fn accepted_removal_ignores_outer_receiver_closure() {
        let (reply, receiver) = tokio::sync::oneshot::channel::<()>();
        drop(receiver);
        assert!(reply.is_closed());

        let mut coordinator = AccountCoordinator::default();
        let _ = coordinator
            .try_start(AccountWorkflowRequest::ResumeRemove(ticket()))
            .unwrap();
        let action = coordinator
            .credential_completed(AccountCredentialCompletion::Deleted(
                CredentialDeleteOutcome::Deleted,
            ))
            .unwrap();
        assert!(matches!(
            action,
            AccountWorkflowAction::Database {
                stage: AccountWorkflowStage::ConfirmRemoval,
                ..
            }
        ));
        assert_eq!(
            coordinator.stage(),
            Some(AccountWorkflowStage::ConfirmRemoval)
        );
    }

    #[test]
    fn global_active_slot_returns_the_untouched_request() {
        let mut coordinator = AccountCoordinator::default();
        let _ = coordinator
            .try_start(AccountWorkflowRequest::BeginRemove {
                account_id: account_id(),
                expected_generation: generation(1),
            })
            .unwrap();

        let failure = coordinator
            .try_start(AccountWorkflowRequest::PersistAndStore {
                write: Box::new(AccountWrite::Create(input(KEY))),
                secret: secret(),
            })
            .unwrap_err();
        assert_eq!(failure.kind(), AccountWorkflowStartFailureKind::Busy);
        assert!(matches!(
            failure.into_request(),
            AccountWorkflowRequest::PersistAndStore { .. }
        ));
        assert_eq!(
            coordinator.stage(),
            Some(AccountWorkflowStage::BeginRemoval)
        );
    }

    #[test]
    fn confirmation_failure_remains_restart_retryable() {
        let mut coordinator = AccountCoordinator::default();
        let removal_ticket = ticket();
        let _ = coordinator
            .try_start(AccountWorkflowRequest::ResumeRemove(removal_ticket.clone()))
            .unwrap();
        let _ = coordinator
            .credential_completed(AccountCredentialCompletion::Deleted(
                CredentialDeleteOutcome::AlreadyMissing,
            ))
            .unwrap();
        let action = coordinator
            .database_completed(AccountDatabaseCompletion::Failed(FailureKind::Database))
            .unwrap();
        assert_eq!(
            action_as_outcome(action),
            AccountWorkflowOutcome::RemovalPending {
                account_id: removal_ticket.account_id,
                generation: removal_ticket.generation,
                stage: AccountWorkflowStage::ConfirmRemoval,
                failure: AccountWorkflowFailureKind::Database(FailureKind::Database),
            }
        );
    }

    #[test]
    fn purge_reuses_fence_until_complete() {
        let mut coordinator = AccountCoordinator::default();
        let action = coordinator
            .try_start(AccountWorkflowRequest::ResumePurge {
                account_id: account_id(),
                expected_generation: generation(3),
            })
            .unwrap();
        assert_purge_action(&action, 3);

        let action = coordinator
            .database_completed(AccountDatabaseCompletion::Purged(
                AccountPurgeOutcome::Pending {
                    removed_messages: 1,
                    removed_attachments: 2,
                    removed_staging_files: 0,
                    queued_files: 3,
                },
            ))
            .unwrap();
        assert_purge_action(&action, 3);

        let action = coordinator
            .database_completed(AccountDatabaseCompletion::Purged(
                AccountPurgeOutcome::Complete(RemovedAccount {
                    account_id: account_id(),
                }),
            ))
            .unwrap();
        assert_eq!(
            action_as_outcome(action),
            AccountWorkflowOutcome::AccountRemoved {
                account_id: account_id(),
            }
        );
        assert_eq!(coordinator.stage(), None);
    }

    #[test]
    fn debug_output_never_contains_credential_locator() {
        let request = AccountWorkflowRequest::RetryStore {
            configuration: configuration(1, AccountLifecycle::Active),
            secret: secret(),
        };
        assert!(!format!("{request:?}").contains(KEY));

        let outcome = AccountWorkflowOutcome::RemovalPending {
            account_id: account_id(),
            generation: generation(2),
            stage: AccountWorkflowStage::DeleteCredential,
            failure: AccountWorkflowFailureKind::CredentialReplyClosed,
        };
        assert!(!format!("{outcome:?}").contains(KEY));
    }

    fn assert_purge_action(action: &AccountWorkflowAction, expected_generation: i64) {
        assert!(matches!(
            action,
            AccountWorkflowAction::Database {
                stage: AccountWorkflowStage::PurgeRemoval,
                write,
            } if matches!(
                write.as_ref(),
                AccountWrite::PurgeRemovedAccount {
                    account_id: id,
                    expected_generation: fence,
                } if *id == account_id() && *fence == generation(expected_generation)
            )
        ));
    }

    fn action_as_outcome(action: AccountWorkflowAction) -> AccountWorkflowOutcome {
        let AccountWorkflowAction::Finished(outcome) = action else {
            panic!("expected a terminal workflow outcome")
        };
        outcome
    }
}
