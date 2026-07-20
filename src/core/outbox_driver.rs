use std::{
    future::Future,
    io,
    pin::Pin,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use tokio::{sync::mpsc, time};

use crate::{
    content::{
        ContentStaging, ReservedFileObservation, StagedFile, StorageError, StorageOperation,
    },
    credentials::{
        CredentialClient, CredentialFailureKind, CredentialLocator, CredentialOperation,
        CredentialOutcome, CredentialSubmitError, Secret,
    },
    network::smtp::{
        self, SmtpDataFence, SmtpDataFenceFailure, SmtpSecurity as NetworkSmtpSecurity,
        SmtpSubmissionCancelHandle, SmtpSubmissionCancellation, SmtpSubmissionFailure,
        SmtpSubmissionFailureKind, SmtpSubmissionReceipt, SmtpSubmissionRequest,
    },
    store::sqlite::{
        AccountAuthKind, AccountConfiguration, AccountId, AccountLifecycle, AccountRecord,
        ArtifactObservation, ComposeDbOperation, ComposeDbOutcome, DatabaseClient,
        DatabaseSubmitError, DbFailure, DraftSnapshot, FailureKind, MessageId, OutboxClaim,
        OutboxClaimOutcome, OutboxErrorClass, OutboxRecoveryOutcome, OutboxReport,
        OutboxReportOutcome, OutboxReservation, OutboxState, ReservationRecovery,
        SmtpSecurity as StoreSmtpSecurity,
    },
};

use super::outbound::{OutboundMailbox, PlainTextMessage};

const DATABASE_RETRY_DELAY: Duration = Duration::from_millis(10);
const MAX_IDLE_POLL: Duration = Duration::from_secs(30);
const RECOVERY_BATCH: usize = 16;
const MAX_OUTBOUND_MIME_BYTES: usize = 8 * 1024 * 1024;
const MIN_RETRY_DELAY_MS: i64 = 30 * 1_000;
const MAX_RETRY_DELAY_MS: i64 = 60 * 60 * 1_000;

pub(super) type SmtpSubmitFuture = Pin<
    Box<dyn Future<Output = Result<SmtpSubmissionReceipt, SmtpSubmissionFailure>> + Send + 'static>,
>;

/// The injected probe keeps the driver testable without changing its durable DATA fence.
pub(super) type SmtpSubmitProbe =
    fn(SmtpSubmissionRequest, SmtpSubmissionCancellation, SmtpDataFence) -> SmtpSubmitFuture;

pub(super) fn production_smtp_submit(
    request: SmtpSubmissionRequest,
    cancellation: SmtpSubmissionCancellation,
    data_fence: SmtpDataFence,
) -> SmtpSubmitFuture {
    Box::pin(smtp::submit_with_data_fence(
        request,
        Some(cancellation),
        data_fence,
    ))
}

#[derive(Clone, Default)]
pub(super) struct SmtpCancellationTracker {
    state: Arc<Mutex<SmtpCancellationState>>,
}

#[derive(Default)]
struct SmtpCancellationState {
    next_token: u64,
    shutdown_requested: bool,
    active: Option<ActiveSmtpCancellation>,
}

struct ActiveSmtpCancellation {
    message_id: MessageId,
    token: u64,
    handle: Option<SmtpSubmissionCancelHandle>,
}

impl SmtpCancellationTracker {
    pub(super) fn cancel(&self, message_id: MessageId) -> bool {
        let handle = {
            let mut state = self.lock();
            let Some(active) = state.active.as_mut() else {
                return false;
            };
            if active.message_id != message_id {
                return false;
            }
            active.handle.take()
        };
        let Some(handle) = handle else {
            return false;
        };
        handle.cancel()
    }

    pub(super) fn begin_shutdown(&self) {
        let handle = {
            let mut state = self.lock();
            state.shutdown_requested = true;
            state
                .active
                .as_mut()
                .and_then(|active| active.handle.take())
        };
        if let Some(handle) = handle {
            let _ = handle.cancel();
        }
    }

    fn is_shutting_down(&self) -> bool {
        self.lock().shutdown_requested
    }

    fn install(
        &self,
        message_id: MessageId,
        handle: SmtpSubmissionCancelHandle,
    ) -> ActiveSmtpCancellationGuard {
        let mut handle = Some(handle);
        let (token, cancel_immediately) = {
            let mut state = self.lock();
            state.next_token = state.next_token.wrapping_add(1).max(1);
            let token = state.next_token;
            let cancel_immediately = state.shutdown_requested;
            debug_assert!(state.active.is_none());
            state.active = Some(ActiveSmtpCancellation {
                message_id,
                token,
                handle: if cancel_immediately {
                    None
                } else {
                    handle.take()
                },
            });
            (token, cancel_immediately)
        };
        if cancel_immediately {
            let _ = handle
                .expect("shutdown retains the cancellation handle")
                .cancel();
        }
        ActiveSmtpCancellationGuard {
            tracker: self.clone(),
            token,
        }
    }

    fn clear(&self, token: u64) {
        let mut state = self.lock();
        if state.active.as_ref().map(|active| active.token) == Some(token) {
            state.active = None;
        }
    }

    fn lock(&self) -> MutexGuard<'_, SmtpCancellationState> {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }
}

struct ActiveSmtpCancellationGuard {
    tracker: SmtpCancellationTracker,
    token: u64,
}

impl Drop for ActiveSmtpCancellationGuard {
    fn drop(&mut self) {
        self.tracker.clear(self.token);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum OutboxDriverExit {
    WakeChannel,
    Database,
    CredentialChannel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OutboxDriverFault {
    Database,
    ContentStorage,
    Credential,
    InvalidSubmission,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OutboxStatus {
    AttemptStarted {
        message_id: MessageId,
        account_id: AccountId,
        attempt_count: u16,
    },
    StateChanged {
        message_id: MessageId,
        account_id: Option<AccountId>,
        state: OutboxState,
        wake_at_ms: Option<i64>,
    },
    Fault {
        message_id: Option<MessageId>,
        kind: OutboxDriverFault,
    },
}

/// Runs the one global outbox drainer on the existing core runtime.
///
/// `wakeups` is intentionally capacity one: a wake only means "scan durable state".
/// Status delivery is lossy and never backpressures durable delivery state.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_outbox_driver(
    database: DatabaseClient,
    credentials: CredentialClient,
    staging: Arc<ContentStaging>,
    mut wakeups: mpsc::Receiver<()>,
    statuses: mpsc::Sender<OutboxStatus>,
    file_gc_wakeups: mpsc::Sender<()>,
    cancellations: SmtpCancellationTracker,
    submit: SmtpSubmitProbe,
) -> OutboxDriverExit {
    loop {
        if cancellations.is_shutting_down() || wakeups.is_closed() {
            return OutboxDriverExit::WakeChannel;
        }
        for _ in 0..RECOVERY_BATCH {
            let recovery = match compose_call(
                &database,
                Box::new(ComposeDbOperation::RecoverOutbox { now_ms: now_ms() }),
            )
            .await
            {
                Ok(ComposeDbOutcome::OutboxRecovered(outcome)) => outcome,
                Ok(_) => {
                    emit_fault(&statuses, None, OutboxDriverFault::Database);
                    break;
                }
                Err(DbCallError::Closed) => return OutboxDriverExit::Database,
                Err(DbCallError::Failure(_)) => {
                    emit_fault(&statuses, None, OutboxDriverFault::Database);
                    break;
                }
            };

            match recovery {
                OutboxRecoveryOutcome::Idle => break,
                OutboxRecoveryOutcome::Recovered { message_id, state } => {
                    emit_state(&statuses, message_id, None, state, None);
                }
                OutboxRecoveryOutcome::Reservation(reservation) => {
                    match recover_reservation(&database, &staging, &statuses, reservation).await {
                        Ok(()) => {}
                        Err(WorkError::DatabaseClosed) => {
                            return OutboxDriverExit::Database;
                        }
                        Err(WorkError::CredentialClosed) => {
                            return OutboxDriverExit::CredentialChannel;
                        }
                        Err(WorkError::Fault { message_id, kind }) => {
                            emit_fault(&statuses, message_id, kind);
                            break;
                        }
                    }
                }
            }
        }

        let claim = match compose_call(
            &database,
            Box::new(ComposeDbOperation::ClaimNextOutbox { now_ms: now_ms() }),
        )
        .await
        {
            Ok(ComposeDbOutcome::OutboxClaimed(outcome)) => outcome,
            Ok(_) => {
                emit_fault(&statuses, None, OutboxDriverFault::Database);
                if !wait_for_wake(&mut wakeups, None).await {
                    return OutboxDriverExit::WakeChannel;
                }
                continue;
            }
            Err(DbCallError::Closed) => return OutboxDriverExit::Database,
            Err(DbCallError::Failure(_)) => {
                emit_fault(&statuses, None, OutboxDriverFault::Database);
                if !wait_for_wake(&mut wakeups, None).await {
                    return OutboxDriverExit::WakeChannel;
                }
                continue;
            }
        };

        match claim {
            OutboxClaimOutcome::Idle { wake_at_ms } => {
                if !wait_for_wake(&mut wakeups, wake_at_ms).await {
                    return OutboxDriverExit::WakeChannel;
                }
            }
            OutboxClaimOutcome::Claimed(claim) => {
                match submit_claim(
                    &database,
                    &credentials,
                    &staging,
                    &statuses,
                    &cancellations,
                    submit,
                    claim,
                )
                .await
                {
                    Ok(()) => {
                        // The database is the GC source of truth. This wake is independent of
                        // lossy UI status delivery and only asks the bounded janitor to rescan.
                        let _ = file_gc_wakeups.try_send(());
                    }
                    Err(WorkError::DatabaseClosed) => {
                        return OutboxDriverExit::Database;
                    }
                    Err(WorkError::CredentialClosed) => {
                        return OutboxDriverExit::CredentialChannel;
                    }
                    Err(WorkError::Fault { message_id, kind }) => {
                        emit_fault(&statuses, message_id, kind);
                    }
                }
            }
        }
    }
}

async fn recover_reservation(
    database: &DatabaseClient,
    staging: &ContentStaging,
    statuses: &mpsc::Sender<OutboxStatus>,
    reservation: OutboxReservation,
) -> Result<(), WorkError> {
    let observation = match staging
        .observe_reserved_file(&reservation.file_key, MAX_OUTBOUND_MIME_BYTES as u64)
    {
        Ok(ReservedFileObservation::Missing) => ArtifactObservation::Missing,
        Ok(ReservedFileObservation::Published { byte_count }) if byte_count > 0 => {
            ArtifactObservation::Published { byte_count }
        }
        Ok(ReservedFileObservation::Published { .. }) => ArtifactObservation::Invalid,
        Err(error) if error.kind == io::ErrorKind::InvalidInput => ArtifactObservation::Invalid,
        Err(_) => {
            return Err(WorkError::fault(
                Some(reservation.message_id),
                OutboxDriverFault::ContentStorage,
            ));
        }
    };

    let outcome = recover_reservation_record(database, &reservation, observation).await?;
    match outcome {
        ReservationRecovery::Ready => {
            emit_state(
                statuses,
                reservation.message_id,
                Some(reservation.account_id),
                OutboxState::Ready,
                None,
            );
            Ok(())
        }
        ReservationRecovery::PermanentFailure => {
            emit_state(
                statuses,
                reservation.message_id,
                Some(reservation.account_id),
                OutboxState::PermanentFailure,
                None,
            );
            Ok(())
        }
        ReservationRecovery::Stale => Ok(()),
        ReservationRecovery::Rebuild(renewed) => {
            rebuild_reservation(database, staging, statuses, renewed).await
        }
    }
}

async fn rebuild_reservation(
    database: &DatabaseClient,
    staging: &ContentStaging,
    statuses: &mpsc::Sender<OutboxStatus>,
    reservation: OutboxReservation,
) -> Result<(), WorkError> {
    let draft = match load_draft(database, reservation.message_id).await? {
        Some(draft) if reservation_matches_draft(&reservation, &draft) => draft,
        _ => return invalidate_reservation(database, statuses, reservation).await,
    };
    let configuration = match load_account(database, reservation.account_id).await {
        Ok(AccountRecord::Configured(configuration))
            if reservation_matches_account(&reservation, &configuration) =>
        {
            configuration
        }
        Ok(_) => return invalidate_reservation(database, statuses, reservation).await,
        Err(DbCallError::Closed) => return Err(WorkError::DatabaseClosed),
        Err(DbCallError::Failure(failure))
            if matches!(failure.kind, FailureKind::NotFound | FailureKind::Conflict) =>
        {
            return invalidate_reservation(database, statuses, reservation).await;
        }
        Err(DbCallError::Failure(_)) => {
            return Err(WorkError::fault(
                Some(reservation.message_id),
                OutboxDriverFault::Database,
            ));
        }
    };

    let staged = match stage_reserved_mime(staging, &reservation, &draft, &configuration) {
        Ok(staged) => staged,
        Err(ReservedMimeError::InvalidInput) => {
            return invalidate_reservation(database, statuses, reservation).await;
        }
        Err(ReservedMimeError::Storage) => {
            return Err(WorkError::fault(
                Some(reservation.message_id),
                OutboxDriverFault::ContentStorage,
            ));
        }
    };
    let byte_count = staged.byte_count();
    let mut published = staged.publish().map_err(|_| {
        WorkError::fault(
            Some(reservation.message_id),
            OutboxDriverFault::ContentStorage,
        )
    })?;

    // From this point the durable reservation, not stack unwinding, owns the artifact.
    published.retain();
    let outcome = recover_reservation_record(
        database,
        &reservation,
        ArtifactObservation::Published { byte_count },
    )
    .await?;
    match outcome {
        ReservationRecovery::Ready => emit_state(
            statuses,
            reservation.message_id,
            Some(reservation.account_id),
            OutboxState::Ready,
            None,
        ),
        ReservationRecovery::PermanentFailure => emit_state(
            statuses,
            reservation.message_id,
            Some(reservation.account_id),
            OutboxState::PermanentFailure,
            None,
        ),
        ReservationRecovery::Rebuild(_) | ReservationRecovery::Stale => {}
    }
    Ok(())
}

async fn invalidate_reservation(
    database: &DatabaseClient,
    statuses: &mpsc::Sender<OutboxStatus>,
    reservation: OutboxReservation,
) -> Result<(), WorkError> {
    let outcome =
        recover_reservation_record(database, &reservation, ArtifactObservation::Invalid).await?;
    if outcome == ReservationRecovery::PermanentFailure {
        emit_state(
            statuses,
            reservation.message_id,
            Some(reservation.account_id),
            OutboxState::PermanentFailure,
            None,
        );
    }
    Ok(())
}

async fn recover_reservation_record(
    database: &DatabaseClient,
    reservation: &OutboxReservation,
    observation: ArtifactObservation,
) -> Result<ReservationRecovery, WorkError> {
    match compose_call(
        database,
        Box::new(ComposeDbOperation::RecoverReservation {
            reservation: reservation.clone(),
            observation,
            now_ms: now_ms(),
        }),
    )
    .await
    {
        Ok(ComposeDbOutcome::ReservationRecovered(outcome)) => Ok(outcome),
        Ok(_) => Err(WorkError::fault(
            Some(reservation.message_id),
            OutboxDriverFault::Database,
        )),
        Err(DbCallError::Closed) => Err(WorkError::DatabaseClosed),
        Err(DbCallError::Failure(_)) => Err(WorkError::fault(
            Some(reservation.message_id),
            OutboxDriverFault::Database,
        )),
    }
}

pub(super) fn stage_reserved_mime(
    staging: &ContentStaging,
    reservation: &OutboxReservation,
    draft: &DraftSnapshot,
    configuration: &AccountConfiguration,
) -> Result<StagedFile, ReservedMimeError> {
    if !reservation_matches_draft(reservation, draft)
        || !reservation_matches_account(reservation, configuration)
    {
        return Err(ReservedMimeError::InvalidInput);
    }
    let from = OutboundMailbox::new(&configuration.address, &configuration.name)
        .map_err(|_| ReservedMimeError::InvalidInput)?;
    let recipients = draft
        .recipients
        .iter()
        .map(|recipient| OutboundMailbox::new(&recipient.address, &recipient.display_name))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| ReservedMimeError::InvalidInput)?;
    let message = PlainTextMessage::new(
        from,
        &recipients,
        &draft.subject,
        &reservation.rfc_message_id,
        draft.updated_at_ms.div_euclid(1_000),
    )
    .map_err(|_| ReservedMimeError::InvalidInput)?;
    let mut body = staging
        .open_file(&draft.body_file_key)
        .map_err(|_| ReservedMimeError::Storage)?;
    let body_bytes = body
        .metadata()
        .map_err(|_| ReservedMimeError::Storage)?
        .len();
    if body_bytes != draft.body_byte_count {
        return Err(ReservedMimeError::InvalidInput);
    }
    staging
        .stage_writer_at(&reservation.file_key, MAX_OUTBOUND_MIME_BYTES, |output| {
            message.write_to(output, &mut body)
        })
        .map_err(classify_mime_storage_error)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReservedMimeError {
    InvalidInput,
    Storage,
}

fn classify_mime_storage_error(error: StorageError) -> ReservedMimeError {
    if error.operation == StorageOperation::WriteTemporary
        && matches!(
            error.kind,
            io::ErrorKind::InvalidInput | io::ErrorKind::FileTooLarge
        )
    {
        ReservedMimeError::InvalidInput
    } else {
        ReservedMimeError::Storage
    }
}

fn reservation_matches_draft(reservation: &OutboxReservation, draft: &DraftSnapshot) -> bool {
    draft.message_id == reservation.message_id
        && draft.account_id == reservation.account_id
        && draft.revision == reservation.draft_revision
        && draft.locked_artifact_generation == Some(reservation.artifact_generation)
}

fn reservation_matches_account(
    reservation: &OutboxReservation,
    configuration: &AccountConfiguration,
) -> bool {
    configuration.account_id == reservation.account_id
        && configuration.generation == reservation.configuration_generation
        && configuration.auth_kind == AccountAuthKind::AppPassword
        && configuration.lifecycle == AccountLifecycle::Active
        && configuration.smtp_configured
}

async fn submit_claim(
    database: &DatabaseClient,
    credentials: &CredentialClient,
    staging: &ContentStaging,
    statuses: &mpsc::Sender<OutboxStatus>,
    cancellations: &SmtpCancellationTracker,
    submit: SmtpSubmitProbe,
    claim: Box<OutboxClaim>,
) -> Result<(), WorkError> {
    emit_status(
        statuses,
        OutboxStatus::AttemptStarted {
            message_id: claim.lease.message_id,
            account_id: claim.account_id,
            attempt_count: claim.attempt_count,
        },
    );

    let secret = match load_credential(credentials, &claim.credential_key).await {
        Ok(secret) => secret,
        Err(CredentialCallError::Closed) => return Err(WorkError::CredentialClosed),
        Err(CredentialCallError::Failure(kind)) => {
            let plan = credential_failure_plan(kind, claim.attempt_count, now_ms())?;
            return report_plan(database, statuses, &claim, plan).await;
        }
    };
    let mime_file = match staging.open_file(&claim.file_key) {
        Ok(file) => file,
        Err(_) => {
            let report = OutboxReport::permanent_failure(
                OutboxErrorClass::Permanent,
                "outbound_file_unavailable",
            )
            .map_err(|_| {
                WorkError::fault(
                    Some(claim.lease.message_id),
                    OutboxDriverFault::InvalidSubmission,
                )
            })?;
            return report_plan(
                database,
                statuses,
                &claim,
                ReportPlan::terminal(report, OutboxState::PermanentFailure),
            )
            .await;
        }
    };
    let request = match submission_request(&claim, secret, mime_file) {
        Ok(request) => request,
        Err(()) => {
            let report = OutboxReport::permanent_failure(
                OutboxErrorClass::Configuration,
                "invalid_smtp_submission",
            )
            .map_err(|_| {
                WorkError::fault(
                    Some(claim.lease.message_id),
                    OutboxDriverFault::InvalidSubmission,
                )
            })?;
            return report_plan(
                database,
                statuses,
                &claim,
                ReportPlan::terminal(report, OutboxState::PermanentFailure),
            )
            .await;
        }
    };

    let fence_database = database.clone();
    let lease = claim.lease;
    let data_fence: SmtpDataFence = Box::pin(async move {
        match compose_call(
            &fence_database,
            Box::new(ComposeDbOperation::MarkDataStarted {
                lease,
                now_ms: now_ms(),
            }),
        )
        .await
        {
            Ok(ComposeDbOutcome::DataStarted(OutboxReportOutcome::Applied(
                OutboxState::InFlight,
            ))) => Ok(()),
            Ok(_) | Err(_) => Err(SmtpDataFenceFailure::new()),
        }
    });
    let (cancel_handle, cancellation) = smtp::smtp_submission_cancellation_pair();
    let active_cancellation = cancellations.install(claim.lease.message_id, cancel_handle);
    let result = submit(request, cancellation, data_fence).await;
    drop(active_cancellation);
    let completed_at_ms = now_ms();
    let plan = match result {
        Ok(_) => ReportPlan::terminal(
            OutboxReport::delivered(completed_at_ms).map_err(|_| {
                WorkError::fault(Some(claim.lease.message_id), OutboxDriverFault::Database)
            })?,
            OutboxState::Delivered,
        ),
        Err(failure) => smtp_failure_plan(failure, claim.attempt_count, completed_at_ms)?,
    };
    report_plan(database, statuses, &claim, plan).await
}

fn submission_request(
    claim: &OutboxClaim,
    secret: Secret,
    mime_file: std::fs::File,
) -> Result<SmtpSubmissionRequest, ()> {
    let security = match claim.smtp_security {
        StoreSmtpSecurity::ImplicitTls => NetworkSmtpSecurity::ImplicitTls,
        StoreSmtpSecurity::StartTls => NetworkSmtpSecurity::StartTls,
    };
    SmtpSubmissionRequest::new(
        &claim.smtp_host,
        claim.smtp_port,
        &claim.login_name,
        secret,
        security,
        &claim.envelope_from,
        claim
            .recipients
            .iter()
            .map(|recipient| recipient.address.as_ref()),
        mime_file,
        claim.wire_byte_count,
    )
    .map_err(|_| ())
}

async fn report_plan(
    database: &DatabaseClient,
    statuses: &mpsc::Sender<OutboxStatus>,
    claim: &OutboxClaim,
    plan: ReportPlan,
) -> Result<(), WorkError> {
    let outcome = match compose_call(
        database,
        Box::new(ComposeDbOperation::ReportOutbox {
            lease: claim.lease,
            report: plan.report,
            now_ms: now_ms(),
        }),
    )
    .await
    {
        Ok(ComposeDbOutcome::OutboxReported(outcome)) => outcome,
        Ok(_) => {
            return Err(WorkError::fault(
                Some(claim.lease.message_id),
                OutboxDriverFault::Database,
            ));
        }
        Err(DbCallError::Closed) => return Err(WorkError::DatabaseClosed),
        Err(DbCallError::Failure(_)) => {
            return Err(WorkError::fault(
                Some(claim.lease.message_id),
                OutboxDriverFault::Database,
            ));
        }
    };
    if let OutboxReportOutcome::Applied(state) = outcome {
        let wake_at_ms = if state == plan.state {
            plan.wake_at_ms
        } else {
            None
        };
        emit_state(
            statuses,
            claim.lease.message_id,
            Some(claim.account_id),
            state,
            wake_at_ms,
        );
    }
    Ok(())
}

struct ReportPlan {
    report: OutboxReport,
    state: OutboxState,
    wake_at_ms: Option<i64>,
}

impl ReportPlan {
    fn terminal(report: OutboxReport, state: OutboxState) -> Self {
        Self {
            report,
            state,
            wake_at_ms: None,
        }
    }
}

fn credential_failure_plan(
    kind: CredentialFailureKind,
    attempt_count: u16,
    now_ms: i64,
) -> Result<ReportPlan, WorkError> {
    match kind {
        CredentialFailureKind::LockedOrDenied | CredentialFailureKind::Unavailable => retry_plan(
            OutboxErrorClass::Authentication,
            "credential_temporarily_unavailable",
            attempt_count,
            now_ms,
        ),
        CredentialFailureKind::Missing
        | CredentialFailureKind::InvalidInput
        | CredentialFailureKind::Ambiguous
        | CredentialFailureKind::CorruptData
        | CredentialFailureKind::Unsupported
        | CredentialFailureKind::RandomUnavailable => Ok(ReportPlan::terminal(
            OutboxReport::permanent_failure(
                OutboxErrorClass::Authentication,
                "credential_unavailable",
            )
            .map_err(|_| WorkError::fault(None, OutboxDriverFault::Credential))?,
            OutboxState::PermanentFailure,
        )),
    }
}

fn smtp_failure_plan(
    failure: SmtpSubmissionFailure,
    attempt_count: u16,
    now_ms: i64,
) -> Result<ReportPlan, WorkError> {
    match failure.kind {
        SmtpSubmissionFailureKind::Retryable
        | SmtpSubmissionFailureKind::Timeout
        | SmtpSubmissionFailureKind::Cancelled => retry_plan(
            OutboxErrorClass::Network,
            smtp_error_code(failure.kind),
            attempt_count,
            now_ms,
        ),
        SmtpSubmissionFailureKind::TransientRejection => retry_after_rejection_plan(
            OutboxErrorClass::Network,
            smtp_error_code(failure.kind),
            attempt_count,
            now_ms,
        ),
        SmtpSubmissionFailureKind::LocalState => retry_plan(
            OutboxErrorClass::Protocol,
            "data_fence_rejected",
            attempt_count,
            now_ms,
        ),
        SmtpSubmissionFailureKind::Uncertain => Ok(ReportPlan::terminal(
            OutboxReport::uncertain("smtp_acceptance_uncertain")
                .map_err(|_| WorkError::fault(None, OutboxDriverFault::Database))?,
            OutboxState::Uncertain,
        )),
        SmtpSubmissionFailureKind::Authentication => Ok(ReportPlan::terminal(
            OutboxReport::permanent_failure(
                OutboxErrorClass::Authentication,
                "smtp_authentication_rejected",
            )
            .map_err(|_| WorkError::fault(None, OutboxDriverFault::Database))?,
            OutboxState::PermanentFailure,
        )),
        SmtpSubmissionFailureKind::Certificate => Ok(ReportPlan::terminal(
            OutboxReport::permanent_failure(
                OutboxErrorClass::Configuration,
                "smtp_certificate_rejected",
            )
            .map_err(|_| WorkError::fault(None, OutboxDriverFault::Database))?,
            OutboxState::PermanentFailure,
        )),
        SmtpSubmissionFailureKind::Permanent => Ok(ReportPlan::terminal(
            OutboxReport::permanent_failure(
                OutboxErrorClass::Permanent,
                "smtp_permanent_rejection",
            )
            .map_err(|_| WorkError::fault(None, OutboxDriverFault::Database))?,
            OutboxState::PermanentFailure,
        )),
        SmtpSubmissionFailureKind::Protocol => Ok(ReportPlan::terminal(
            OutboxReport::permanent_failure(OutboxErrorClass::Protocol, "smtp_protocol_error")
                .map_err(|_| WorkError::fault(None, OutboxDriverFault::Database))?,
            OutboxState::PermanentFailure,
        )),
        SmtpSubmissionFailureKind::ResourceLimit | SmtpSubmissionFailureKind::LocalFile => {
            Ok(ReportPlan::terminal(
                OutboxReport::permanent_failure(
                    OutboxErrorClass::Permanent,
                    smtp_error_code(failure.kind),
                )
                .map_err(|_| WorkError::fault(None, OutboxDriverFault::Database))?,
                OutboxState::PermanentFailure,
            ))
        }
    }
}

fn retry_plan(
    error_class: OutboxErrorClass,
    error_code: &'static str,
    attempt_count: u16,
    now_ms: i64,
) -> Result<ReportPlan, WorkError> {
    let wake_at_ms = now_ms
        .checked_add(retry_delay_ms(attempt_count))
        .ok_or_else(|| WorkError::fault(None, OutboxDriverFault::Database))?;
    let report = OutboxReport::retry(wake_at_ms, error_class, error_code)
        .map_err(|_| WorkError::fault(None, OutboxDriverFault::Database))?;
    Ok(ReportPlan {
        report,
        state: OutboxState::RetryWait,
        wake_at_ms: Some(wake_at_ms),
    })
}

fn retry_after_rejection_plan(
    error_class: OutboxErrorClass,
    error_code: &'static str,
    attempt_count: u16,
    now_ms: i64,
) -> Result<ReportPlan, WorkError> {
    let wake_at_ms = now_ms
        .checked_add(retry_delay_ms(attempt_count))
        .ok_or_else(|| WorkError::fault(None, OutboxDriverFault::Database))?;
    let report = OutboxReport::retry_after_rejection(wake_at_ms, error_class, error_code)
        .map_err(|_| WorkError::fault(None, OutboxDriverFault::Database))?;
    Ok(ReportPlan {
        report,
        state: OutboxState::RetryWait,
        wake_at_ms: Some(wake_at_ms),
    })
}

fn retry_delay_ms(attempt_count: u16) -> i64 {
    let shift = u32::from(attempt_count.saturating_sub(1).min(7));
    MIN_RETRY_DELAY_MS
        .saturating_mul(1_i64 << shift)
        .min(MAX_RETRY_DELAY_MS)
}

fn smtp_error_code(kind: SmtpSubmissionFailureKind) -> &'static str {
    match kind {
        SmtpSubmissionFailureKind::Retryable => "smtp_retryable",
        SmtpSubmissionFailureKind::TransientRejection => "smtp_transient_rejection",
        SmtpSubmissionFailureKind::Timeout => "smtp_timeout",
        SmtpSubmissionFailureKind::Cancelled => "smtp_cancelled",
        SmtpSubmissionFailureKind::ResourceLimit => "smtp_resource_limit",
        SmtpSubmissionFailureKind::LocalFile => "outbound_file_invalid",
        SmtpSubmissionFailureKind::Authentication => "smtp_authentication_rejected",
        SmtpSubmissionFailureKind::Permanent => "smtp_permanent_rejection",
        SmtpSubmissionFailureKind::Certificate => "smtp_certificate_rejected",
        SmtpSubmissionFailureKind::Protocol => "smtp_protocol_error",
        SmtpSubmissionFailureKind::LocalState => "data_fence_rejected",
        SmtpSubmissionFailureKind::Uncertain => "smtp_acceptance_uncertain",
    }
}

async fn load_draft(
    database: &DatabaseClient,
    message_id: MessageId,
) -> Result<Option<DraftSnapshot>, WorkError> {
    match compose_call(
        database,
        Box::new(ComposeDbOperation::LoadDraft { message_id }),
    )
    .await
    {
        Ok(ComposeDbOutcome::Draft(draft)) => Ok(draft),
        Ok(_) => Err(WorkError::fault(
            Some(message_id),
            OutboxDriverFault::Database,
        )),
        Err(DbCallError::Closed) => Err(WorkError::DatabaseClosed),
        Err(DbCallError::Failure(failure))
            if matches!(failure.kind, FailureKind::NotFound | FailureKind::Conflict) =>
        {
            Ok(None)
        }
        Err(DbCallError::Failure(_)) => Err(WorkError::fault(
            Some(message_id),
            OutboxDriverFault::Database,
        )),
    }
}

async fn load_account(
    database: &DatabaseClient,
    account_id: AccountId,
) -> Result<AccountRecord, DbCallError> {
    loop {
        let receiver = match database.try_load_account(account_id) {
            Ok(receiver) => receiver,
            Err(DatabaseSubmitError::Busy) => {
                time::sleep(DATABASE_RETRY_DELAY).await;
                continue;
            }
            Err(DatabaseSubmitError::Closed) => return Err(DbCallError::Closed),
        };
        match receiver.await {
            Ok(Ok(record)) => return Ok(record),
            Ok(Err(failure)) if is_database_busy(&failure) => {
                time::sleep(DATABASE_RETRY_DELAY).await;
            }
            Ok(Err(failure)) => return Err(DbCallError::Failure(failure)),
            Err(_) => return Err(DbCallError::Closed),
        }
    }
}

async fn load_credential(
    credentials: &CredentialClient,
    key: &str,
) -> Result<Secret, CredentialCallError> {
    let locator = CredentialLocator::parse(key)
        .map_err(|failure| CredentialCallError::Failure(failure.kind))?;
    loop {
        let operation = CredentialOperation::Load {
            locator: locator.clone(),
        };
        let response = match credentials.try_submit(operation) {
            Ok(response) => response,
            Err(failure) if failure.reason() == CredentialSubmitError::Busy => {
                time::sleep(DATABASE_RETRY_DELAY).await;
                continue;
            }
            Err(_) => return Err(CredentialCallError::Closed),
        };
        match response.await {
            Ok(Ok(CredentialOutcome::Loaded(secret))) => return Ok(secret),
            Ok(Ok(_)) => {
                return Err(CredentialCallError::Failure(
                    CredentialFailureKind::CorruptData,
                ));
            }
            Ok(Err(failure)) => return Err(CredentialCallError::Failure(failure.kind)),
            Err(_) => return Err(CredentialCallError::Closed),
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

fn is_database_busy(failure: &DbFailure) -> bool {
    if failure.kind != FailureKind::Database {
        return false;
    }
    let message = failure.message.as_ref();
    message.contains("database is locked")
        || message.contains("database is busy")
        || message.contains("SQLITE_BUSY")
        || message.contains("SQLITE_LOCKED")
}

async fn wait_for_wake(wakeups: &mut mpsc::Receiver<()>, wake_at_ms: Option<i64>) -> bool {
    let delay = wake_at_ms
        .map(|deadline| deadline.saturating_sub(now_ms()))
        .map_or(MAX_IDLE_POLL, |milliseconds| {
            Duration::from_millis(u64::try_from(milliseconds.max(0)).unwrap_or(u64::MAX))
                .min(MAX_IDLE_POLL)
        });
    if delay.is_zero() {
        return true;
    }
    match time::timeout(delay, wakeups.recv()).await {
        Ok(Some(())) | Err(_) => true,
        Ok(None) => false,
    }
}

fn emit_state(
    statuses: &mpsc::Sender<OutboxStatus>,
    message_id: MessageId,
    account_id: Option<AccountId>,
    state: OutboxState,
    wake_at_ms: Option<i64>,
) {
    emit_status(
        statuses,
        OutboxStatus::StateChanged {
            message_id,
            account_id,
            state,
            wake_at_ms,
        },
    );
}

fn emit_fault(
    statuses: &mpsc::Sender<OutboxStatus>,
    message_id: Option<MessageId>,
    kind: OutboxDriverFault,
) {
    emit_status(statuses, OutboxStatus::Fault { message_id, kind });
}

fn emit_status(statuses: &mpsc::Sender<OutboxStatus>, status: OutboxStatus) {
    let _ = statuses.try_send(status);
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}

enum DbCallError {
    Closed,
    Failure(DbFailure),
}

enum CredentialCallError {
    Closed,
    Failure(CredentialFailureKind),
}

#[derive(Debug)]
enum WorkError {
    DatabaseClosed,
    CredentialClosed,
    Fault {
        message_id: Option<MessageId>,
        kind: OutboxDriverFault,
    },
}

impl WorkError {
    fn fault(message_id: Option<MessageId>, kind: OutboxDriverFault) -> Self {
        Self::Fault { message_id, kind }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        future::poll_fn,
        path::PathBuf,
        sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        task::Poll,
    };

    use keyring_core::CredentialStore;
    use rusqlite::{Connection, OptionalExtension};

    use super::*;
    use crate::{
        core::compose::{ComposeDraftInput, ComposeOperation, ComposeSuccess, execute_compose},
        credentials::{self, CredentialOperation, CredentialOutcome},
        store::sqlite::{
            self, AccountConfigInput, AccountWrite, AccountWriteOutcome, ComposeDbOperation,
            ComposeDbOutcome, DatabaseReplies, DatabaseRuntime, OutboxActionFence,
            OutboxClaimOutcome, SmtpSecurity,
        },
    };

    const CREDENTIAL_KEY: &str = "fedcba9876543210fedcba9876543210";
    const TEST_TIMEOUT: Duration = Duration::from_secs(3);
    static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);
    static RETRY_DATA_FENCES: AtomicUsize = AtomicUsize::new(0);
    static DISCONNECT_DATA_FENCES: AtomicUsize = AtomicUsize::new(0);
    static UNEXPECTED_SUBMISSIONS: AtomicUsize = AtomicUsize::new(0);
    static PRE_DATA_CANCEL_READY: AtomicBool = AtomicBool::new(false);
    static SHUTDOWN_PRE_DATA_READY: AtomicBool = AtomicBool::new(false);

    struct DriverFixture {
        root: PathBuf,
        database_path: PathBuf,
        database: DatabaseClient,
        _database_replies: DatabaseReplies,
        database_runtime: Option<DatabaseRuntime>,
        credentials: CredentialClient,
        credential_runtime: Option<crate::credentials::CredentialRuntime>,
        credential_store: Arc<CredentialStore>,
        staging: Arc<ContentStaging>,
        account: AccountConfiguration,
        message_id: MessageId,
    }

    impl DriverFixture {
        async fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "nivalis-outbox-driver-{}-{}",
                std::process::id(),
                NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
            ));
            let database_path = root.join("mail.db");
            let content_root = root.join("content");
            let (database, database_replies, database_runtime, _) =
                sqlite::spawn(database_path.clone()).expect("start outbox test database");
            let account = create_account(&database).await;
            let credential_store: Arc<CredentialStore> =
                keyring_core::mock::Store::new().expect("create mock credential store");
            let factory_store = Arc::clone(&credential_store);
            let (credentials, credential_runtime) =
                credentials::spawn_with_test_factory(move || Ok(Arc::clone(&factory_store)));
            let stored = credentials
                .try_submit(CredentialOperation::Store {
                    locator: CredentialLocator::parse(CREDENTIAL_KEY).unwrap(),
                    secret: Secret::new(b"smtp-secret".to_vec()).unwrap(),
                })
                .unwrap()
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(stored, CredentialOutcome::Stored));
            let staging = Arc::new(ContentStaging::open(content_root).unwrap());
            let (wakeups, mut wake_receiver) = mpsc::channel(1);
            let queued = execute_compose(
                &database,
                &staging,
                &wakeups,
                ComposeOperation::Queue(
                    ComposeDraftInput::new(
                        account.account_id,
                        account.generation,
                        None,
                        "recipient@example.test",
                        "Recovery subject",
                        "Recovery body",
                    )
                    .unwrap(),
                ),
            )
            .await
            .unwrap();
            let ComposeSuccess::Queued { draft, .. } = queued else {
                panic!("fixture must queue an outbound message")
            };
            assert_eq!(wake_receiver.try_recv(), Ok(()));

            Self {
                root,
                database_path,
                database,
                _database_replies: database_replies,
                database_runtime: Some(database_runtime),
                credentials,
                credential_runtime: Some(credential_runtime),
                credential_store,
                staging,
                account,
                message_id: draft.message_id,
            }
        }

        fn stop_actors(&mut self) {
            if let Some(runtime) = self.credential_runtime.take() {
                runtime.shutdown().unwrap();
            }
            if let Some(runtime) = self.database_runtime.take() {
                runtime.shutdown().unwrap();
            }
        }
    }

    impl Drop for DriverFixture {
        fn drop(&mut self) {
            self.stop_actors();
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    async fn create_account(database: &DatabaseClient) -> AccountConfiguration {
        let input = AccountConfigInput::new_with_smtp(
            CREDENTIAL_KEY,
            "Sender",
            "sender@example.test",
            AccountAuthKind::AppPassword,
            "sender@example.test",
            "imap.example.test",
            993,
            "smtp.example.test",
            465,
            SmtpSecurity::ImplicitTls,
            true,
            0x335577,
        )
        .unwrap();
        let outcome = database
            .try_write_account(Box::new(AccountWrite::Create(input)))
            .unwrap()
            .await
            .unwrap()
            .unwrap();
        let AccountWriteOutcome::Saved(configuration) = outcome else {
            panic!("fixture account must be created")
        };
        configuration
    }

    async fn database_call(
        database: &DatabaseClient,
        operation: ComposeDbOperation,
    ) -> ComposeDbOutcome {
        match compose_call(database, Box::new(operation)).await {
            Ok(outcome) => outcome,
            Err(_) => panic!("compose database operation failed"),
        }
    }

    async fn claim(database: &DatabaseClient, claim_at_ms: i64) -> Box<OutboxClaim> {
        let ComposeDbOutcome::OutboxClaimed(OutboxClaimOutcome::Claimed(claim)) = database_call(
            database,
            ComposeDbOperation::ClaimNextOutbox {
                now_ms: claim_at_ms,
            },
        )
        .await
        else {
            panic!("expected claimable outbox item")
        };
        claim
    }

    async fn expect_state(
        statuses: &mut mpsc::Receiver<OutboxStatus>,
        expected: OutboxState,
    ) -> (MessageId, Option<i64>) {
        time::timeout(TEST_TIMEOUT, async {
            loop {
                match statuses.recv().await {
                    Some(OutboxStatus::StateChanged {
                        message_id,
                        state,
                        wake_at_ms,
                        ..
                    }) if state == expected => return (message_id, wake_at_ms),
                    Some(OutboxStatus::Fault { kind, .. }) => {
                        panic!("outbox driver faulted: {kind:?}")
                    }
                    Some(_) => {}
                    None => panic!("outbox status channel closed"),
                }
            }
        })
        .await
        .expect("outbox state timed out")
    }

    fn pre_data_retryable(
        _request: SmtpSubmissionRequest,
        _cancellation: SmtpSubmissionCancellation,
        _data_fence: SmtpDataFence,
    ) -> SmtpSubmitFuture {
        Box::pin(async {
            Err(SmtpSubmissionFailure {
                stage: smtp::SmtpSubmissionStage::Connect,
                kind: SmtpSubmissionFailureKind::Retryable,
            })
        })
    }

    fn wait_for_pre_data_cancellation(
        _request: SmtpSubmissionRequest,
        cancellation: SmtpSubmissionCancellation,
        _data_fence: SmtpDataFence,
    ) -> SmtpSubmitFuture {
        PRE_DATA_CANCEL_READY.store(true, Ordering::Release);
        Box::pin(async move {
            cancellation.cancelled().await;
            Err(SmtpSubmissionFailure {
                stage: smtp::SmtpSubmissionStage::Connect,
                kind: SmtpSubmissionFailureKind::Cancelled,
            })
        })
    }

    fn wait_for_shutdown_before_data(
        _request: SmtpSubmissionRequest,
        cancellation: SmtpSubmissionCancellation,
        _data_fence: SmtpDataFence,
    ) -> SmtpSubmitFuture {
        SHUTDOWN_PRE_DATA_READY.store(true, Ordering::Release);
        Box::pin(async move {
            cancellation.cancelled().await;
            Err(SmtpSubmissionFailure {
                stage: smtp::SmtpSubmissionStage::Connect,
                kind: SmtpSubmissionFailureKind::Cancelled,
            })
        })
    }

    fn authentication_rejected(
        _request: SmtpSubmissionRequest,
        _cancellation: SmtpSubmissionCancellation,
        _data_fence: SmtpDataFence,
    ) -> SmtpSubmitFuture {
        Box::pin(async {
            Err(SmtpSubmissionFailure {
                stage: smtp::SmtpSubmissionStage::Authenticate,
                kind: SmtpSubmissionFailureKind::Authentication,
            })
        })
    }

    fn delivered_after_fence(
        _request: SmtpSubmissionRequest,
        _cancellation: SmtpSubmissionCancellation,
        data_fence: SmtpDataFence,
    ) -> SmtpSubmitFuture {
        Box::pin(async move {
            data_fence.await.map_err(|_| SmtpSubmissionFailure {
                stage: smtp::SmtpSubmissionStage::DataFence,
                kind: SmtpSubmissionFailureKind::LocalState,
            })?;
            RETRY_DATA_FENCES.fetch_add(1, Ordering::AcqRel);
            Ok(SmtpSubmissionReceipt {
                response_code: 250,
                wire_byte_count: 1,
            })
        })
    }

    fn disconnected_after_fence(
        _request: SmtpSubmissionRequest,
        _cancellation: SmtpSubmissionCancellation,
        data_fence: SmtpDataFence,
    ) -> SmtpSubmitFuture {
        Box::pin(async move {
            data_fence.await.map_err(|_| SmtpSubmissionFailure {
                stage: smtp::SmtpSubmissionStage::DataFence,
                kind: SmtpSubmissionFailureKind::LocalState,
            })?;
            DISCONNECT_DATA_FENCES.fetch_add(1, Ordering::AcqRel);
            Err(SmtpSubmissionFailure {
                stage: smtp::SmtpSubmissionStage::Body,
                kind: SmtpSubmissionFailureKind::Uncertain,
            })
        })
    }

    fn transient_rejection_after_fence(
        _request: SmtpSubmissionRequest,
        _cancellation: SmtpSubmissionCancellation,
        data_fence: SmtpDataFence,
    ) -> SmtpSubmitFuture {
        Box::pin(async move {
            data_fence.await.map_err(|_| SmtpSubmissionFailure {
                stage: smtp::SmtpSubmissionStage::DataFence,
                kind: SmtpSubmissionFailureKind::LocalState,
            })?;
            Err(SmtpSubmissionFailure {
                stage: smtp::SmtpSubmissionStage::Body,
                kind: SmtpSubmissionFailureKind::TransientRejection,
            })
        })
    }

    fn unexpected_submission(
        _request: SmtpSubmissionRequest,
        _cancellation: SmtpSubmissionCancellation,
        _data_fence: SmtpDataFence,
    ) -> SmtpSubmitFuture {
        UNEXPECTED_SUBMISSIONS.fetch_add(1, Ordering::AcqRel);
        Box::pin(async {
            Err(SmtpSubmissionFailure {
                stage: smtp::SmtpSubmissionStage::Connect,
                kind: SmtpSubmissionFailureKind::Retryable,
            })
        })
    }

    fn block_on<F: Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                time::timeout(TEST_TIMEOUT, future)
                    .await
                    .expect("outbox driver test timed out")
            })
    }

    fn stored_outbox_state(database_path: &PathBuf, message_id: MessageId) -> Option<String> {
        Connection::open(database_path)
            .unwrap()
            .query_row(
                "SELECT state FROM outbox WHERE message_id = ?1",
                [message_id.get()],
                |row| row.get(0),
            )
            .optional()
            .unwrap()
    }

    #[test]
    fn retry_delay_is_bounded_exponential() {
        assert_eq!(retry_delay_ms(0), 30_000);
        assert_eq!(retry_delay_ms(1), 30_000);
        assert_eq!(retry_delay_ms(2), 60_000);
        assert_eq!(retry_delay_ms(7), 1_920_000);
        assert_eq!(retry_delay_ms(8), 3_600_000);
        assert_eq!(retry_delay_ms(u16::MAX), 3_600_000);
    }

    #[test]
    fn smtp_outcomes_do_not_retry_ambiguous_or_permanent_failures() {
        let uncertain = smtp_failure_plan(
            SmtpSubmissionFailure {
                stage: smtp::SmtpSubmissionStage::Body,
                kind: SmtpSubmissionFailureKind::Uncertain,
            },
            1,
            10_000,
        )
        .unwrap();
        assert_eq!(uncertain.state, OutboxState::Uncertain);
        assert_eq!(uncertain.wake_at_ms, None);

        let permanent = smtp_failure_plan(
            SmtpSubmissionFailure {
                stage: smtp::SmtpSubmissionStage::Recipient,
                kind: SmtpSubmissionFailureKind::Permanent,
            },
            1,
            10_000,
        )
        .unwrap();
        assert_eq!(permanent.state, OutboxState::PermanentFailure);
        assert_eq!(permanent.wake_at_ms, None);

        let retry = smtp_failure_plan(
            SmtpSubmissionFailure {
                stage: smtp::SmtpSubmissionStage::Connect,
                kind: SmtpSubmissionFailureKind::Retryable,
            },
            3,
            10_000,
        )
        .unwrap();
        assert_eq!(retry.state, OutboxState::RetryWait);
        assert_eq!(retry.wake_at_ms, Some(130_000));

        let rejected = smtp_failure_plan(
            SmtpSubmissionFailure {
                stage: smtp::SmtpSubmissionStage::Body,
                kind: SmtpSubmissionFailureKind::TransientRejection,
            },
            3,
            10_000,
        )
        .unwrap();
        assert_eq!(rejected.state, OutboxState::RetryWait);
        assert_eq!(rejected.wake_at_ms, Some(130_000));
        assert!(matches!(
            rejected.report,
            OutboxReport::RetryAfterRejection { .. }
        ));
    }

    #[test]
    fn pre_data_failure_enters_retry_wait_and_can_deliver_on_retry() {
        RETRY_DATA_FENCES.store(0, Ordering::Release);
        block_on(async {
            let fixture = DriverFixture::new().await;
            let (status_tx, mut status_rx) = mpsc::channel(16);
            let first = claim(&fixture.database, now_ms()).await;

            submit_claim(
                &fixture.database,
                &fixture.credentials,
                &fixture.staging,
                &status_tx,
                &SmtpCancellationTracker::default(),
                pre_data_retryable,
                first,
            )
            .await
            .unwrap();
            let (message_id, retry_at_ms) =
                expect_state(&mut status_rx, OutboxState::RetryWait).await;
            assert_eq!(message_id, fixture.message_id);
            assert_eq!(RETRY_DATA_FENCES.load(Ordering::Acquire), 0);

            let second = claim(
                &fixture.database,
                retry_at_ms.expect("retry must have a bounded wake time"),
            )
            .await;
            submit_claim(
                &fixture.database,
                &fixture.credentials,
                &fixture.staging,
                &status_tx,
                &SmtpCancellationTracker::default(),
                delivered_after_fence,
                second,
            )
            .await
            .unwrap();
            let (message_id, wake_at_ms) =
                expect_state(&mut status_rx, OutboxState::Delivered).await;
            assert_eq!(message_id, fixture.message_id);
            assert_eq!(wake_at_ms, None);
            assert_eq!(RETRY_DATA_FENCES.load(Ordering::Acquire), 1);
        });
    }

    #[test]
    fn final_body_transient_rejection_enters_retry_wait_after_fence() {
        block_on(async {
            let fixture = DriverFixture::new().await;
            let (status_tx, mut status_rx) = mpsc::channel(16);
            let outbound = claim(&fixture.database, now_ms()).await;

            submit_claim(
                &fixture.database,
                &fixture.credentials,
                &fixture.staging,
                &status_tx,
                &SmtpCancellationTracker::default(),
                transient_rejection_after_fence,
                outbound,
            )
            .await
            .unwrap();
            let (message_id, wake_at_ms) =
                expect_state(&mut status_rx, OutboxState::RetryWait).await;
            assert_eq!(message_id, fixture.message_id);
            assert!(wake_at_ms.is_some());
            assert_eq!(
                stored_outbox_state(&fixture.database_path, fixture.message_id).as_deref(),
                Some("retry_wait")
            );
        });
    }

    #[test]
    fn delivered_report_wakes_file_gc_when_status_channel_is_full() {
        block_on(async {
            let fixture = DriverFixture::new().await;
            let (wake_tx, wake_rx) = mpsc::channel(1);
            let (status_tx, mut status_rx) = mpsc::channel(1);
            let sentinel = OutboxStatus::Fault {
                message_id: None,
                kind: OutboxDriverFault::Database,
            };
            status_tx.try_send(sentinel).unwrap();
            let (file_gc_tx, mut file_gc_rx) = mpsc::channel(1);
            let cancellations = SmtpCancellationTracker::default();
            let driver = run_outbox_driver(
                fixture.database.clone(),
                fixture.credentials.clone(),
                Arc::clone(&fixture.staging),
                wake_rx,
                status_tx,
                file_gc_tx,
                cancellations.clone(),
                delivered_after_fence,
            );
            let mut driver = Box::pin(driver);

            time::timeout(
                TEST_TIMEOUT,
                poll_fn(|context| {
                    if let Poll::Ready(exit) = driver.as_mut().poll(context) {
                        panic!("outbox driver exited before waking file GC: {exit:?}");
                    }
                    file_gc_rx.poll_recv(context)
                }),
            )
            .await
            .expect("file GC wake timed out")
            .expect("file GC wake channel closed");

            assert_eq!(status_rx.try_recv(), Ok(sentinel));
            assert!(status_rx.try_recv().is_err());
            assert_eq!(
                Connection::open(&fixture.database_path)
                    .unwrap()
                    .query_row("SELECT count(*) FROM file_gc", [], |row| row
                        .get::<_, i64>(0))
                    .unwrap(),
                1
            );

            cancellations.begin_shutdown();
            drop(wake_tx);
            assert_eq!(driver.await, OutboxDriverExit::WakeChannel);
        });
    }

    #[test]
    fn current_attempt_cancel_is_message_fenced_and_retries_before_data() {
        PRE_DATA_CANCEL_READY.store(false, Ordering::Release);
        block_on(async {
            let fixture = DriverFixture::new().await;
            let (status_tx, mut status_rx) = mpsc::channel(16);
            let outbound = claim(&fixture.database, now_ms()).await;
            let cancellations = SmtpCancellationTracker::default();
            let wrong_message_id = MessageId::new(fixture.message_id.get() + 1).unwrap();

            let submission = submit_claim(
                &fixture.database,
                &fixture.credentials,
                &fixture.staging,
                &status_tx,
                &cancellations,
                wait_for_pre_data_cancellation,
                outbound,
            );
            let mut submission = Box::pin(submission);
            let mut cancel_applied = false;
            let result = std::future::poll_fn(|context| {
                let result = submission.as_mut().poll(context);
                if !cancel_applied && PRE_DATA_CANCEL_READY.load(Ordering::Acquire) {
                    assert!(!cancellations.cancel(wrong_message_id));
                    assert!(cancellations.cancel(fixture.message_id));
                    assert!(!cancellations.cancel(fixture.message_id));
                    cancel_applied = true;
                }
                result
            })
            .await;
            result.unwrap();
            assert!(cancel_applied);

            let (message_id, wake_at_ms) =
                expect_state(&mut status_rx, OutboxState::RetryWait).await;
            assert_eq!(message_id, fixture.message_id);
            assert!(wake_at_ms.is_some());
            assert!(!cancellations.cancel(fixture.message_id));
            assert_eq!(
                stored_outbox_state(&fixture.database_path, fixture.message_id).as_deref(),
                Some("retry_wait")
            );
        });
    }

    #[test]
    fn shutdown_before_data_persists_retry_wait() {
        SHUTDOWN_PRE_DATA_READY.store(false, Ordering::Release);
        block_on(async {
            let fixture = DriverFixture::new().await;
            let (status_tx, mut status_rx) = mpsc::channel(16);
            let outbound = claim(&fixture.database, now_ms()).await;
            let cancellations = SmtpCancellationTracker::default();

            let submission = submit_claim(
                &fixture.database,
                &fixture.credentials,
                &fixture.staging,
                &status_tx,
                &cancellations,
                wait_for_shutdown_before_data,
                outbound,
            );
            let mut submission = Box::pin(submission);
            let mut shutdown_started = false;
            let result = std::future::poll_fn(|context| {
                let result = submission.as_mut().poll(context);
                if !shutdown_started && SHUTDOWN_PRE_DATA_READY.load(Ordering::Acquire) {
                    cancellations.begin_shutdown();
                    shutdown_started = true;
                }
                result
            })
            .await;
            result.unwrap();
            assert!(shutdown_started);

            let (message_id, wake_at_ms) =
                expect_state(&mut status_rx, OutboxState::RetryWait).await;
            assert_eq!(message_id, fixture.message_id);
            assert!(wake_at_ms.is_some());
            assert_eq!(
                Connection::open(&fixture.database_path)
                    .unwrap()
                    .query_row(
                        "SELECT state, lease_expires_at_ms IS NULL, delivery_started
                         FROM outbox WHERE message_id = ?1",
                        [fixture.message_id.get()],
                        |row| Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, bool>(1)?,
                            row.get::<_, bool>(2)?,
                        )),
                    )
                    .unwrap(),
                ("retry_wait".to_owned(), true, false)
            );
        });
    }

    #[test]
    fn post_data_disconnect_is_uncertain_and_restart_never_resubmits() {
        DISCONNECT_DATA_FENCES.store(0, Ordering::Release);
        UNEXPECTED_SUBMISSIONS.store(0, Ordering::Release);
        block_on(async {
            let mut fixture = DriverFixture::new().await;
            let (status_tx, mut status_rx) = mpsc::channel(16);
            let outbound = claim(&fixture.database, now_ms()).await;

            submit_claim(
                &fixture.database,
                &fixture.credentials,
                &fixture.staging,
                &status_tx,
                &SmtpCancellationTracker::default(),
                disconnected_after_fence,
                outbound,
            )
            .await
            .unwrap();
            let (message_id, wake_at_ms) =
                expect_state(&mut status_rx, OutboxState::Uncertain).await;
            assert_eq!(message_id, fixture.message_id);
            assert_eq!(wake_at_ms, None);
            assert_eq!(DISCONNECT_DATA_FENCES.load(Ordering::Acquire), 1);
            assert_eq!(
                stored_outbox_state(&fixture.database_path, fixture.message_id).as_deref(),
                Some("uncertain")
            );

            fixture.stop_actors();
            let (database, database_replies, database_runtime, _) =
                sqlite::spawn(fixture.database_path.clone()).unwrap();
            let restart_store = Arc::clone(&fixture.credential_store);
            let (credentials, credential_runtime) =
                credentials::spawn_with_test_factory(move || Ok(Arc::clone(&restart_store)));
            let (wake_tx, wake_rx) = mpsc::channel(1);
            drop(wake_tx);
            let (status_tx, _status_rx) = mpsc::channel(16);
            let (file_gc_tx, _file_gc_rx) = mpsc::channel(1);

            assert_eq!(
                run_outbox_driver(
                    database,
                    credentials,
                    Arc::clone(&fixture.staging),
                    wake_rx,
                    status_tx,
                    file_gc_tx,
                    SmtpCancellationTracker::default(),
                    unexpected_submission,
                )
                .await,
                OutboxDriverExit::WakeChannel
            );
            assert_eq!(UNEXPECTED_SUBMISSIONS.load(Ordering::Acquire), 0);
            credential_runtime.shutdown().unwrap();
            drop(database_replies);
            database_runtime.shutdown().unwrap();
        });
    }

    #[test]
    fn authentication_failure_can_be_retried_or_released_explicitly() {
        block_on(async {
            let fixture = DriverFixture::new().await;
            let (status_tx, mut status_rx) = mpsc::channel(16);
            let first = claim(&fixture.database, now_ms()).await;
            let artifact_generation = first.lease.artifact_generation;
            let action_fence = OutboxActionFence::new(
                fixture.message_id,
                fixture.account.account_id,
                fixture.account.generation,
                artifact_generation,
            )
            .unwrap();

            submit_claim(
                &fixture.database,
                &fixture.credentials,
                &fixture.staging,
                &status_tx,
                &SmtpCancellationTracker::default(),
                authentication_rejected,
                first,
            )
            .await
            .unwrap();
            expect_state(&mut status_rx, OutboxState::PermanentFailure).await;
            let retried = database_call(
                &fixture.database,
                ComposeDbOperation::RetryOutbox {
                    fence: action_fence,
                    now_ms: now_ms(),
                },
            )
            .await;
            assert_eq!(
                retried,
                ComposeDbOutcome::OutboxRetried(OutboxReportOutcome::Applied(OutboxState::Ready))
            );

            let second = claim(&fixture.database, now_ms()).await;
            submit_claim(
                &fixture.database,
                &fixture.credentials,
                &fixture.staging,
                &status_tx,
                &SmtpCancellationTracker::default(),
                authentication_rejected,
                second,
            )
            .await
            .unwrap();
            expect_state(&mut status_rx, OutboxState::PermanentFailure).await;
            let released = database_call(
                &fixture.database,
                ComposeDbOperation::ReleaseFailedOutbox {
                    fence: action_fence,
                    now_ms: now_ms(),
                },
            )
            .await;
            assert_eq!(
                released,
                ComposeDbOutcome::FailedOutboxReleased(OutboxReportOutcome::Applied(
                    OutboxState::PermanentFailure
                ))
            );
            let loaded = database_call(
                &fixture.database,
                ComposeDbOperation::LoadDraft {
                    message_id: fixture.message_id,
                },
            )
            .await;
            let ComposeDbOutcome::Draft(Some(draft)) = loaded else {
                panic!("released outbox must retain its editable draft")
            };
            assert_eq!(draft.locked_artifact_generation, None);
            assert_eq!(
                stored_outbox_state(&fixture.database_path, fixture.message_id),
                None
            );
        });
    }
}
