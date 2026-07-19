use std::{
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use mail_parser::DateTime;

use crate::{
    content::{ContentError, ContentLimits, ContentStaging, MimeError, prepare_content},
    credentials::{
        CredentialClient, CredentialLocator, CredentialOperation, CredentialOutcome,
        CredentialSubmitError,
    },
    network::imap::{
        ImapInboxContent, ImapInboxFetchFailure, ImapInboxFetchRequest, ImapInboxMessage,
        ImapInboxPage, fetch_canonical_inbox,
    },
    store::sqlite::{
        AccountAuthKind, AccountGeneration, AccountId, AccountLifecycle, AccountRecord,
        ContentImportSubmission, DatabaseClient, DatabaseSubmitError, FailureKind,
        InboxCheckpointOutcome, InboxCursorCommit, InboxCursorOutcome, InboxEnvelope, InboxFlags,
        InboxReceivePage, InboxStageOutcome,
    },
};

use super::account::{AccountWorkflowFailureKind, AccountWorkflowStage, InboxSyncFailureKind};

const FILE_GC_LIMIT: usize = 16;
const MAX_SENDER_BYTES: usize = 320;
const MAX_SUBJECT_BYTES: usize = 998;

pub(super) type ImapInboxFetchFuture =
    Pin<Box<dyn Future<Output = Result<ImapInboxPage, ImapInboxFetchFailure>> + 'static>>;
pub(super) type ImapInboxFetchProbe = fn(ImapInboxFetchRequest) -> ImapInboxFetchFuture;
pub(super) type SyncInboxFuture = Pin<Box<dyn Future<Output = SyncInboxOutcome> + 'static>>;

pub(super) fn production_imap_inbox_fetch(request: ImapInboxFetchRequest) -> ImapInboxFetchFuture {
    Box::pin(fetch_canonical_inbox(request))
}

pub(super) enum SyncInboxOutcome {
    Synced {
        account_id: AccountId,
        generation: AccountGeneration,
        imported: u8,
        has_more: bool,
    },
    Failed {
        stage: AccountWorkflowStage,
        failure: AccountWorkflowFailureKind,
    },
    DatabaseClosed,
}

pub(super) fn start_inbox_sync(
    database: DatabaseClient,
    credentials: CredentialClient,
    fetch_probe: ImapInboxFetchProbe,
    content_root: PathBuf,
    account_id: AccountId,
    expected_generation: AccountGeneration,
) -> SyncInboxFuture {
    Box::pin(run_inbox_sync(
        database,
        credentials,
        fetch_probe,
        content_root,
        account_id,
        expected_generation,
    ))
}

async fn run_inbox_sync(
    database: DatabaseClient,
    credentials: CredentialClient,
    fetch_probe: ImapInboxFetchProbe,
    content_root: PathBuf,
    account_id: AccountId,
    expected_generation: AccountGeneration,
) -> SyncInboxOutcome {
    let configuration = match load_configuration(&database, account_id, expected_generation).await {
        Ok(configuration) => configuration,
        Err(outcome) => return outcome,
    };
    let checkpoint = match load_checkpoint(&database, account_id, expected_generation).await {
        Ok(checkpoint) => checkpoint,
        Err(outcome) => return outcome,
    };
    let secret = match load_credential(&credentials, &configuration.credential_key).await {
        Ok(secret) => secret,
        Err(outcome) => return outcome,
    };
    let first_uid = checkpoint
        .expected_cursor
        .map_or(1, |cursor| cursor.saturating_add(1));
    let fetch_request = match ImapInboxFetchRequest::new(
        &configuration.imap_host,
        configuration.imap_port,
        &configuration.login_name,
        secret,
        first_uid,
        checkpoint.uid_validity,
    ) {
        Ok(request) => request,
        Err(_) => {
            return sync_failure(
                AccountWorkflowStage::FetchInbox,
                InboxSyncFailureKind::Protocol,
            );
        }
    };
    let fetched = match fetch_probe(fetch_request).await {
        Ok(page) => page,
        Err(failure) => {
            return sync_failure(AccountWorkflowStage::FetchInbox, map_fetch_failure(failure));
        }
    };

    if fetched
        .messages
        .iter()
        .any(|message| matches!(message.content, ImapInboxContent::Oversized { .. }))
    {
        return sync_failure(
            AccountWorkflowStage::FetchInbox,
            InboxSyncFailureKind::ResourceLimit,
        );
    }

    let staging = match ContentStaging::open(content_root) {
        Ok(staging) => Arc::new(staging),
        Err(_) => {
            return sync_failure(
                AccountWorkflowStage::ParseContent,
                InboxSyncFailureKind::Storage,
            );
        }
    };
    let ImapInboxPage {
        uid_validity,
        uid_next: _,
        scanned_through_uid,
        next_uid,
        messages,
    } = fetched;
    let has_more = next_uid.is_some();
    let received_at_fallback = now_ms();
    let envelopes = match messages
        .iter()
        .map(|message| inbox_envelope(message, received_at_fallback))
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(envelopes) => envelopes,
        Err(_) => {
            return sync_failure(
                AccountWorkflowStage::StageInbox,
                InboxSyncFailureKind::Protocol,
            );
        }
    };
    let receive_page = match InboxReceivePage::new(
        account_id,
        expected_generation,
        checkpoint.expected_cursor,
        uid_validity.get(),
        scanned_through_uid.map(std::num::NonZeroU32::get),
        envelopes,
    ) {
        Ok(page) => page,
        Err(_) => {
            return sync_failure(
                AccountWorkflowStage::StageInbox,
                InboxSyncFailureKind::Protocol,
            );
        }
    };
    let stage = match database.try_stage_inbox(Box::new(receive_page)) {
        Ok(receiver) => match receiver.await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(failure)) => {
                return database_failure(AccountWorkflowStage::StageInbox, failure.failure().kind);
            }
            Err(_) => return SyncInboxOutcome::DatabaseClosed,
        },
        Err(failure) if failure.reason() == DatabaseSubmitError::Busy => {
            return busy(AccountWorkflowStage::StageInbox);
        }
        Err(_) => return SyncInboxOutcome::DatabaseClosed,
    };
    let (staged_messages, ticket) = match stage {
        InboxStageOutcome::Staged {
            messages, ticket, ..
        } => (messages, ticket),
        InboxStageOutcome::Stale => {
            return database_failure(AccountWorkflowStage::StageInbox, FailureKind::Conflict);
        }
    };

    let mut imported = 0_u8;
    for message in messages.into_vec() {
        let Some(staged) = staged_message(&staged_messages, message.uid.get()) else {
            continue;
        };
        if !staged.needs_content {
            continue;
        }
        let ImapInboxContent::Fetched(raw) = message.content else {
            unreachable!("oversized inbox messages were rejected before staging")
        };
        let prepared = match prepare_content(&raw, &staging, ContentLimits::default()) {
            Ok(prepared) => prepared,
            Err(failure) => return content_failure(failure),
        };
        drop(raw);
        let published = match prepared.publish() {
            Ok(published) => published,
            Err(_) => {
                return sync_failure(
                    AccountWorkflowStage::ImportContent,
                    InboxSyncFailureKind::Storage,
                );
            }
        };
        let submission = Box::new(ContentImportSubmission::new(
            staged.message_id,
            account_id,
            expected_generation,
            published,
        ));
        match database.try_import_content(submission) {
            Ok(receiver) => match receiver.await {
                Ok(Ok(_)) => {
                    imported = imported
                        .checked_add(1)
                        .expect("inbox receive pages fit in u8");
                }
                Ok(Err(failure)) => {
                    return database_failure(
                        AccountWorkflowStage::ImportContent,
                        failure.failure().kind,
                    );
                }
                Err(_) => return SyncInboxOutcome::DatabaseClosed,
            },
            Err(failure) if failure.reason() == DatabaseSubmitError::Busy => {
                return busy(AccountWorkflowStage::ImportContent);
            }
            Err(_) => return SyncInboxOutcome::DatabaseClosed,
        }
    }

    let commit = match InboxCursorCommit::new(ticket, now_ms()) {
        Ok(commit) => commit,
        Err(_) => {
            return sync_failure(
                AccountWorkflowStage::CommitInbox,
                InboxSyncFailureKind::Protocol,
            );
        }
    };
    let committed = match database.try_commit_inbox_cursor(Box::new(commit)) {
        Ok(receiver) => match receiver.await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(failure)) => {
                return database_failure(AccountWorkflowStage::CommitInbox, failure.failure().kind);
            }
            Err(_) => return SyncInboxOutcome::DatabaseClosed,
        },
        Err(failure) if failure.reason() == DatabaseSubmitError::Busy => {
            return busy(AccountWorkflowStage::CommitInbox);
        }
        Err(_) => return SyncInboxOutcome::DatabaseClosed,
    };
    if !matches!(committed, InboxCursorOutcome::Committed { .. }) {
        return database_failure(AccountWorkflowStage::CommitInbox, FailureKind::Conflict);
    }

    // Collection is intentionally best-effort: the durable cursor is already committed and the
    // queue will be retried by the next sync or startup janitor.
    if !collect_files(&database, &staging).await {
        return SyncInboxOutcome::DatabaseClosed;
    }

    SyncInboxOutcome::Synced {
        account_id,
        generation: expected_generation,
        imported,
        has_more,
    }
}

async fn collect_files(database: &DatabaseClient, staging: &Arc<ContentStaging>) -> bool {
    match database.try_run_file_gc(staging, FILE_GC_LIMIT) {
        Ok(receiver) => receiver.await.is_ok(),
        Err(DatabaseSubmitError::Busy) => true,
        Err(DatabaseSubmitError::Closed) => false,
    }
}

async fn load_configuration(
    database: &DatabaseClient,
    account_id: AccountId,
    expected_generation: AccountGeneration,
) -> Result<crate::store::sqlite::AccountConfiguration, SyncInboxOutcome> {
    let receiver = match database.try_load_account(account_id) {
        Ok(receiver) => receiver,
        Err(DatabaseSubmitError::Busy) => {
            return Err(busy(AccountWorkflowStage::LoadConfiguration));
        }
        Err(DatabaseSubmitError::Closed) => return Err(SyncInboxOutcome::DatabaseClosed),
    };
    let record = match receiver.await {
        Ok(Ok(record)) => record,
        Ok(Err(failure)) => {
            return Err(database_failure(
                AccountWorkflowStage::LoadConfiguration,
                failure.kind,
            ));
        }
        Err(_) => return Err(SyncInboxOutcome::DatabaseClosed),
    };
    let AccountRecord::Configured(configuration) = record else {
        return Err(database_failure(
            AccountWorkflowStage::LoadConfiguration,
            FailureKind::Conflict,
        ));
    };
    if configuration.account_id != account_id
        || configuration.generation != expected_generation
        || configuration.auth_kind != AccountAuthKind::AppPassword
        || configuration.lifecycle != AccountLifecycle::Active
    {
        return Err(database_failure(
            AccountWorkflowStage::LoadConfiguration,
            FailureKind::Conflict,
        ));
    }
    Ok(configuration)
}

async fn load_checkpoint(
    database: &DatabaseClient,
    account_id: AccountId,
    expected_generation: AccountGeneration,
) -> Result<crate::store::sqlite::InboxCheckpoint, SyncInboxOutcome> {
    let receiver = match database.try_load_inbox_checkpoint(account_id, expected_generation) {
        Ok(receiver) => receiver,
        Err(DatabaseSubmitError::Busy) => {
            return Err(busy(AccountWorkflowStage::LoadInboxCheckpoint));
        }
        Err(DatabaseSubmitError::Closed) => return Err(SyncInboxOutcome::DatabaseClosed),
    };
    match receiver.await {
        Ok(Ok(InboxCheckpointOutcome::Current(checkpoint))) => Ok(checkpoint),
        Ok(Ok(InboxCheckpointOutcome::Stale | InboxCheckpointOutcome::NotFound)) => {
            Err(database_failure(
                AccountWorkflowStage::LoadInboxCheckpoint,
                FailureKind::Conflict,
            ))
        }
        Ok(Err(failure)) => Err(database_failure(
            AccountWorkflowStage::LoadInboxCheckpoint,
            failure.kind,
        )),
        Err(_) => Err(SyncInboxOutcome::DatabaseClosed),
    }
}

async fn load_credential(
    credentials: &CredentialClient,
    key: &str,
) -> Result<crate::credentials::Secret, SyncInboxOutcome> {
    let locator = CredentialLocator::parse(key).map_err(|_| SyncInboxOutcome::Failed {
        stage: AccountWorkflowStage::LoadCredential,
        failure: AccountWorkflowFailureKind::InvalidLocator,
    })?;
    let response = match credentials.try_submit(CredentialOperation::Load { locator }) {
        Ok(response) => response,
        Err(failure) if failure.reason() == CredentialSubmitError::Busy => {
            return Err(busy(AccountWorkflowStage::LoadCredential));
        }
        Err(_) => {
            return Err(SyncInboxOutcome::Failed {
                stage: AccountWorkflowStage::LoadCredential,
                failure: AccountWorkflowFailureKind::CredentialReplyClosed,
            });
        }
    };
    match response.await {
        Ok(Ok(CredentialOutcome::Loaded(secret))) => Ok(secret),
        Ok(Ok(_)) => Err(SyncInboxOutcome::Failed {
            stage: AccountWorkflowStage::LoadCredential,
            failure: AccountWorkflowFailureKind::UnexpectedReply,
        }),
        Ok(Err(failure)) => Err(SyncInboxOutcome::Failed {
            stage: AccountWorkflowStage::LoadCredential,
            failure: AccountWorkflowFailureKind::Credential(failure.kind),
        }),
        Err(_) => Err(SyncInboxOutcome::Failed {
            stage: AccountWorkflowStage::LoadCredential,
            failure: AccountWorkflowFailureKind::CredentialReplyClosed,
        }),
    }
}

fn inbox_envelope(
    message: &ImapInboxMessage,
    received_at_fallback: i64,
) -> Result<InboxEnvelope, crate::store::sqlite::InboxValidationError> {
    let sender_name = bounded_utf8(&message.envelope.from_name, MAX_SENDER_BYTES);
    let sender_address =
        bounded_sender_address(&message.envelope.from_mailbox, &message.envelope.from_host);
    let subject = bounded_utf8(&message.envelope.subject, MAX_SUBJECT_BYTES);
    let received_at_ms = DateTime::parse_rfc822(&message.internal_date)
        .filter(DateTime::is_valid)
        .and_then(|date| date.to_timestamp().checked_mul(1_000))
        .unwrap_or(received_at_fallback);
    InboxEnvelope::new(
        message.uid.get(),
        sender_name.as_bytes(),
        sender_address.as_bytes(),
        subject.as_bytes(),
        b"",
        received_at_ms,
        InboxFlags::new(message.flags.seen, message.flags.flagged),
        false,
    )
}

fn bounded_sender_address(mailbox: &[u8], host: &[u8]) -> String {
    let mailbox = String::from_utf8_lossy(mailbox);
    let host = String::from_utf8_lossy(host);
    let mut address = String::with_capacity(
        mailbox
            .len()
            .saturating_add(host.len())
            .saturating_add(usize::from(!mailbox.is_empty() && !host.is_empty())),
    );
    address.push_str(&mailbox);
    if !mailbox.is_empty() && !host.is_empty() {
        address.push('@');
    }
    address.push_str(&host);
    truncate_utf8(address, MAX_SENDER_BYTES)
}

fn bounded_utf8(bytes: &[u8], maximum: usize) -> String {
    truncate_utf8(String::from_utf8_lossy(bytes).into_owned(), maximum)
}

fn truncate_utf8(mut value: String, maximum: usize) -> String {
    if value.len() <= maximum {
        return value;
    }
    let mut boundary = maximum;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
    value
}

fn staged_message(
    messages: &[crate::store::sqlite::StagedInboxMessage],
    uid: u32,
) -> Option<crate::store::sqlite::StagedInboxMessage> {
    messages.iter().find(|message| message.uid == uid).copied()
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}

fn map_fetch_failure(failure: ImapInboxFetchFailure) -> InboxSyncFailureKind {
    match failure {
        ImapInboxFetchFailure::Authentication => InboxSyncFailureKind::Authentication,
        ImapInboxFetchFailure::Permission => InboxSyncFailureKind::Permission,
        ImapInboxFetchFailure::Certificate => InboxSyncFailureKind::Certificate,
        ImapInboxFetchFailure::Timeout => InboxSyncFailureKind::Timeout,
        ImapInboxFetchFailure::Offline => InboxSyncFailureKind::Offline,
        ImapInboxFetchFailure::Protocol
        | ImapInboxFetchFailure::MissingUidValidity
        | ImapInboxFetchFailure::MissingUidNext => InboxSyncFailureKind::Protocol,
        ImapInboxFetchFailure::ResourceLimit => InboxSyncFailureKind::ResourceLimit,
        ImapInboxFetchFailure::Cancelled => InboxSyncFailureKind::Cancelled,
        ImapInboxFetchFailure::UidValidityChanged { .. } => {
            InboxSyncFailureKind::UidValidityChanged
        }
    }
}

fn content_failure(failure: ContentError) -> SyncInboxOutcome {
    let kind = match failure {
        ContentError::Mime(MimeError::LimitExceeded { .. }) => InboxSyncFailureKind::ResourceLimit,
        ContentError::Mime(MimeError::Malformed(_)) => InboxSyncFailureKind::MalformedContent,
        ContentError::Storage(_) => InboxSyncFailureKind::Storage,
    };
    sync_failure(AccountWorkflowStage::ParseContent, kind)
}

fn sync_failure(stage: AccountWorkflowStage, kind: InboxSyncFailureKind) -> SyncInboxOutcome {
    SyncInboxOutcome::Failed {
        stage,
        failure: AccountWorkflowFailureKind::InboxSync(kind),
    }
}

fn database_failure(stage: AccountWorkflowStage, kind: FailureKind) -> SyncInboxOutcome {
    SyncInboxOutcome::Failed {
        stage,
        failure: AccountWorkflowFailureKind::Database(kind),
    }
}

fn busy(stage: AccountWorkflowStage) -> SyncInboxOutcome {
    SyncInboxOutcome::Failed {
        stage,
        failure: AccountWorkflowFailureKind::Busy,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        content::FileKey,
        credentials::{self, Secret},
        network::imap::{ImapInboxEnvelope, ImapInboxFlags},
        store::sqlite::{
            self, AccountConfigInput, AccountPurgeOutcome, AccountScope, AccountWrite,
            AccountWriteOutcome, DbReply, FolderScope, Generation, PageBoundary, PageSpec,
            RequestId,
        },
    };
    use keyring_core::CredentialStore;
    use std::{
        fs,
        io::Read,
        num::NonZeroU32,
        path::{Path, PathBuf},
        sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        },
    };

    const CREDENTIAL_KEY: &str = "0123456789abcdef0123456789abcdef";
    const RAW_MESSAGE: &[u8] = b"From: Alice <alice@example.test>\r\n\
Subject: Imported through core\r\n\
Date: Tue, 01 Jul 2025 12:00:00 +0000\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"nivalis-test\"\r\n\
\r\n\
--nivalis-test\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
\r\n\
streamed body\r\n\
--nivalis-test\r\n\
Content-Type: application/octet-stream\r\n\
Content-Disposition: attachment; filename=\"note.bin\"\r\n\
Content-Transfer-Encoding: base64\r\n\
\r\n\
AQIDBA==\r\n\
--nivalis-test--\r\n";
    const RETRY_ONE_V1: &[u8] = b"From: Alice <alice@example.test>\r\n\
Subject: Retry one v1\r\n\
Date: Tue, 01 Jul 2025 12:00:00 +0000\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
\r\n\
first body v1\r\n";
    const RETRY_ONE_V2: &[u8] = b"From: Alice <alice@example.test>\r\n\
Subject: Retry one v2\r\n\
Date: Tue, 01 Jul 2025 12:00:00 +0000\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
\r\n\
first body v2\r\n";
    const RETRY_TWO: &[u8] = b"From: Bob <bob@example.test>\r\n\
Subject: Retry two\r\n\
Date: Tue, 01 Jul 2025 12:01:00 +0000\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
\r\n\
second body\r\n";
    static RETRY_FETCH_ATTEMPT: AtomicU64 = AtomicU64::new(0);

    fn fake_fetch(_: ImapInboxFetchRequest) -> ImapInboxFetchFuture {
        Box::pin(async {
            Ok(ImapInboxPage {
                uid_validity: NonZeroU32::new(7).unwrap(),
                uid_next: NonZeroU32::new(3).unwrap(),
                scanned_through_uid: NonZeroU32::new(1),
                next_uid: NonZeroU32::new(2),
                messages: vec![ImapInboxMessage {
                    uid: NonZeroU32::new(1).unwrap(),
                    flags: ImapInboxFlags {
                        seen: false,
                        flagged: true,
                        ..ImapInboxFlags::default()
                    },
                    internal_date: "01-Jul-2025 12:00:00 +0000".into(),
                    envelope: ImapInboxEnvelope {
                        subject: b"staged subject".to_vec().into_boxed_slice(),
                        from_name: b"Alice".to_vec().into_boxed_slice(),
                        from_mailbox: b"alice".to_vec().into_boxed_slice(),
                        from_host: b"example.test".to_vec().into_boxed_slice(),
                        message_id: b"<one@example.test>".to_vec().into_boxed_slice(),
                    },
                    declared_bytes: u32::try_from(RAW_MESSAGE.len()).unwrap(),
                    content: ImapInboxContent::Fetched(RAW_MESSAGE.into()),
                }]
                .into_boxed_slice(),
            })
        })
    }

    fn oversized_fetch(_: ImapInboxFetchRequest) -> ImapInboxFetchFuture {
        Box::pin(async {
            Ok(ImapInboxPage {
                uid_validity: NonZeroU32::new(7).unwrap(),
                uid_next: NonZeroU32::new(2).unwrap(),
                scanned_through_uid: NonZeroU32::new(1),
                next_uid: None,
                messages: vec![ImapInboxMessage {
                    uid: NonZeroU32::new(1).unwrap(),
                    flags: ImapInboxFlags::default(),
                    internal_date: "01-Jul-2025 12:00:00 +0000".into(),
                    envelope: ImapInboxEnvelope::default(),
                    declared_bytes: (1024 * 1024) + 1,
                    content: ImapInboxContent::Oversized {
                        declared_bytes: (1024 * 1024) + 1,
                    },
                }]
                .into_boxed_slice(),
            })
        })
    }

    fn retry_fetch(_: ImapInboxFetchRequest) -> ImapInboxFetchFuture {
        let retry = RETRY_FETCH_ATTEMPT.fetch_add(1, Ordering::SeqCst) != 0;
        Box::pin(async move {
            let first = if retry { RETRY_ONE_V2 } else { RETRY_ONE_V1 };
            let second = if retry { RETRY_TWO } else { b"malformed" };
            Ok(ImapInboxPage {
                uid_validity: NonZeroU32::new(7).unwrap(),
                uid_next: NonZeroU32::new(3).unwrap(),
                scanned_through_uid: NonZeroU32::new(2),
                next_uid: None,
                messages: vec![
                    retry_message(1, b"staged one", first),
                    retry_message(2, b"staged two", second),
                ]
                .into_boxed_slice(),
            })
        })
    }

    fn retry_message(uid: u32, subject: &[u8], raw: &[u8]) -> ImapInboxMessage {
        ImapInboxMessage {
            uid: NonZeroU32::new(uid).unwrap(),
            flags: ImapInboxFlags::default(),
            internal_date: "01-Jul-2025 12:00:00 +0000".into(),
            envelope: ImapInboxEnvelope {
                subject: subject.into(),
                ..ImapInboxEnvelope::default()
            },
            declared_bytes: u32::try_from(raw.len()).unwrap(),
            content: ImapInboxContent::Fetched(raw.into()),
        }
    }

    #[test]
    fn receive_open_close_remove_and_collect_form_one_bounded_slice() {
        let root = temporary_root();
        let database_path = root.join("mail.sqlite3");
        let content_root = root.join("content");
        fs::create_dir(&root).unwrap();
        let (database, mut replies, database_runtime, _info) =
            sqlite::spawn(database_path.clone()).unwrap();
        let store: Arc<CredentialStore> =
            keyring_core::mock::Store::new().expect("create mock credential store");
        let credential_parts = credentials::spawn_with_test_factory(move || Ok(Arc::clone(&store)));
        let credentials = credential_parts.0.clone();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();

        runtime.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                let saved = database
                    .try_write_account(Box::new(AccountWrite::Create(
                        AccountConfigInput::new(
                            CREDENTIAL_KEY,
                            "Core sync",
                            "alice@example.test",
                            AccountAuthKind::AppPassword,
                            "alice@example.test",
                            "imap.example.test",
                            993,
                            0x335577,
                        )
                        .unwrap(),
                    )))
                    .unwrap()
                    .await
                    .unwrap()
                    .unwrap();
                let AccountWriteOutcome::Saved(configuration) = saved else {
                    panic!("account creation must return a configuration")
                };
                credentials
                    .try_submit(CredentialOperation::Store {
                        locator: CredentialLocator::parse(CREDENTIAL_KEY).unwrap(),
                        secret: Secret::new(b"app-password".to_vec()).unwrap(),
                    })
                    .unwrap()
                    .await
                    .unwrap()
                    .unwrap();

                let outcome = run_inbox_sync(
                    database.clone(),
                    credentials.clone(),
                    fake_fetch,
                    content_root.clone(),
                    configuration.account_id,
                    configuration.generation,
                )
                .await;
                assert!(matches!(
                    outcome,
                    SyncInboxOutcome::Synced {
                        imported: 1,
                        has_more: true,
                        ..
                    }
                ));

                database
                    .try_query_mailbox(
                        RequestId::new(1).unwrap(),
                        Generation::new(1),
                        PageSpec::new(
                            AccountScope::Account(configuration.account_id),
                            FolderScope::Inbox,
                            None,
                            PageBoundary::First,
                            16,
                        )
                        .unwrap(),
                    )
                    .unwrap();
                let message_id = match replies.recv().await.unwrap() {
                    DbReply::Mailbox(reply) => {
                        let page = reply.result.unwrap();
                        assert_eq!(page.rows.len(), 1);
                        assert_eq!(page.rows[0].subject.as_ref(), "Imported through core");
                        assert_eq!(page.rows[0].preview.as_ref(), "streamed body");
                        assert!(page.rows[0].has_attachment);
                        page.rows[0].id
                    }
                    _ => panic!("mailbox query returned an unexpected reply"),
                };

                database
                    .try_open_message(RequestId::new(2).unwrap(), Generation::new(1), message_id)
                    .unwrap();
                let detail = match replies.recv().await.unwrap() {
                    DbReply::Message(reply) => reply.result.unwrap().unwrap(),
                    _ => panic!("message query returned an unexpected reply"),
                };
                let body_key = FileKey::parse(detail.body_file_key.as_deref().unwrap()).unwrap();
                let staging = Arc::new(ContentStaging::open(content_root.clone()).unwrap());
                let mut body_file = staging.open_file(&body_key).unwrap();
                let mut body = String::new();
                body_file.read_to_string(&mut body).unwrap();
                assert_eq!(body.trim(), "streamed body");
                drop(body_file);

                let removal = database
                    .try_write_account(Box::new(AccountWrite::BeginRemove {
                        account_id: configuration.account_id,
                        expected_generation: configuration.generation,
                    }))
                    .unwrap()
                    .await
                    .unwrap()
                    .unwrap();
                let AccountWriteOutcome::RemovalStarted(removal) = removal else {
                    panic!("account removal must return a ticket")
                };
                let cache_removal = database
                    .try_write_account(Box::new(AccountWrite::ConfirmCredentialsRemoved {
                        account_id: removal.account_id,
                        expected_generation: removal.generation,
                    }))
                    .unwrap()
                    .await
                    .unwrap()
                    .unwrap();
                let AccountWriteOutcome::Saved(cache_removal) = cache_removal else {
                    panic!("credential removal must begin cache removal")
                };
                loop {
                    let purged = database
                        .try_write_account(Box::new(AccountWrite::PurgeRemovedAccount {
                            account_id: cache_removal.account_id,
                            expected_generation: cache_removal.generation,
                        }))
                        .unwrap()
                        .await
                        .unwrap()
                        .unwrap();
                    match purged {
                        AccountWriteOutcome::Purged(AccountPurgeOutcome::Pending { .. }) => {}
                        AccountWriteOutcome::Purged(AccountPurgeOutcome::Complete(_)) => break,
                        _ => panic!("cache purge returned an unexpected reply"),
                    }
                }
                let gc = database
                    .try_run_file_gc(&staging, FILE_GC_LIMIT)
                    .unwrap()
                    .await
                    .unwrap()
                    .unwrap();
                assert!(gc.removed >= 2, "body and attachment should be reclaimed");
                let error = staging.open_file(&body_key).unwrap_err();
                assert_eq!(error.kind, std::io::ErrorKind::NotFound);
            })
            .await
            .expect("bounded receive slice timed out");
        });

        credential_parts.1.shutdown().unwrap();
        database_runtime.shutdown().unwrap();
        remove_database_files(&database_path);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn oversized_message_is_rejected_before_staging_or_cursor_advance() {
        let root = temporary_root();
        fs::create_dir(&root).unwrap();
        let database_path = root.join("mail.sqlite3");
        let (database, replies, database_runtime, _info) =
            sqlite::spawn(database_path.clone()).unwrap();
        let store: Arc<CredentialStore> =
            keyring_core::mock::Store::new().expect("create mock credential store");
        let credential_parts = credentials::spawn_with_test_factory(move || Ok(Arc::clone(&store)));
        let credentials = credential_parts.0.clone();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();

        runtime.block_on(async {
            let saved = database
                .try_write_account(Box::new(AccountWrite::Create(
                    AccountConfigInput::new(
                        CREDENTIAL_KEY,
                        "Oversized",
                        "alice@example.test",
                        AccountAuthKind::AppPassword,
                        "alice@example.test",
                        "imap.example.test",
                        993,
                        0x335577,
                    )
                    .unwrap(),
                )))
                .unwrap()
                .await
                .unwrap()
                .unwrap();
            let AccountWriteOutcome::Saved(configuration) = saved else {
                panic!("account creation must return a configuration")
            };
            credentials
                .try_submit(CredentialOperation::Store {
                    locator: CredentialLocator::parse(CREDENTIAL_KEY).unwrap(),
                    secret: Secret::new(b"app-password".to_vec()).unwrap(),
                })
                .unwrap()
                .await
                .unwrap()
                .unwrap();

            let outcome = run_inbox_sync(
                database.clone(),
                credentials.clone(),
                oversized_fetch,
                root.join("content"),
                configuration.account_id,
                configuration.generation,
            )
            .await;
            assert!(matches!(
                outcome,
                SyncInboxOutcome::Failed {
                    stage: AccountWorkflowStage::FetchInbox,
                    failure: AccountWorkflowFailureKind::InboxSync(
                        InboxSyncFailureKind::ResourceLimit
                    ),
                }
            ));
            let checkpoint = database
                .try_load_inbox_checkpoint(configuration.account_id, configuration.generation)
                .unwrap()
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                checkpoint,
                InboxCheckpointOutcome::Current(Default::default())
            );
            assert!(replies.is_empty());
        });

        credential_parts.1.shutdown().unwrap();
        database_runtime.shutdown().unwrap();
        remove_database_files(&database_path);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn retry_imports_only_content_missing_after_a_partial_failure() {
        RETRY_FETCH_ATTEMPT.store(0, Ordering::SeqCst);
        let root = temporary_root();
        fs::create_dir(&root).unwrap();
        let database_path = root.join("mail.sqlite3");
        let content_root = root.join("content");
        let (database, mut replies, database_runtime, _info) =
            sqlite::spawn(database_path.clone()).unwrap();
        let store: Arc<CredentialStore> =
            keyring_core::mock::Store::new().expect("create mock credential store");
        let credential_parts = credentials::spawn_with_test_factory(move || Ok(Arc::clone(&store)));
        let credentials = credential_parts.0.clone();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();

        runtime.block_on(async {
            let saved = database
                .try_write_account(Box::new(AccountWrite::Create(
                    AccountConfigInput::new(
                        CREDENTIAL_KEY,
                        "Retry",
                        "alice@example.test",
                        AccountAuthKind::AppPassword,
                        "alice@example.test",
                        "imap.example.test",
                        993,
                        0x335577,
                    )
                    .unwrap(),
                )))
                .unwrap()
                .await
                .unwrap()
                .unwrap();
            let AccountWriteOutcome::Saved(configuration) = saved else {
                panic!("account creation must return a configuration")
            };
            credentials
                .try_submit(CredentialOperation::Store {
                    locator: CredentialLocator::parse(CREDENTIAL_KEY).unwrap(),
                    secret: Secret::new(b"app-password".to_vec()).unwrap(),
                })
                .unwrap()
                .await
                .unwrap()
                .unwrap();

            let first = run_inbox_sync(
                database.clone(),
                credentials.clone(),
                retry_fetch,
                content_root.clone(),
                configuration.account_id,
                configuration.generation,
            )
            .await;
            assert!(matches!(
                first,
                SyncInboxOutcome::Failed {
                    stage: AccountWorkflowStage::ParseContent,
                    failure: AccountWorkflowFailureKind::InboxSync(
                        InboxSyncFailureKind::MalformedContent
                    ),
                }
            ));
            let (first_id, first_key) = query_retry_one(&database, &mut replies).await;

            let retried = run_inbox_sync(
                database.clone(),
                credentials.clone(),
                retry_fetch,
                content_root.clone(),
                configuration.account_id,
                configuration.generation,
            )
            .await;
            assert!(matches!(
                retried,
                SyncInboxOutcome::Synced {
                    imported: 1,
                    has_more: false,
                    ..
                }
            ));
            let (retried_id, retried_key) = query_retry_one(&database, &mut replies).await;
            assert_eq!(retried_id, first_id);
            assert_eq!(retried_key, first_key);

            let staging = ContentStaging::open(content_root).unwrap();
            let mut body = String::new();
            staging
                .open_file(&first_key)
                .unwrap()
                .read_to_string(&mut body)
                .unwrap();
            assert_eq!(body.trim(), "first body v1");
            let checkpoint = database
                .try_load_inbox_checkpoint(configuration.account_id, configuration.generation)
                .unwrap()
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(
                checkpoint,
                InboxCheckpointOutcome::Current(crate::store::sqlite::InboxCheckpoint {
                    expected_cursor: Some(2),
                    uid_validity: Some(7),
                })
            ));
        });

        credential_parts.1.shutdown().unwrap();
        database_runtime.shutdown().unwrap();
        remove_database_files(&database_path);
        fs::remove_dir_all(root).unwrap();
    }

    async fn query_retry_one(
        database: &DatabaseClient,
        replies: &mut crate::store::sqlite::DatabaseReplies,
    ) -> (crate::store::sqlite::MessageId, FileKey) {
        static NEXT_REQUEST: AtomicU64 = AtomicU64::new(10);
        database
            .try_query_mailbox(
                RequestId::new(NEXT_REQUEST.fetch_add(1, Ordering::Relaxed)).unwrap(),
                Generation::new(1),
                PageSpec::new(
                    AccountScope::All,
                    FolderScope::Inbox,
                    None,
                    PageBoundary::First,
                    16,
                )
                .unwrap(),
            )
            .unwrap();
        let message_id = match replies.recv().await.unwrap() {
            DbReply::Mailbox(reply) => {
                reply
                    .result
                    .unwrap()
                    .rows
                    .iter()
                    .find(|row| row.subject.as_ref() == "Retry one v1")
                    .expect("MIME-derived metadata remains visible")
                    .id
            }
            _ => panic!("mailbox query returned an unexpected reply"),
        };
        database
            .try_open_message(
                RequestId::new(NEXT_REQUEST.fetch_add(1, Ordering::Relaxed)).unwrap(),
                Generation::new(1),
                message_id,
            )
            .unwrap();
        let body_key = match replies.recv().await.unwrap() {
            DbReply::Message(reply) => reply.result.unwrap().unwrap().body_file_key.unwrap(),
            _ => panic!("message query returned an unexpected reply"),
        };
        (message_id, FileKey::parse(&body_key).unwrap())
    }

    fn temporary_root() -> PathBuf {
        static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);
        std::env::temp_dir().join(format!(
            "nivalis-core-sync-{}-{}",
            std::process::id(),
            NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn remove_database_files(path: &Path) {
        let _ = fs::remove_file(path);
        let _ = fs::remove_file(format!("{}-wal", path.display()));
        let _ = fs::remove_file(format!("{}-shm", path.display()));
    }
}
