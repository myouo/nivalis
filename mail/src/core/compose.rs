use std::{
    io::{self, Read},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use tokio::{sync::mpsc, time};

use crate::{
    content::{ContentStaging, FileKind, StorageError, StorageOperation},
    store::sqlite::{
        AccountConfiguration, AccountGeneration, AccountId, AccountLifecycle, AccountRecord,
        ComposeDbOperation, ComposeDbOutcome, DatabaseClient, DatabaseSubmitError, DbFailure,
        DraftRecipient, DraftSnapshot, DraftUpdate, FailureKind, MAX_OUTBOX_SUMMARIES, MessageId,
        NewDraft, OutboxActionFence, OutboxRecipient, OutboxReportOutcome, OutboxReservation,
        OutboxReservationToken, OutboxReserveRequest, OutboxState, OutboxSummaryPage,
        RecipientKind, UncertainResolution,
    },
};

use super::outbound::{OutboundMailbox, PlainTextMessage};

const DATABASE_RETRY_DELAY: Duration = Duration::from_millis(10);
pub(crate) const COMPOSE_BODY_BYTE_LIMIT: usize = 1024 * 1024;
pub(crate) const COMPOSE_SUBJECT_BYTE_LIMIT: usize = 998;
const MAX_RECIPIENTS: usize = 64;
const MAX_ADDRESS_BYTES: usize = 320;
pub(crate) const COMPOSE_TO_FIELD_BYTE_LIMIT: usize = MAX_RECIPIENTS * (MAX_ADDRESS_BYTES + 2);
const MAX_BODY_BYTES: usize = COMPOSE_BODY_BYTE_LIMIT;
const MAX_SUBJECT_BYTES: usize = COMPOSE_SUBJECT_BYTE_LIMIT;
const MAX_PREVIEW_BYTES: usize = 2 * 1024;
const MAX_READER_EXCERPT_BYTES: usize = 64 * 1024;
const MAX_OUTBOUND_MIME_BYTES: usize = 8 * 1024 * 1024;
const MAX_TO_FIELD_BYTES: usize = COMPOSE_TO_FIELD_BYTE_LIMIT;
const MAX_FAILURE_MESSAGE_BYTES: usize = 512;
const RESERVATION_TTL_MS: i64 = 15 * 60 * 1_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ComposeDraftIdentity {
    pub(crate) message_id: MessageId,
    pub(crate) revision: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ComposeDraftInput {
    account_id: AccountId,
    expected_generation: AccountGeneration,
    identity: Option<ComposeDraftIdentity>,
    to: Box<str>,
    subject: Box<str>,
    body: Box<str>,
}

impl ComposeDraftInput {
    pub(crate) fn new(
        account_id: AccountId,
        expected_generation: AccountGeneration,
        identity: Option<ComposeDraftIdentity>,
        to: &str,
        subject: &str,
        body: &str,
    ) -> Result<Self, ComposeFailure> {
        validate_subject(subject)?;
        if body.len() > MAX_BODY_BYTES {
            return Err(ComposeFailure::new(
                ComposeFailureKind::ResourceLimit,
                "draft body exceeds the 1 MiB limit",
                identity,
            ));
        }
        parse_recipients(to).map_err(|failure| failure.with_draft(identity))?;
        Ok(Self {
            account_id,
            expected_generation,
            identity,
            to: to.into(),
            subject: subject.into(),
            body: body.into(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ComposeOperation {
    LoadLatest {
        account_id: AccountId,
        expected_generation: AccountGeneration,
    },
    SaveAndClose(ComposeDraftInput),
    Queue(ComposeDraftInput),
    LoadOutbox,
    RetryOutbox(OutboxActionFence),
    ReleaseFailedOutbox(OutboxActionFence),
    ResolveUncertainOutbox {
        fence: OutboxActionFence,
        resolution: UncertainResolution,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LoadedComposeDraft {
    pub(crate) identity: ComposeDraftIdentity,
    pub(crate) to: Box<str>,
    pub(crate) subject: Box<str>,
    pub(crate) body: Box<str>,
    pub(crate) locked_for_delivery: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ComposeSuccess {
    Loaded(Option<LoadedComposeDraft>),
    Saved(ComposeDraftIdentity),
    Discarded,
    Queued {
        draft: ComposeDraftIdentity,
        rfc_message_id: Box<str>,
    },
    Recovering,
    OutboxLoaded(OutboxSummaryPage),
    OutboxRetried {
        message_id: MessageId,
    },
    OutboxReleased {
        message_id: MessageId,
    },
    UncertainOutboxResolved {
        message_id: MessageId,
        resolution: UncertainResolution,
    },
}

pub(crate) type ComposeReply = Result<ComposeSuccess, ComposeFailure>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ComposeFailureKind {
    InvalidInput,
    ResourceLimit,
    Conflict,
    Configuration,
    NotFound,
    Storage,
    Database,
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ComposeFailure {
    pub(crate) kind: ComposeFailureKind,
    pub(crate) message: Box<str>,
    pub(crate) draft: Option<ComposeDraftIdentity>,
}

impl ComposeFailure {
    fn new(
        kind: ComposeFailureKind,
        message: impl AsRef<str>,
        draft: Option<ComposeDraftIdentity>,
    ) -> Self {
        Self {
            kind,
            message: bounded_text(message.as_ref(), MAX_FAILURE_MESSAGE_BYTES).into(),
            draft,
        }
    }

    fn with_draft(mut self, draft: Option<ComposeDraftIdentity>) -> Self {
        self.draft = draft;
        self
    }
}

/// Executes one bounded compose operation on the existing core runtime.
pub(crate) async fn execute_compose(
    database: &DatabaseClient,
    staging: &ContentStaging,
    outbox_wakeups: &mpsc::Sender<()>,
    operation: ComposeOperation,
) -> ComposeReply {
    match operation {
        ComposeOperation::LoadLatest {
            account_id,
            expected_generation,
        } => load_latest(database, staging, account_id, expected_generation).await,
        ComposeOperation::SaveAndClose(input) => {
            if input.identity.is_none()
                && input.to.trim().is_empty()
                && input.subject.is_empty()
                && input.body.is_empty()
            {
                return Ok(ComposeSuccess::Discarded);
            }
            require_account(database, input.account_id, input.expected_generation)
                .await
                .map_err(|failure| failure.with_draft(input.identity))?;
            save_draft(database, staging, &input, now_ms())
                .await
                .map(|draft| ComposeSuccess::Saved(identity(&draft)))
        }
        ComposeOperation::Queue(input) => {
            queue_draft(database, staging, outbox_wakeups, input).await
        }
        ComposeOperation::LoadOutbox => load_outbox(database).await,
        ComposeOperation::RetryOutbox(fence) => {
            let success = change_outbox(
                database,
                ComposeDbOperation::RetryOutbox {
                    fence,
                    now_ms: now_ms(),
                },
                OutboxState::Ready,
                ComposeSuccess::OutboxRetried {
                    message_id: fence.message_id,
                },
            )
            .await?;
            let _ = outbox_wakeups.try_send(());
            Ok(success)
        }
        ComposeOperation::ReleaseFailedOutbox(fence) => {
            change_outbox(
                database,
                ComposeDbOperation::ReleaseFailedOutbox {
                    fence,
                    now_ms: now_ms(),
                },
                OutboxState::PermanentFailure,
                ComposeSuccess::OutboxReleased {
                    message_id: fence.message_id,
                },
            )
            .await
        }
        ComposeOperation::ResolveUncertainOutbox { fence, resolution } => {
            let expected = match resolution {
                UncertainResolution::AssumeDelivered => OutboxState::Delivered,
                UncertainResolution::Release => OutboxState::Uncertain,
            };
            change_outbox(
                database,
                ComposeDbOperation::ResolveUncertainOutbox {
                    fence,
                    resolution,
                    now_ms: now_ms(),
                },
                expected,
                ComposeSuccess::UncertainOutboxResolved {
                    message_id: fence.message_id,
                    resolution,
                },
            )
            .await
        }
    }
}

async fn load_outbox(database: &DatabaseClient) -> ComposeReply {
    let outcome = compose_call(
        database,
        Box::new(ComposeDbOperation::LoadOutboxSummaries {
            limit: MAX_OUTBOX_SUMMARIES,
        }),
    )
    .await
    .map_err(|error| error.into_failure(None))?;
    let ComposeDbOutcome::OutboxSummaries(page) = outcome else {
        return Err(unexpected_database_outcome(None));
    };
    Ok(ComposeSuccess::OutboxLoaded(page))
}

async fn change_outbox(
    database: &DatabaseClient,
    operation: ComposeDbOperation,
    expected_state: OutboxState,
    success: ComposeSuccess,
) -> ComposeReply {
    let outcome = compose_call(database, Box::new(operation))
        .await
        .map_err(|error| error.into_failure(None))?;
    let report = match outcome {
        ComposeDbOutcome::OutboxRetried(report)
        | ComposeDbOutcome::FailedOutboxReleased(report)
        | ComposeDbOutcome::UncertainOutboxResolved(report) => report,
        _ => return Err(unexpected_database_outcome(None)),
    };
    match report {
        OutboxReportOutcome::Applied(state) if state == expected_state => Ok(success),
        OutboxReportOutcome::Stale => Err(ComposeFailure::new(
            ComposeFailureKind::Conflict,
            "outbox item changed; reload the outbox before trying again",
            None,
        )),
        OutboxReportOutcome::Applied(_) => Err(unexpected_database_outcome(None)),
    }
}

async fn load_latest(
    database: &DatabaseClient,
    staging: &ContentStaging,
    account_id: AccountId,
    expected_generation: AccountGeneration,
) -> ComposeReply {
    require_account(database, account_id, expected_generation).await?;
    let outcome = compose_call(
        database,
        Box::new(ComposeDbOperation::LoadLatestDraft { account_id }),
    )
    .await
    .map_err(|error| error.into_failure(None))?;
    let ComposeDbOutcome::LatestDraft(draft) = outcome else {
        return Err(unexpected_database_outcome(None));
    };
    let Some(draft) = draft else {
        return Ok(ComposeSuccess::Loaded(None));
    };
    let draft_identity = identity(&draft);
    let recipients = draft
        .recipients
        .iter()
        .map(|recipient| recipient.address.as_ref())
        .collect::<Vec<_>>()
        .join(", ");
    parse_recipients(&recipients).map_err(|failure| failure.with_draft(Some(draft_identity)))?;
    let body = read_draft_body(staging, &draft)
        .map_err(|failure| failure.with_draft(Some(draft_identity)))?;
    Ok(ComposeSuccess::Loaded(Some(LoadedComposeDraft {
        identity: draft_identity,
        to: recipients.into_boxed_str(),
        subject: draft.subject,
        body,
        locked_for_delivery: draft.locked_artifact_generation.is_some(),
    })))
}

async fn queue_draft(
    database: &DatabaseClient,
    staging: &ContentStaging,
    outbox_wakeups: &mpsc::Sender<()>,
    input: ComposeDraftInput,
) -> ComposeReply {
    let recipients =
        parse_recipients(&input.to).map_err(|failure| failure.with_draft(input.identity))?;
    if recipients.is_empty() {
        return Err(ComposeFailure::new(
            ComposeFailureKind::InvalidInput,
            "at least one To recipient is required",
            input.identity,
        ));
    }
    let configuration = require_account(database, input.account_id, input.expected_generation)
        .await
        .map_err(|failure| failure.with_draft(input.identity))?;
    if !configuration.smtp_configured {
        return Err(ComposeFailure::new(
            ComposeFailureKind::Configuration,
            "SMTP setup is incomplete",
            input.identity,
        ));
    }
    validate_sender_and_recipients(&configuration, &recipients)
        .map_err(|failure| failure.with_draft(input.identity))?;

    let saved = save_draft(database, staging, &input, now_ms()).await?;
    let saved_identity = identity(&saved);
    let random = random_bytes().map_err(|failure| failure.with_draft(Some(saved_identity)))?;
    let token = OutboxReservationToken::new(random);
    let encoded = encode_hex(random);
    let rfc_message_id = format!("<{encoded}@nivalis.invalid>");
    let created_at_ms = now_ms();
    let expires_at_ms = created_at_ms
        .checked_add(RESERVATION_TTL_MS)
        .ok_or_else(|| {
            ComposeFailure::new(
                ComposeFailureKind::ResourceLimit,
                "outbox reservation time overflow",
                Some(saved_identity),
            )
        })?;
    let outbox_recipients = recipients
        .iter()
        .map(|address| OutboxRecipient::new(RecipientKind::To, address, ""))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|failure| database_failure(failure, Some(saved_identity)))?;
    let reserve = OutboxReserveRequest::new(
        saved.message_id,
        saved.account_id,
        input.expected_generation,
        saved.revision,
        token,
        &rfc_message_id,
        outbox_recipients,
        created_at_ms,
        expires_at_ms,
    )
    .map_err(|failure| database_failure(failure, Some(saved_identity)))?;
    let outcome = compose_call(
        database,
        Box::new(ComposeDbOperation::ReserveOutbox(reserve)),
    )
    .await
    .map_err(|error| error.into_failure(Some(saved_identity)))?;
    let ComposeDbOutcome::OutboxReserved(reservation) = outcome else {
        return Err(unexpected_database_outcome(Some(saved_identity)));
    };
    let finalized =
        finalize_reserved_message(database, staging, &configuration, &saved, reservation)
            .await
            .is_ok();
    // Once reserved, the durable drainer owns recovery. Never leave the UI in a retryable
    // editing state that could make the same delivery intent look unsaved.
    let _ = outbox_wakeups.try_send(());
    let rfc_message_id = rfc_message_id.into_boxed_str();
    if finalized {
        Ok(ComposeSuccess::Queued {
            draft: saved_identity,
            rfc_message_id,
        })
    } else {
        Ok(ComposeSuccess::Recovering)
    }
}

async fn finalize_reserved_message(
    database: &DatabaseClient,
    staging: &ContentStaging,
    configuration: &AccountConfiguration,
    saved: &DraftSnapshot,
    reservation: OutboxReservation,
) -> Result<(), ComposeFailure> {
    let saved_identity = identity(saved);
    let from = OutboundMailbox::new(&configuration.address, &configuration.name)
        .map_err(|error| io_failure(error, Some(saved_identity)))?;
    let mailboxes = saved
        .recipients
        .iter()
        .map(|recipient| OutboundMailbox::new(&recipient.address, &recipient.display_name))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| io_failure(error, Some(saved_identity)))?;
    // The saved timestamp is part of the immutable snapshot, so crash rebuilds are byte-stable.
    let message = PlainTextMessage::new(
        from,
        &mailboxes,
        &saved.subject,
        &reservation.rfc_message_id,
        saved.updated_at_ms.div_euclid(1_000),
    )
    .map_err(|error| io_failure(error, Some(saved_identity)))?;
    let mut body = staging
        .open_file(&saved.body_file_key)
        .map_err(|error| storage_failure(error, Some(saved_identity)))?;
    let body_bytes = body
        .metadata()
        .map_err(|error| {
            ComposeFailure::new(
                ComposeFailureKind::Storage,
                error.to_string(),
                Some(saved_identity),
            )
        })?
        .len();
    if body_bytes != saved.body_byte_count {
        return Err(ComposeFailure::new(
            ComposeFailureKind::Storage,
            "saved draft body length no longer matches its database record",
            Some(saved_identity),
        ));
    }
    let staged = staging
        .stage_writer_at(&reservation.file_key, MAX_OUTBOUND_MIME_BYTES, |output| {
            message.write_to(output, &mut body)
        })
        .map_err(|error| storage_failure(error, Some(saved_identity)))?;
    let byte_count = staged.byte_count();
    let mut published = staged
        .publish()
        .map_err(|error| storage_failure(error, Some(saved_identity)))?;
    // The durable reservation owns this file even if finalization is interrupted.
    published.retain();

    let outcome = compose_call(
        database,
        Box::new(ComposeDbOperation::FinalizeOutbox {
            reservation,
            wire_byte_count: byte_count,
            now_ms: now_ms(),
        }),
    )
    .await
    .map_err(|error| error.into_failure(Some(saved_identity)))?;
    let ComposeDbOutcome::OutboxFinalized(finalized) = outcome else {
        return Err(unexpected_database_outcome(Some(saved_identity)));
    };
    if finalized != OutboxReportOutcome::Applied(OutboxState::Ready) {
        return Err(ComposeFailure::new(
            ComposeFailureKind::Conflict,
            "outbox reservation changed before it could be finalized",
            Some(saved_identity),
        ));
    }

    Ok(())
}

async fn save_draft(
    database: &DatabaseClient,
    staging: &ContentStaging,
    input: &ComposeDraftInput,
    updated_at_ms: i64,
) -> Result<DraftSnapshot, ComposeFailure> {
    let recipients =
        parse_recipients(&input.to).map_err(|failure| failure.with_draft(input.identity))?;
    let draft_recipients = recipients
        .iter()
        .map(|address| DraftRecipient::new(address, ""))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|failure| database_failure(failure, input.identity))?;
    let preview = make_preview(&input.body);
    let excerpt = bounded_text(&input.body, MAX_READER_EXCERPT_BYTES);
    let remote_key = if input.identity.is_none() {
        let random = random_bytes().map_err(|failure| failure.with_draft(input.identity))?;
        Some(format!("local-draft-{}", encode_hex(random)))
    } else {
        None
    };
    let staged = staging
        .stage_reader(FileKind::Body, input.body.as_bytes(), MAX_BODY_BYTES)
        .map_err(|error| storage_failure(error, input.identity))?;
    let body_byte_count = staged.byte_count();
    let mut published = staged
        .publish()
        .map_err(|error| storage_failure(error, input.identity))?;
    let operation = match input.identity {
        Some(identity) => ComposeDbOperation::UpdateDraft(
            DraftUpdate::new(
                identity.message_id,
                input.account_id,
                input.expected_generation,
                identity.revision,
                &input.subject,
                &preview,
                excerpt,
                published.key().clone(),
                body_byte_count,
                draft_recipients,
                updated_at_ms,
            )
            .map_err(|failure| database_failure(failure, Some(identity)))?,
        ),
        None => ComposeDbOperation::CreateDraft(
            NewDraft::new(
                input.account_id,
                input.expected_generation,
                remote_key.as_deref().expect("new drafts have a local key"),
                &input.subject,
                &preview,
                excerpt,
                published.key().clone(),
                body_byte_count,
                draft_recipients,
                updated_at_ms,
            )
            .map_err(|failure| database_failure(failure, None))?,
        ),
    };
    let outcome = compose_call(database, Box::new(operation))
        .await
        .map_err(|error| error.into_failure(input.identity))?;
    let ComposeDbOutcome::DraftSaved(saved) = outcome else {
        return Err(unexpected_database_outcome(input.identity));
    };
    published.retain();
    Ok(saved)
}

async fn require_account(
    database: &DatabaseClient,
    account_id: AccountId,
    expected_generation: AccountGeneration,
) -> Result<AccountConfiguration, ComposeFailure> {
    let record = load_account(database, account_id).await?;
    let AccountRecord::Configured(configuration) = record else {
        return Err(ComposeFailure::new(
            ComposeFailureKind::Conflict,
            "account setup is incomplete",
            None,
        ));
    };
    if configuration.generation != expected_generation {
        return Err(ComposeFailure::new(
            ComposeFailureKind::Conflict,
            "account configuration changed",
            None,
        ));
    }
    if configuration.lifecycle != AccountLifecycle::Active {
        return Err(ComposeFailure::new(
            ComposeFailureKind::Conflict,
            "account is not active",
            None,
        ));
    }
    Ok(configuration)
}

async fn load_account(
    database: &DatabaseClient,
    account_id: AccountId,
) -> Result<AccountRecord, ComposeFailure> {
    loop {
        let receiver = match database.try_load_account(account_id) {
            Ok(receiver) => receiver,
            Err(DatabaseSubmitError::Busy) => {
                time::sleep(DATABASE_RETRY_DELAY).await;
                continue;
            }
            Err(DatabaseSubmitError::Closed) => {
                return Err(ComposeFailure::new(
                    ComposeFailureKind::Unavailable,
                    "database actor is unavailable",
                    None,
                ));
            }
        };
        match receiver.await {
            Ok(Ok(record)) => return Ok(record),
            Ok(Err(failure)) if is_database_busy(&failure) => {
                time::sleep(DATABASE_RETRY_DELAY).await;
            }
            Ok(Err(failure)) => return Err(database_failure(failure, None)),
            Err(_) => {
                return Err(ComposeFailure::new(
                    ComposeFailureKind::Unavailable,
                    "database actor stopped before replying",
                    None,
                ));
            }
        }
    }
}

async fn compose_call(
    database: &DatabaseClient,
    mut operation: Box<ComposeDbOperation>,
) -> Result<ComposeDbOutcome, DbCallError> {
    loop {
        let receiver = match database.try_compose_db(operation) {
            Ok(receiver) => receiver,
            Err(failure) => {
                let (reason, returned) = failure.into_parts();
                match reason {
                    DatabaseSubmitError::Busy => {
                        operation = returned;
                        time::sleep(DATABASE_RETRY_DELAY).await;
                        continue;
                    }
                    DatabaseSubmitError::Closed => return Err(DbCallError::Closed),
                }
            }
        };
        match receiver.await {
            Ok(Ok(outcome)) => return Ok(outcome),
            Ok(Err(failure)) => {
                let (database_failure, returned) = failure.into_parts();
                if is_database_busy(&database_failure) {
                    operation = returned;
                    time::sleep(DATABASE_RETRY_DELAY).await;
                    continue;
                }
                return Err(DbCallError::Failure(database_failure));
            }
            Err(_) => return Err(DbCallError::Closed),
        }
    }
}

fn read_draft_body(
    staging: &ContentStaging,
    draft: &DraftSnapshot,
) -> Result<Box<str>, ComposeFailure> {
    if draft.body_byte_count > MAX_BODY_BYTES as u64 {
        return Err(ComposeFailure::new(
            ComposeFailureKind::ResourceLimit,
            "stored draft body exceeds the 1 MiB limit",
            Some(identity(draft)),
        ));
    }
    let mut file = staging
        .open_file(&draft.body_file_key)
        .map_err(|error| storage_failure(error, Some(identity(draft))))?;
    let metadata = file.metadata().map_err(|error| {
        ComposeFailure::new(
            ComposeFailureKind::Storage,
            error.to_string(),
            Some(identity(draft)),
        )
    })?;
    if metadata.len() != draft.body_byte_count {
        return Err(ComposeFailure::new(
            ComposeFailureKind::Storage,
            "stored draft body length does not match its database record",
            Some(identity(draft)),
        ));
    }
    let capacity = usize::try_from(draft.body_byte_count).unwrap_or(MAX_BODY_BYTES);
    let mut bytes = Vec::with_capacity(capacity);
    file.by_ref()
        .take((MAX_BODY_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            ComposeFailure::new(
                ComposeFailureKind::Storage,
                error.to_string(),
                Some(identity(draft)),
            )
        })?;
    if bytes.len() != capacity {
        return Err(ComposeFailure::new(
            ComposeFailureKind::Storage,
            "stored draft body changed while it was being opened",
            Some(identity(draft)),
        ));
    }
    String::from_utf8(bytes)
        .map(String::into_boxed_str)
        .map_err(|_| {
            ComposeFailure::new(
                ComposeFailureKind::Storage,
                "stored draft body is not valid UTF-8",
                Some(identity(draft)),
            )
        })
}

fn parse_recipients(value: &str) -> Result<Vec<&str>, ComposeFailure> {
    if value.len() > MAX_TO_FIELD_BYTES {
        return Err(ComposeFailure::new(
            ComposeFailureKind::ResourceLimit,
            "recipient field exceeds its bounded size",
            None,
        ));
    }
    if value.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut recipients = Vec::new();
    for address in value.split(',') {
        let address = address.trim();
        if address.is_empty() {
            return Err(ComposeFailure::new(
                ComposeFailureKind::InvalidInput,
                "recipient list contains an empty address",
                None,
            ));
        }
        if recipients.len() == MAX_RECIPIENTS {
            return Err(ComposeFailure::new(
                ComposeFailureKind::ResourceLimit,
                "recipient list exceeds the 64-address limit",
                None,
            ));
        }
        OutboundMailbox::new(address, "").map_err(|error| io_failure(error, None))?;
        recipients.push(address);
    }
    Ok(recipients)
}

fn validate_subject(subject: &str) -> Result<(), ComposeFailure> {
    if subject.len() > MAX_SUBJECT_BYTES {
        return Err(ComposeFailure::new(
            ComposeFailureKind::ResourceLimit,
            "subject exceeds the 998-byte limit",
            None,
        ));
    }
    if subject.bytes().any(|byte| byte < b' ' || byte == 0x7f) {
        return Err(ComposeFailure::new(
            ComposeFailureKind::InvalidInput,
            "subject cannot contain control characters",
            None,
        ));
    }
    Ok(())
}

fn validate_sender_and_recipients(
    configuration: &AccountConfiguration,
    recipients: &[&str],
) -> Result<(), ComposeFailure> {
    OutboundMailbox::new(&configuration.address, &configuration.name)
        .map_err(|error| io_failure(error, None))?;
    for recipient in recipients {
        OutboundMailbox::new(recipient, "").map_err(|error| io_failure(error, None))?;
    }
    Ok(())
}

fn make_preview(body: &str) -> String {
    let mut preview = String::with_capacity(body.len().min(MAX_PREVIEW_BYTES));
    let mut pending_space = false;
    for character in body.chars() {
        if character.is_whitespace() {
            pending_space = !preview.is_empty();
            continue;
        }
        if pending_space && preview.len() < MAX_PREVIEW_BYTES {
            preview.push(' ');
        }
        pending_space = false;
        if preview.len() + character.len_utf8() > MAX_PREVIEW_BYTES {
            break;
        }
        preview.push(character);
    }
    preview
}

fn bounded_text(value: &str, maximum: usize) -> &str {
    if value.len() <= maximum {
        return value;
    }
    let mut end = maximum;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn identity(draft: &DraftSnapshot) -> ComposeDraftIdentity {
    ComposeDraftIdentity {
        message_id: draft.message_id,
        revision: draft.revision,
    }
}

fn random_bytes() -> Result<[u8; 16], ComposeFailure> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| {
        ComposeFailure::new(
            ComposeFailureKind::Unavailable,
            "operating-system randomness is unavailable",
            None,
        )
    })?;
    Ok(bytes)
}

fn encode_hex(bytes: [u8; 16]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(32);
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}

fn is_database_busy(failure: &DbFailure) -> bool {
    failure.kind == FailureKind::Database
        && (failure.message.contains("database is locked")
            || failure.message.contains("database is busy")
            || failure.message.contains("SQLITE_BUSY")
            || failure.message.contains("SQLITE_LOCKED"))
}

fn database_failure(failure: DbFailure, draft: Option<ComposeDraftIdentity>) -> ComposeFailure {
    let kind = match failure.kind {
        FailureKind::ResourceLimit => ComposeFailureKind::ResourceLimit,
        FailureKind::NotFound => ComposeFailureKind::NotFound,
        FailureKind::Conflict => ComposeFailureKind::Conflict,
        FailureKind::Database | FailureKind::Migration => ComposeFailureKind::Database,
    };
    ComposeFailure::new(kind, failure.message, draft)
}

fn storage_failure(failure: StorageError, draft: Option<ComposeDraftIdentity>) -> ComposeFailure {
    let kind = if failure.operation == StorageOperation::WriteTemporary
        && failure.kind == io::ErrorKind::FileTooLarge
    {
        ComposeFailureKind::ResourceLimit
    } else if failure.operation == StorageOperation::WriteTemporary
        && failure.kind == io::ErrorKind::InvalidInput
    {
        ComposeFailureKind::InvalidInput
    } else {
        ComposeFailureKind::Storage
    };
    ComposeFailure::new(kind, failure.to_string(), draft)
}

fn io_failure(error: io::Error, draft: Option<ComposeDraftIdentity>) -> ComposeFailure {
    let kind = if error.kind() == io::ErrorKind::FileTooLarge {
        ComposeFailureKind::ResourceLimit
    } else {
        ComposeFailureKind::InvalidInput
    };
    ComposeFailure::new(kind, error.to_string(), draft)
}

fn unexpected_database_outcome(draft: Option<ComposeDraftIdentity>) -> ComposeFailure {
    ComposeFailure::new(
        ComposeFailureKind::Database,
        "database actor returned an unexpected compose result",
        draft,
    )
}

#[derive(Debug)]
enum DbCallError {
    Closed,
    Failure(DbFailure),
}

impl DbCallError {
    fn into_failure(self, draft: Option<ComposeDraftIdentity>) -> ComposeFailure {
        match self {
            Self::Closed => ComposeFailure::new(
                ComposeFailureKind::Unavailable,
                "database actor is unavailable",
                draft,
            ),
            Self::Failure(failure) => database_failure(failure, draft),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use crate::store::sqlite::{
        AccountAuthKind, AccountConfigInput, AccountWrite, AccountWriteOutcome, ComposeDbOperation,
        ComposeDbOutcome, OutboxClaimOutcome, OutboxErrorClass, OutboxReport, SmtpSecurity,
    };

    use super::*;

    const TEST_NOW_TIMEOUT: Duration = Duration::from_secs(3);

    #[test]
    fn saved_draft_loads_after_database_restart() {
        let paths = TestPaths::new("restart");
        let staging = ContentStaging::open(paths.content.clone()).unwrap();
        let (client, _replies, runtime, _) =
            crate::store::sqlite::spawn(paths.database.clone()).expect("start test database");
        let account = block_on(create_account(&client));
        let (wakeups, _wake_receiver) = mpsc::channel(1);
        let saved = block_on(execute_compose(
            &client,
            &staging,
            &wakeups,
            ComposeOperation::SaveAndClose(input(&account, None, "first body")),
        ))
        .unwrap();
        let ComposeSuccess::Saved(saved_identity) = saved else {
            panic!("expected saved draft");
        };
        runtime.shutdown().unwrap();
        drop(staging);

        let (client, _replies, runtime, _) =
            crate::store::sqlite::spawn(paths.database.clone()).expect("restart test database");
        let staging = ContentStaging::open(paths.content.clone()).unwrap();
        let loaded = block_on(execute_compose(
            &client,
            &staging,
            &wakeups,
            ComposeOperation::LoadLatest {
                account_id: account.account_id,
                expected_generation: account.generation,
            },
        ))
        .unwrap();
        let ComposeSuccess::Loaded(Some(loaded)) = loaded else {
            panic!("expected loaded draft");
        };
        assert_eq!(loaded.identity, saved_identity);
        assert_eq!(loaded.to.as_ref(), "bob@example.test");
        assert_eq!(loaded.subject.as_ref(), "Subject");
        assert_eq!(loaded.body.as_ref(), "first body");
        runtime.shutdown().unwrap();
    }

    #[test]
    fn stale_revision_cannot_overwrite_newer_saved_draft() {
        let paths = TestPaths::new("revision");
        let staging = ContentStaging::open(paths.content.clone()).unwrap();
        let (client, _replies, runtime, _) =
            crate::store::sqlite::spawn(paths.database.clone()).expect("start test database");
        let account = block_on(create_account(&client));
        let (wakeups, _wake_receiver) = mpsc::channel(1);
        let first = block_on(execute_compose(
            &client,
            &staging,
            &wakeups,
            ComposeOperation::SaveAndClose(input(&account, None, "one")),
        ))
        .unwrap();
        let ComposeSuccess::Saved(first) = first else {
            panic!("expected first save");
        };
        let second = block_on(execute_compose(
            &client,
            &staging,
            &wakeups,
            ComposeOperation::SaveAndClose(input(&account, Some(first), "two")),
        ))
        .unwrap();
        let ComposeSuccess::Saved(second) = second else {
            panic!("expected second save");
        };
        assert!(second.revision > first.revision);

        let failure = block_on(execute_compose(
            &client,
            &staging,
            &wakeups,
            ComposeOperation::SaveAndClose(input(&account, Some(first), "stale")),
        ))
        .unwrap_err();
        assert_eq!(failure.kind, ComposeFailureKind::Conflict);
        assert_eq!(failure.draft, Some(first));
        let loaded = block_on(execute_compose(
            &client,
            &staging,
            &wakeups,
            ComposeOperation::LoadLatest {
                account_id: account.account_id,
                expected_generation: account.generation,
            },
        ))
        .unwrap();
        let ComposeSuccess::Loaded(Some(loaded)) = loaded else {
            panic!("expected latest draft");
        };
        assert_eq!(loaded.identity, second);
        assert_eq!(loaded.body.as_ref(), "two");
        runtime.shutdown().unwrap();
    }

    #[test]
    fn queue_commits_ready_record_and_private_mime_artifact() {
        let paths = TestPaths::new("queue");
        let staging = ContentStaging::open(paths.content.clone()).unwrap();
        let (client, _replies, runtime, _) =
            crate::store::sqlite::spawn(paths.database.clone()).expect("start test database");
        let account = block_on(create_account(&client));
        let (wakeups, mut wake_receiver) = mpsc::channel(1);
        let queued = block_on(execute_compose(
            &client,
            &staging,
            &wakeups,
            ComposeOperation::Queue(input(&account, None, "queued body")),
        ))
        .unwrap();
        let ComposeSuccess::Queued {
            draft,
            rfc_message_id,
        } = queued
        else {
            panic!("expected queued draft");
        };
        assert!(rfc_message_id.ends_with("@nivalis.invalid>"));
        assert_eq!(wake_receiver.try_recv(), Ok(()));

        let outcome = block_on(compose_call(
            &client,
            Box::new(ComposeDbOperation::ClaimNextOutbox {
                now_ms: now_ms().saturating_add(1),
            }),
        ))
        .unwrap();
        let ComposeDbOutcome::OutboxClaimed(OutboxClaimOutcome::Claimed(claim)) = outcome else {
            panic!("expected durable ready outbox claim");
        };
        assert_eq!(claim.lease.message_id, draft.message_id);
        let mut mime = Vec::new();
        staging
            .open_file(&claim.file_key)
            .unwrap()
            .read_to_end(&mut mime)
            .unwrap();
        assert!(mime.len() <= MAX_OUTBOUND_MIME_BYTES);
        assert!(
            mime.windows(b"queued=20body".len())
                .any(|part| part == b"queued=20body")
        );
        runtime.shutdown().unwrap();
    }

    #[test]
    fn persistent_outbox_management_retries_and_releases_with_loaded_fence() {
        let paths = TestPaths::new("outbox-management");
        let staging = ContentStaging::open(paths.content.clone()).unwrap();
        let (client, _replies, runtime, _) =
            crate::store::sqlite::spawn(paths.database.clone()).expect("start test database");
        let account = block_on(create_account(&client));
        let (wakeups, mut wake_receiver) = mpsc::channel(1);
        let queued = block_on(execute_compose(
            &client,
            &staging,
            &wakeups,
            ComposeOperation::Queue(input(&account, None, "managed body")),
        ))
        .unwrap();
        let ComposeSuccess::Queued { draft, .. } = queued else {
            panic!("expected queued draft");
        };
        assert_eq!(wake_receiver.try_recv(), Ok(()));

        let claim = block_on(compose_call(
            &client,
            Box::new(ComposeDbOperation::ClaimNextOutbox {
                now_ms: now_ms().saturating_add(1),
            }),
        ))
        .unwrap();
        let ComposeDbOutcome::OutboxClaimed(OutboxClaimOutcome::Claimed(claim)) = claim else {
            panic!("expected first outbox claim");
        };
        let reported = block_on(compose_call(
            &client,
            Box::new(ComposeDbOperation::ReportOutbox {
                lease: claim.lease,
                report: OutboxReport::permanent_failure(
                    OutboxErrorClass::Authentication,
                    "test_authentication",
                )
                .unwrap(),
                now_ms: now_ms().saturating_add(2),
            }),
        ))
        .unwrap();
        assert!(matches!(
            reported,
            ComposeDbOutcome::OutboxReported(OutboxReportOutcome::Applied(
                OutboxState::PermanentFailure
            ))
        ));

        let loaded = block_on(execute_compose(
            &client,
            &staging,
            &wakeups,
            ComposeOperation::LoadOutbox,
        ))
        .unwrap();
        let ComposeSuccess::OutboxLoaded(page) = loaded else {
            panic!("expected persistent outbox page");
        };
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].message_id, draft.message_id);
        assert_eq!(page.items[0].state, OutboxState::PermanentFailure);
        let fence = page.items[0].action_fence();

        let retried = block_on(execute_compose(
            &client,
            &staging,
            &wakeups,
            ComposeOperation::RetryOutbox(fence),
        ))
        .unwrap();
        assert_eq!(
            retried,
            ComposeSuccess::OutboxRetried {
                message_id: draft.message_id
            }
        );
        assert_eq!(wake_receiver.try_recv(), Ok(()));

        let claim = block_on(compose_call(
            &client,
            Box::new(ComposeDbOperation::ClaimNextOutbox {
                now_ms: now_ms().saturating_add(3),
            }),
        ))
        .unwrap();
        let ComposeDbOutcome::OutboxClaimed(OutboxClaimOutcome::Claimed(claim)) = claim else {
            panic!("expected retried outbox claim");
        };
        block_on(compose_call(
            &client,
            Box::new(ComposeDbOperation::ReportOutbox {
                lease: claim.lease,
                report: OutboxReport::permanent_failure(
                    OutboxErrorClass::Permanent,
                    "test_permanent",
                )
                .unwrap(),
                now_ms: now_ms().saturating_add(4),
            }),
        ))
        .unwrap();
        let loaded = block_on(execute_compose(
            &client,
            &staging,
            &wakeups,
            ComposeOperation::LoadOutbox,
        ))
        .unwrap();
        let ComposeSuccess::OutboxLoaded(page) = loaded else {
            panic!("expected refreshed outbox page");
        };
        let released = block_on(execute_compose(
            &client,
            &staging,
            &wakeups,
            ComposeOperation::ReleaseFailedOutbox(page.items[0].action_fence()),
        ))
        .unwrap();
        assert_eq!(
            released,
            ComposeSuccess::OutboxReleased {
                message_id: draft.message_id
            }
        );
        let loaded = block_on(execute_compose(
            &client,
            &staging,
            &wakeups,
            ComposeOperation::LoadOutbox,
        ))
        .unwrap();
        let ComposeSuccess::OutboxLoaded(page) = loaded else {
            panic!("expected empty outbox page");
        };
        assert!(page.items.is_empty());
        let draft = block_on(execute_compose(
            &client,
            &staging,
            &wakeups,
            ComposeOperation::LoadLatest {
                account_id: account.account_id,
                expected_generation: account.generation,
            },
        ))
        .unwrap();
        let ComposeSuccess::Loaded(Some(draft)) = draft else {
            panic!("expected released draft");
        };
        assert!(!draft.locked_for_delivery);
        runtime.shutdown().unwrap();
    }

    fn input(
        account: &AccountConfiguration,
        identity: Option<ComposeDraftIdentity>,
        body: &str,
    ) -> ComposeDraftInput {
        ComposeDraftInput::new(
            account.account_id,
            account.generation,
            identity,
            "bob@example.test",
            "Subject",
            body,
        )
        .unwrap()
    }

    async fn create_account(database: &DatabaseClient) -> AccountConfiguration {
        let input = AccountConfigInput::new_with_smtp(
            "0123456789abcdef0123456789abcdef",
            "Alice",
            "alice@example.test",
            AccountAuthKind::AppPassword,
            "alice@example.test",
            "imap.example.test",
            993,
            "smtp.example.test",
            465,
            SmtpSecurity::ImplicitTls,
            true,
            0x334455,
        )
        .unwrap();
        loop {
            match database.try_write_account(Box::new(AccountWrite::Create(input.clone()))) {
                Ok(reply) => {
                    let outcome = reply.await.unwrap().unwrap();
                    let AccountWriteOutcome::Saved(configuration) = outcome else {
                        panic!("expected saved account");
                    };
                    return configuration;
                }
                Err(failure) if failure.reason() == DatabaseSubmitError::Busy => {
                    time::sleep(DATABASE_RETRY_DELAY).await;
                }
                Err(_) => panic!("database actor closed"),
            }
        }
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                time::timeout(TEST_NOW_TIMEOUT, future)
                    .await
                    .expect("compose test timed out")
            })
    }

    struct TestPaths {
        database: PathBuf,
        content: PathBuf,
    }

    impl TestPaths {
        fn new(label: &str) -> Self {
            static NEXT_PATH: AtomicU64 = AtomicU64::new(1);
            let serial = NEXT_PATH.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "nivalis-compose-{label}-{}-{serial}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).unwrap();
            Self {
                database: root.join("mail.db"),
                content: root.join("content"),
            }
        }
    }

    impl Drop for TestPaths {
        fn drop(&mut self) {
            let root = self.database.parent().unwrap_or(Path::new("/nonexistent"));
            let _ = fs::remove_dir_all(root);
        }
    }
}
