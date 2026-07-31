use std::cmp;

use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};

use super::{
    DELETE_MASK, MAX_FOLDER_KEYS_PER_SIDE, MAX_IMAP_SOURCES, PLACEMENT_MASK, REMOTE_LEASE_TTL_MS,
    RemoteClaim, RemoteFlagDelta, RemoteImapSource, RemoteLease, RemoteProvider, RemoteWorkMode,
    STARRED_MASK, UNREAD_MASK, validate_timestamp,
};
use crate::store::sqlite::{
    domain::DbFailure,
    journal::{ensure_payload_budget, map_journal_error},
};

const MAX_TEXT_ID_BYTES: usize = 512;
const MAX_ERROR_CODE_BYTES: usize = 64;
const MAX_ERROR_DETAIL_BYTES: usize = 1_024;
const MIN_RETRY_DELAY_MS: u32 = 1_000;
const MAX_RETRY_DELAY_MS: u32 = 24 * 60 * 60 * 1_000;
const MAX_DEFAULT_RETRY_DELAY_MS: u64 = 15 * 60 * 1_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RemoteErrorClass {
    Network,
    RateLimit,
    Auth,
    Conflict,
    Permanent,
}

impl RemoteErrorClass {
    fn database_value(self) -> &'static str {
        match self {
            Self::Network => "network",
            Self::RateLimit => "rate_limit",
            Self::Auth => "auth",
            Self::Conflict => "conflict",
            Self::Permanent => "permanent",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RemoteProblem {
    class: RemoteErrorClass,
    code: Box<str>,
    detail: Box<str>,
}

impl RemoteProblem {
    pub(crate) fn new(
        class: RemoteErrorClass,
        code: &str,
        detail: &str,
    ) -> Result<Self, DbFailure> {
        validate_text(code, MAX_ERROR_CODE_BYTES, "remote error code")?;
        validate_text(detail, MAX_ERROR_DETAIL_BYTES, "remote error detail")?;
        Ok(Self {
            class,
            code: code.into(),
            detail: detail.into(),
        })
    }

    pub(crate) fn class(&self) -> RemoteErrorClass {
        self.class
    }

    pub(crate) fn code(&self) -> &str {
        &self.code
    }

    pub(crate) fn detail(&self) -> &str {
        &self.detail
    }
}

#[derive(Debug, PartialEq, Eq)]
enum RemoteCheckpointKind {
    JmapEmailState(Box<str>),
    ImapSources(Box<[RemoteImapSource]>),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RemoteCheckpoint {
    kind: RemoteCheckpointKind,
}

impl RemoteCheckpoint {
    pub(crate) fn jmap_email_state(state_token: &str) -> Result<Self, DbFailure> {
        validate_text(state_token, MAX_TEXT_ID_BYTES, "JMAP Email state")?;
        Ok(Self {
            kind: RemoteCheckpointKind::JmapEmailState(state_token.into()),
        })
    }

    pub(crate) fn imap_sources(sources: Box<[RemoteImapSource]>) -> Result<Self, DbFailure> {
        if sources.is_empty() || sources.len() > MAX_IMAP_SOURCES {
            return Err(DbFailure::resource_limit(
                "IMAP checkpoint source count exceeds bounds",
            ));
        }
        for (index, source) in sources.iter().enumerate() {
            validate_imap_source(source)?;
            if sources[..index]
                .iter()
                .any(|existing| existing.folder_key == source.folder_key)
            {
                return Err(DbFailure::conflict(
                    "IMAP checkpoint contains more than one locator for a folder",
                ));
            }
        }
        Ok(Self {
            kind: RemoteCheckpointKind::ImapSources(sources),
        })
    }
}

impl RemoteImapSource {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        folder_key: &str,
        mailbox_object_id: Option<&str>,
        uid_validity: u32,
        uid: u32,
        modseq: Option<u64>,
        email_id: Option<&str>,
        remote_seen: bool,
        remote_flagged: bool,
    ) -> Result<Self, DbFailure> {
        let source = Self {
            folder_key: folder_key.into(),
            mailbox_object_id: mailbox_object_id.map(Into::into),
            uid_validity,
            uid,
            modseq,
            email_id: email_id.map(Into::into),
            remote_seen,
            remote_flagged,
        };
        validate_imap_source(&source)?;
        Ok(source)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum RemoteReportKind {
    Confirmed(Option<RemoteCheckpoint>),
    Satisfied(Option<RemoteCheckpoint>),
    Progress(RemoteCheckpoint),
    Retry {
        retry_after_ms: Option<u32>,
        problem: RemoteProblem,
    },
    Reconcile(RemoteProblem),
    Blocked(RemoteProblem),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RemoteReport {
    kind: RemoteReportKind,
}

impl RemoteReport {
    pub(crate) fn confirmed(checkpoint: Option<RemoteCheckpoint>) -> Self {
        Self {
            kind: RemoteReportKind::Confirmed(checkpoint),
        }
    }

    pub(crate) fn satisfied(checkpoint: Option<RemoteCheckpoint>) -> Self {
        Self {
            kind: RemoteReportKind::Satisfied(checkpoint),
        }
    }

    pub(crate) fn progress(checkpoint: RemoteCheckpoint) -> Self {
        Self {
            kind: RemoteReportKind::Progress(checkpoint),
        }
    }

    pub(crate) fn retry(
        retry_after_ms: Option<u32>,
        problem: RemoteProblem,
    ) -> Result<Self, DbFailure> {
        if !matches!(
            problem.class,
            RemoteErrorClass::Network | RemoteErrorClass::RateLimit
        ) {
            return Err(DbFailure::conflict(
                "remote retry requires a network or rate-limit problem",
            ));
        }
        Ok(Self {
            kind: RemoteReportKind::Retry {
                retry_after_ms,
                problem,
            },
        })
    }

    pub(crate) fn reconcile(problem: RemoteProblem) -> Result<Self, DbFailure> {
        if !matches!(
            problem.class,
            RemoteErrorClass::Network | RemoteErrorClass::Conflict
        ) {
            return Err(DbFailure::conflict(
                "remote reconciliation requires a network or conflict problem",
            ));
        }
        Ok(Self {
            kind: RemoteReportKind::Reconcile(problem),
        })
    }

    pub(crate) fn blocked(problem: RemoteProblem) -> Result<Self, DbFailure> {
        if !matches!(
            problem.class,
            RemoteErrorClass::Auth | RemoteErrorClass::Conflict | RemoteErrorClass::Permanent
        ) {
            return Err(DbFailure::conflict(
                "blocked remote work requires an auth, conflict, or permanent problem",
            ));
        }
        Ok(Self {
            kind: RemoteReportKind::Blocked(problem),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RemotePendingState {
    Ready,
    RetryWait,
    Reconcile,
    Blocked,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RemoteReportOutcome {
    Stale,
    Completed,
    Pending {
        state: RemotePendingState,
        wake_at_ms: Option<i64>,
    },
    Continued(Box<RemoteClaim>),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RemoteReportSubmission {
    claim: Box<RemoteClaim>,
    report: RemoteReport,
}

impl RemoteReportSubmission {
    pub(crate) fn new(claim: Box<RemoteClaim>, report: RemoteReport) -> Box<Self> {
        Box::new(Self { claim, report })
    }

    pub(crate) fn claim(&self) -> &RemoteClaim {
        &self.claim
    }

    pub(crate) fn report(&self) -> &RemoteReport {
        &self.report
    }

    #[allow(clippy::boxed_local)]
    pub(crate) fn into_parts(self: Box<Self>) -> (Box<RemoteClaim>, RemoteReport) {
        let Self { claim, report } = *self;
        (claim, report)
    }

    pub(crate) fn continue_claim(self: Box<Self>, lease: RemoteLease) -> RemoteReportOutcome {
        let (mut claim, report) = self.into_parts();
        let RemoteReportKind::Progress(checkpoint) = report.kind else {
            unreachable!("only a progress report can continue a remote claim");
        };
        claim.lease = lease;
        merge_checkpoint_into_claim(&mut claim, checkpoint);
        RemoteReportOutcome::Continued(claim)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReportTransition {
    Stale,
    Completed,
    Pending {
        state: RemotePendingState,
        wake_at_ms: Option<i64>,
    },
    Continued(RemoteLease),
}

struct LeasedIntent {
    intent_version: i64,
    message_id: Option<i64>,
    unread_base: Option<bool>,
    unread_desired: Option<bool>,
    starred_base: Option<bool>,
    starred_desired: Option<bool>,
    placement_active: bool,
    reconcile_requested: bool,
    delete_requested: bool,
    attempt_count: i64,
    not_before_ms: i64,
}

pub(crate) fn report_remote(
    connection: &mut rusqlite::Connection,
    claim: &RemoteClaim,
    report: &RemoteReport,
    now_ms: i64,
) -> Result<ReportTransition, DbFailure> {
    validate_timestamp(now_ms)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(DbFailure::database)?;
    let Some(current) = load_matching_lease(&transaction, claim)? else {
        transaction.commit().map_err(DbFailure::database)?;
        return Ok(ReportTransition::Stale);
    };

    let transition = match &report.kind {
        RemoteReportKind::Confirmed(checkpoint) => {
            if claim.mode != RemoteWorkMode::Apply {
                return Err(DbFailure::conflict(
                    "a reconciliation claim cannot report provider confirmation",
                ));
            }
            validate_final_checkpoint(claim.provider, checkpoint.as_ref())?;
            if let Some(checkpoint) = checkpoint {
                apply_checkpoint(&transaction, claim, checkpoint, now_ms, false)?;
            }
            acknowledge(&transaction, claim, &current, now_ms, false)?
        }
        RemoteReportKind::Satisfied(checkpoint) => {
            validate_final_checkpoint(claim.provider, checkpoint.as_ref())?;
            if let Some(checkpoint) = checkpoint {
                apply_checkpoint(&transaction, claim, checkpoint, now_ms, false)?;
            }
            acknowledge(&transaction, claim, &current, now_ms, true)?
        }
        RemoteReportKind::Progress(checkpoint) => {
            validate_checkpoint_provider(claim.provider, checkpoint)?;
            if current.intent_version < checked_i64(claim.lease.leased_version, "leased version")? {
                return Err(DbFailure::conflict(
                    "remote intent version regressed during progress",
                ));
            }
            let next_epoch = claim
                .lease
                .claim_epoch
                .checked_add(1)
                .ok_or_else(|| DbFailure::resource_limit("remote claim epoch exhausted"))?;
            let next_epoch_i64 = checked_i64(next_epoch, "claim epoch")?;
            let expires_at_ms = now_ms
                .checked_add(REMOTE_LEASE_TTL_MS)
                .ok_or_else(|| DbFailure::resource_limit("remote lease expiry overflow"))?;
            validate_timestamp(expires_at_ms)?;
            if !apply_checkpoint(&transaction, claim, checkpoint, now_ms, true)? {
                return Err(DbFailure::conflict(
                    "remote progress did not advance a durable checkpoint",
                ));
            }
            let updated = transaction
                .execute(
                    "UPDATE remote_change_intents
                     SET claim_epoch = ?4, lease_expires_at_ms = ?5, updated_at_ms = ?6
                     WHERE id = ?1 AND state = 'in_flight'
                       AND leased_version = ?2 AND claim_epoch = ?3",
                    params![
                        claim.lease.intent_id,
                        checked_i64(claim.lease.leased_version, "leased version")?,
                        checked_i64(claim.lease.claim_epoch, "claim epoch")?,
                        next_epoch_i64,
                        expires_at_ms,
                        now_ms,
                    ],
                )
                .map_err(DbFailure::database)?;
            if updated != 1 {
                return Err(DbFailure::conflict(
                    "remote lease changed while progress was committed",
                ));
            }
            ReportTransition::Continued(RemoteLease {
                intent_id: claim.lease.intent_id,
                leased_version: claim.lease.leased_version,
                claim_epoch: next_epoch,
                expires_at_ms,
            })
        }
        RemoteReportKind::Retry {
            retry_after_ms,
            problem,
        } => retry(
            &transaction,
            claim,
            &current,
            *retry_after_ms,
            problem,
            now_ms,
        )?,
        RemoteReportKind::Reconcile(problem) => {
            reconcile(&transaction, claim, &current, problem, now_ms)?
        }
        RemoteReportKind::Blocked(problem) => {
            blocked(&transaction, claim, &current, problem, now_ms)?
        }
    };

    transaction.commit().map_err(DbFailure::database)?;
    Ok(transition)
}

fn load_matching_lease(
    transaction: &Transaction<'_>,
    claim: &RemoteClaim,
) -> Result<Option<LeasedIntent>, DbFailure> {
    transaction
        .query_row(
            "SELECT i.intent_version, i.message_id,
                    i.unread_base, i.unread_desired,
                    i.starred_base, i.starred_desired,
                    i.placement_active, i.reconcile_requested,
                    i.delete_requested, i.attempt_count, i.not_before_ms
             FROM remote_change_intents AS i
             JOIN accounts AS a ON a.id = i.account_id
             WHERE i.id = ?1 AND i.state = 'in_flight'
               AND i.leased_version = ?2 AND i.claim_epoch = ?3
               AND i.account_id = ?4 AND i.target_key = ?5
               AND a.provider = ?6 COLLATE NOCASE",
            params![
                claim.lease.intent_id,
                checked_i64(claim.lease.leased_version, "leased version")?,
                checked_i64(claim.lease.claim_epoch, "claim epoch")?,
                claim.account_id,
                &claim.target_key,
                provider_name(claim.provider),
            ],
            |row| {
                Ok(LeasedIntent {
                    intent_version: row.get(0)?,
                    message_id: row.get(1)?,
                    unread_base: row.get(2)?,
                    unread_desired: row.get(3)?,
                    starred_base: row.get(4)?,
                    starred_desired: row.get(5)?,
                    placement_active: row.get(6)?,
                    reconcile_requested: row.get(7)?,
                    delete_requested: row.get(8)?,
                    attempt_count: row.get(9)?,
                    not_before_ms: row.get(10)?,
                })
            },
        )
        .optional()
        .map_err(DbFailure::database)
}

fn acknowledge(
    transaction: &Transaction<'_>,
    claim: &RemoteClaim,
    current: &LeasedIntent,
    now_ms: i64,
    satisfied: bool,
) -> Result<ReportTransition, DbFailure> {
    let leased_version = checked_i64(claim.lease.leased_version, "leased version")?;
    let same_version = current.intent_version == leased_version;
    if current.intent_version < leased_version {
        return Err(DbFailure::conflict(
            "remote intent version regressed before acknowledgement",
        ));
    }

    if claim.delete_requested {
        if !current.delete_requested || !same_version {
            return Err(DbFailure::conflict(
                "a terminal remote acknowledgement was superseded",
            ));
        }
        let deleted = transaction
            .execute(
                "DELETE FROM remote_change_intents
                 WHERE id = ?1 AND state = 'in_flight'
                   AND leased_version = ?2 AND claim_epoch = ?3",
                params![
                    claim.lease.intent_id,
                    leased_version,
                    checked_i64(claim.lease.claim_epoch, "claim epoch")?,
                ],
            )
            .map_err(DbFailure::database)?;
        if deleted != 1 {
            return Err(DbFailure::conflict(
                "terminal remote lease changed during acknowledgement",
            ));
        }
        return Ok(ReportTransition::Completed);
    }

    let claim_mask = claim_mask(claim);
    if current.delete_requested {
        let state = terminal_supersession_state(transaction, claim)?;
        release_lease(
            transaction,
            claim,
            state,
            current.not_before_ms,
            claim_mask,
            same_version,
            now_ms,
        )?;
        return Ok(ReportTransition::Pending {
            state,
            wake_at_ms: pending_wake(state, current.not_before_ms, now_ms),
        });
    }

    transaction
        .execute(
            "UPDATE remote_change_intents
             SET leased_folder_reserve = 0, reconcile_requested = 1
             WHERE id = ?1",
            [claim.lease.intent_id],
        )
        .map_err(DbFailure::database)?;
    acknowledge_flag(
        transaction,
        claim,
        current,
        claim.unread,
        FlagColumn::Unread,
    )?;
    acknowledge_flag(
        transaction,
        claim,
        current,
        claim.starred,
        FlagColumn::Starred,
    )?;
    acknowledge_placement(transaction, claim, current)?;

    if satisfied
        && same_version
        && let Some(message_id) = current.message_id
    {
        transaction
            .execute(
                "UPDATE messages SET legacy_reconcile_revision = NULL
                 WHERE id = ?1 AND revision = ?2
                   AND legacy_reconcile_revision IS NOT NULL",
                params![
                    message_id,
                    checked_i64(claim.local_revision, "local revision")?
                ],
            )
            .map_err(DbFailure::database)?;
    }

    let has_pending_change: bool = transaction
        .query_row(
            "SELECT unread_base IS NOT NULL
                        OR starred_base IS NOT NULL
                        OR placement_active = 1
                        OR delete_requested = 1
             FROM remote_change_intents WHERE id = ?1",
            [claim.lease.intent_id],
            |row| row.get(0),
        )
        .map_err(DbFailure::database)?;
    let reconcile_requested = if satisfied && same_version {
        false
    } else {
        current.reconcile_requested
    };
    if !has_pending_change && !reconcile_requested {
        transaction
            .execute(
                "DELETE FROM remote_change_intents WHERE id = ?1",
                [claim.lease.intent_id],
            )
            .map_err(DbFailure::database)?;
        return Ok(ReportTransition::Completed);
    }
    transaction
        .execute(
            "UPDATE remote_change_intents SET reconcile_requested = ?2 WHERE id = ?1",
            params![claim.lease.intent_id, reconcile_requested],
        )
        .map_err(DbFailure::database)?;

    let state = if reconcile_requested {
        RemotePendingState::Reconcile
    } else {
        RemotePendingState::Ready
    };
    release_lease(
        transaction,
        claim,
        state,
        current.not_before_ms,
        claim_mask,
        same_version,
        now_ms,
    )?;
    ensure_payload_budget(transaction, claim.lease.intent_id)?;
    Ok(ReportTransition::Pending {
        state,
        wake_at_ms: pending_wake(state, current.not_before_ms, now_ms),
    })
}

#[derive(Clone, Copy)]
enum FlagColumn {
    Unread,
    Starred,
}

fn acknowledge_flag(
    transaction: &Transaction<'_>,
    claim: &RemoteClaim,
    current: &LeasedIntent,
    claimed: Option<RemoteFlagDelta>,
    column: FlagColumn,
) -> Result<(), DbFailure> {
    let Some(claimed) = claimed else {
        return Ok(());
    };
    let (current_base, current_desired) = match column {
        FlagColumn::Unread => (current.unread_base, current.unread_desired),
        FlagColumn::Starred => (current.starred_base, current.starred_desired),
    };
    let desired = match (current_base, current_desired) {
        (Some(base), Some(desired)) => {
            if base != claimed.base {
                return Err(DbFailure::conflict(
                    "remote flag base changed while its lease was active",
                ));
            }
            desired
        }
        (None, None)
            if claim.mode == RemoteWorkMode::Reconcile
                && current.intent_version
                    > checked_i64(claim.lease.leased_version, "leased version")? =>
        {
            current_message_flag(transaction, current.message_id, column)?
        }
        _ => {
            return Err(DbFailure::conflict(
                "remote flag delta disappeared from an apply lease",
            ));
        }
    };
    let (base, desired) = if desired == claimed.desired {
        (None, None)
    } else {
        (Some(claimed.desired), Some(desired))
    };
    let sql = match column {
        FlagColumn::Unread => {
            "UPDATE remote_change_intents
             SET unread_base = ?2, unread_desired = ?3 WHERE id = ?1"
        }
        FlagColumn::Starred => {
            "UPDATE remote_change_intents
             SET starred_base = ?2, starred_desired = ?3 WHERE id = ?1"
        }
    };
    transaction
        .execute(sql, params![claim.lease.intent_id, base, desired])
        .map_err(DbFailure::database)?;
    Ok(())
}

fn current_message_flag(
    transaction: &Transaction<'_>,
    message_id: Option<i64>,
    column: FlagColumn,
) -> Result<bool, DbFailure> {
    let message_id = message_id
        .ok_or_else(|| DbFailure::conflict("remote flag reversal no longer has a local message"))?;
    let sql = match column {
        FlagColumn::Unread => "SELECT unread FROM messages WHERE id = ?1",
        FlagColumn::Starred => "SELECT starred FROM messages WHERE id = ?1",
    };
    transaction
        .query_row(sql, [message_id], |row| row.get(0))
        .optional()
        .map_err(DbFailure::database)?
        .ok_or_else(|| DbFailure::conflict("remote flag reversal message no longer exists"))
}

fn acknowledge_placement(
    transaction: &Transaction<'_>,
    claim: &RemoteClaim,
    current: &LeasedIntent,
) -> Result<(), DbFailure> {
    let Some(placement) = claim.placement.as_ref() else {
        return Ok(());
    };
    if current.placement_active {
        if !folder_side_matches(transaction, claim.lease.intent_id, "base", &placement.base)? {
            return Err(DbFailure::conflict(
                "remote placement base changed while its lease was active",
            ));
        }
        if folder_side_matches(
            transaction,
            claim.lease.intent_id,
            "desired",
            &placement.desired,
        )? {
            transaction
                .execute(
                    "DELETE FROM remote_change_intent_folders WHERE intent_id = ?1",
                    [claim.lease.intent_id],
                )
                .map_err(map_journal_error)?;
            transaction
                .execute(
                    "UPDATE remote_change_intents SET placement_active = 0 WHERE id = ?1",
                    [claim.lease.intent_id],
                )
                .map_err(DbFailure::database)?;
        } else {
            replace_folder_side(
                transaction,
                claim.lease.intent_id,
                "base",
                &placement.desired,
            )?;
        }
        return Ok(());
    }

    if claim.mode != RemoteWorkMode::Reconcile
        || current.intent_version <= checked_i64(claim.lease.leased_version, "leased version")?
    {
        return Err(DbFailure::conflict(
            "remote placement disappeared from an apply lease",
        ));
    }
    let message_id = current.message_id.ok_or_else(|| {
        DbFailure::conflict("remote placement reversal no longer has a local message")
    })?;
    if membership_matches(
        transaction,
        message_id,
        claim.lease.intent_id,
        &placement.desired,
    )? {
        return Ok(());
    }
    let membership_count = bounded_membership_count(transaction, message_id)?;
    transaction
        .execute(
            "UPDATE remote_change_intents SET placement_active = 1 WHERE id = ?1",
            [claim.lease.intent_id],
        )
        .map_err(DbFailure::database)?;
    replace_folder_side(
        transaction,
        claim.lease.intent_id,
        "base",
        &placement.desired,
    )?;
    let inserted = transaction
        .execute(
            "INSERT INTO remote_change_intent_folders (intent_id, side, folder_key)
             SELECT ?1, 'desired', f.remote_key
             FROM message_folders AS mf
             JOIN folders AS f
               ON f.id = mf.folder_id AND f.account_id = mf.account_id
             WHERE mf.message_id = ?2",
            params![claim.lease.intent_id, message_id],
        )
        .map_err(map_journal_error)?;
    if inserted != membership_count {
        return Err(DbFailure::conflict(
            "remote placement reversal membership changed during report",
        ));
    }
    Ok(())
}

fn folder_side_matches(
    transaction: &Transaction<'_>,
    intent_id: i64,
    side: &str,
    keys: &[Box<str>],
) -> Result<bool, DbFailure> {
    let count: i64 = transaction
        .query_row(
            "SELECT count(*) FROM remote_change_intent_folders
             WHERE intent_id = ?1 AND side = ?2",
            params![intent_id, side],
            |row| row.get(0),
        )
        .map_err(DbFailure::database)?;
    if usize::try_from(count).ok() != Some(keys.len()) {
        return Ok(false);
    }
    let mut statement = transaction
        .prepare(
            "SELECT EXISTS (
                 SELECT 1 FROM remote_change_intent_folders
                 WHERE intent_id = ?1 AND side = ?2 AND folder_key = ?3
             )",
        )
        .map_err(DbFailure::database)?;
    for key in keys {
        let exists: bool = statement
            .query_row(params![intent_id, side, key], |row| row.get(0))
            .map_err(DbFailure::database)?;
        if !exists {
            return Ok(false);
        }
    }
    Ok(true)
}

fn replace_folder_side(
    transaction: &Transaction<'_>,
    intent_id: i64,
    side: &str,
    keys: &[Box<str>],
) -> Result<(), DbFailure> {
    transaction
        .execute(
            "DELETE FROM remote_change_intent_folders
             WHERE intent_id = ?1 AND side = ?2",
            params![intent_id, side],
        )
        .map_err(map_journal_error)?;
    let mut insert = transaction
        .prepare(
            "INSERT INTO remote_change_intent_folders (intent_id, side, folder_key)
             VALUES (?1, ?2, ?3)",
        )
        .map_err(DbFailure::database)?;
    for key in keys {
        insert
            .execute(params![intent_id, side, key])
            .map_err(map_journal_error)?;
    }
    Ok(())
}

fn membership_matches(
    transaction: &Transaction<'_>,
    message_id: i64,
    intent_id: i64,
    keys: &[Box<str>],
) -> Result<bool, DbFailure> {
    let count = bounded_membership_count(transaction, message_id)?;
    if count != keys.len() {
        return Ok(false);
    }
    let mut statement = transaction
        .prepare(
            "SELECT EXISTS (
                 SELECT 1 FROM message_folders AS mf
                 JOIN folders AS f
                   ON f.id = mf.folder_id AND f.account_id = mf.account_id
                 JOIN remote_change_intents AS i
                   ON i.account_id = mf.account_id
                 WHERE mf.message_id = ?1 AND i.id = ?2 AND f.remote_key = ?3
             )",
        )
        .map_err(DbFailure::database)?;
    for key in keys {
        let exists: bool = statement
            .query_row(params![message_id, intent_id, key], |row| row.get(0))
            .map_err(DbFailure::database)?;
        if !exists {
            return Ok(false);
        }
    }
    Ok(true)
}

fn bounded_membership_count(
    transaction: &Transaction<'_>,
    message_id: i64,
) -> Result<usize, DbFailure> {
    let count: i64 = transaction
        .query_row(
            "SELECT count(*) FROM (
                 SELECT 1 FROM message_folders WHERE message_id = ?1 LIMIT 257
             )",
            [message_id],
            |row| row.get(0),
        )
        .map_err(DbFailure::database)?;
    let count = usize::try_from(count)
        .map_err(|_| DbFailure::resource_limit("invalid folder membership count"))?;
    if count > MAX_FOLDER_KEYS_PER_SIDE {
        Err(DbFailure::resource_limit(
            "remote placement reversal exceeds folder bounds",
        ))
    } else {
        Ok(count)
    }
}

fn retry(
    transaction: &Transaction<'_>,
    claim: &RemoteClaim,
    current: &LeasedIntent,
    retry_after_ms: Option<u32>,
    problem: &RemoteProblem,
    now_ms: i64,
) -> Result<ReportTransition, DbFailure> {
    if current.attempt_count >= 1_000 {
        return block_attempt_limit(transaction, claim, current, now_ms);
    }
    let delay_ms = retry_after_ms.map_or_else(
        || default_retry_delay(current.attempt_count),
        |delay| u64::from(delay.clamp(MIN_RETRY_DELAY_MS, MAX_RETRY_DELAY_MS)),
    );
    let derived_wake_at_ms = now_ms
        .checked_add(
            i64::try_from(delay_ms)
                .map_err(|_| DbFailure::resource_limit("remote retry delay overflow"))?,
        )
        .ok_or_else(|| DbFailure::resource_limit("remote retry timestamp overflow"))?;
    let wake_at_ms = cmp::max(current.not_before_ms, derived_wake_at_ms);
    validate_timestamp(wake_at_ms)?;
    update_failure_state(
        transaction,
        claim,
        "retry_wait",
        wake_at_ms,
        false,
        problem,
        now_ms,
    )?;
    Ok(ReportTransition::Pending {
        state: RemotePendingState::RetryWait,
        wake_at_ms: Some(wake_at_ms),
    })
}

fn reconcile(
    transaction: &Transaction<'_>,
    claim: &RemoteClaim,
    current: &LeasedIntent,
    problem: &RemoteProblem,
    now_ms: i64,
) -> Result<ReportTransition, DbFailure> {
    if current.attempt_count >= 1_000 {
        return block_attempt_limit(transaction, claim, current, now_ms);
    }
    let wake_at_ms = cmp::max(current.not_before_ms, now_ms);
    update_failure_state(
        transaction,
        claim,
        "reconcile",
        wake_at_ms,
        !current.delete_requested,
        problem,
        now_ms,
    )?;
    Ok(ReportTransition::Pending {
        state: RemotePendingState::Reconcile,
        wake_at_ms: Some(wake_at_ms),
    })
}

fn blocked(
    transaction: &Transaction<'_>,
    claim: &RemoteClaim,
    current: &LeasedIntent,
    problem: &RemoteProblem,
    now_ms: i64,
) -> Result<ReportTransition, DbFailure> {
    let not_before_ms = cmp::max(current.not_before_ms, now_ms);
    update_failure_state(
        transaction,
        claim,
        "blocked",
        not_before_ms,
        false,
        problem,
        now_ms,
    )?;
    Ok(ReportTransition::Pending {
        state: RemotePendingState::Blocked,
        wake_at_ms: None,
    })
}

fn block_attempt_limit(
    transaction: &Transaction<'_>,
    claim: &RemoteClaim,
    current: &LeasedIntent,
    now_ms: i64,
) -> Result<ReportTransition, DbFailure> {
    let problem = RemoteProblem {
        class: RemoteErrorClass::Permanent,
        code: "attempt_limit".into(),
        detail:
            "Remote synchronization stopped after 1,000 attempts; review the account before retrying."
                .into(),
    };
    update_failure_state(
        transaction,
        claim,
        "blocked",
        cmp::max(current.not_before_ms, now_ms),
        false,
        &problem,
        now_ms,
    )?;
    Ok(ReportTransition::Pending {
        state: RemotePendingState::Blocked,
        wake_at_ms: None,
    })
}

#[allow(clippy::too_many_arguments)]
fn update_failure_state(
    transaction: &Transaction<'_>,
    claim: &RemoteClaim,
    state: &str,
    not_before_ms: i64,
    request_reconcile: bool,
    problem: &RemoteProblem,
    now_ms: i64,
) -> Result<(), DbFailure> {
    let updated = transaction
        .execute(
            "UPDATE remote_change_intents
             SET state = ?4,
                 leased_version = NULL,
                 lease_expires_at_ms = NULL,
                 leased_folder_reserve = 0,
                 reconcile_requested = CASE
                     WHEN ?5 THEN 1 ELSE reconcile_requested
                 END,
                 not_before_ms = ?6,
                 error_class = ?7,
                 error_code = ?8,
                 error_detail = ?9,
                 updated_at_ms = ?10
             WHERE id = ?1 AND state = 'in_flight'
               AND leased_version = ?2 AND claim_epoch = ?3",
            params![
                claim.lease.intent_id,
                checked_i64(claim.lease.leased_version, "leased version")?,
                checked_i64(claim.lease.claim_epoch, "claim epoch")?,
                state,
                request_reconcile,
                not_before_ms,
                problem.class.database_value(),
                &problem.code,
                &problem.detail,
                now_ms,
            ],
        )
        .map_err(DbFailure::database)?;
    if updated != 1 {
        return Err(DbFailure::conflict(
            "remote lease changed while its report was committed",
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn release_lease(
    transaction: &Transaction<'_>,
    claim: &RemoteClaim,
    state: RemotePendingState,
    not_before_ms: i64,
    acknowledged_mask: i64,
    clear_errors: bool,
    now_ms: i64,
) -> Result<(), DbFailure> {
    let state_name = match state {
        RemotePendingState::Ready => "ready",
        RemotePendingState::RetryWait => "retry_wait",
        RemotePendingState::Reconcile => "reconcile",
        RemotePendingState::Blocked => "blocked",
    };
    let updated = transaction
        .execute(
            "UPDATE remote_change_intents
             SET state = ?4,
                 leased_version = NULL,
                 lease_expires_at_ms = NULL,
                 leased_folder_reserve = 0,
                 attempt_count = 0,
                 dispatched_mask = dispatched_mask & ~?5,
                 error_class = CASE WHEN ?6 THEN NULL ELSE error_class END,
                 error_code = CASE WHEN ?6 THEN NULL ELSE error_code END,
                 error_detail = CASE WHEN ?6 THEN NULL ELSE error_detail END,
                 not_before_ms = ?7,
                 updated_at_ms = ?8
             WHERE id = ?1 AND state = 'in_flight'
               AND leased_version = ?2 AND claim_epoch = ?3",
            params![
                claim.lease.intent_id,
                checked_i64(claim.lease.leased_version, "leased version")?,
                checked_i64(claim.lease.claim_epoch, "claim epoch")?,
                state_name,
                acknowledged_mask,
                clear_errors,
                not_before_ms,
                now_ms,
            ],
        )
        .map_err(DbFailure::database)?;
    if updated != 1 {
        return Err(DbFailure::conflict(
            "remote lease changed during acknowledgement",
        ));
    }
    Ok(())
}

fn terminal_supersession_state(
    transaction: &Transaction<'_>,
    claim: &RemoteClaim,
) -> Result<RemotePendingState, DbFailure> {
    if claim.provider == RemoteProvider::Imap {
        let has_sources: bool = transaction
            .query_row(
                "SELECT EXISTS (
                     SELECT 1 FROM remote_change_intent_imap_sources WHERE intent_id = ?1
                 )",
                [claim.lease.intent_id],
                |row| row.get(0),
            )
            .map_err(DbFailure::database)?;
        if !has_sources {
            return Ok(RemotePendingState::Reconcile);
        }
    }
    Ok(RemotePendingState::Ready)
}

fn claim_mask(claim: &RemoteClaim) -> i64 {
    (i64::from(claim.unread.is_some()) * UNREAD_MASK)
        | (i64::from(claim.starred.is_some()) * STARRED_MASK)
        | (i64::from(claim.placement.is_some()) * PLACEMENT_MASK)
        | (i64::from(claim.delete_requested) * DELETE_MASK)
}

fn pending_wake(state: RemotePendingState, not_before_ms: i64, now_ms: i64) -> Option<i64> {
    match state {
        RemotePendingState::Ready => Some(cmp::max(not_before_ms, now_ms)),
        RemotePendingState::RetryWait => Some(not_before_ms),
        RemotePendingState::Reconcile => Some(cmp::max(not_before_ms, now_ms)),
        RemotePendingState::Blocked => None,
    }
}

fn default_retry_delay(attempt_count: i64) -> u64 {
    let exponent = attempt_count.saturating_sub(1).clamp(0, 20) as u32;
    cmp::min(1_000_u64 << exponent, MAX_DEFAULT_RETRY_DELAY_MS)
}

fn validate_final_checkpoint(
    provider: RemoteProvider,
    checkpoint: Option<&RemoteCheckpoint>,
) -> Result<(), DbFailure> {
    if provider == RemoteProvider::Jmap && checkpoint.is_none() {
        return Err(DbFailure::conflict(
            "a final JMAP report requires an Email state checkpoint",
        ));
    }
    if let Some(checkpoint) = checkpoint {
        validate_checkpoint_provider(provider, checkpoint)?;
    }
    Ok(())
}

fn validate_checkpoint_provider(
    provider: RemoteProvider,
    checkpoint: &RemoteCheckpoint,
) -> Result<(), DbFailure> {
    let matches = matches!(
        (provider, &checkpoint.kind),
        (
            RemoteProvider::Jmap,
            RemoteCheckpointKind::JmapEmailState(_)
        ) | (RemoteProvider::Imap, RemoteCheckpointKind::ImapSources(_))
    );
    if matches {
        Ok(())
    } else {
        Err(DbFailure::conflict(
            "remote checkpoint does not match the account provider",
        ))
    }
}

fn apply_checkpoint(
    transaction: &Transaction<'_>,
    claim: &RemoteClaim,
    checkpoint: &RemoteCheckpoint,
    now_ms: i64,
    require_change: bool,
) -> Result<bool, DbFailure> {
    match &checkpoint.kind {
        RemoteCheckpointKind::JmapEmailState(state_token) => {
            let previous: Option<String> = transaction
                .query_row(
                    "SELECT state_token FROM account_object_states
                     WHERE account_id = ?1 AND object_kind = 'email'",
                    [claim.account_id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(DbFailure::database)?;
            let changed = previous.as_deref() != Some(state_token);
            if require_change && !changed {
                return Ok(false);
            }
            transaction
                .execute(
                    "INSERT INTO account_object_states
                         (account_id, object_kind, state_token, updated_at_ms)
                     VALUES (?1, 'email', ?2, ?3)
                     ON CONFLICT(account_id, object_kind) DO UPDATE
                     SET state_token = excluded.state_token,
                         updated_at_ms = excluded.updated_at_ms",
                    params![claim.account_id, state_token, now_ms],
                )
                .map_err(DbFailure::database)?;
            Ok(changed)
        }
        RemoteCheckpointKind::ImapSources(sources) => {
            let mut changed = false;
            for source in sources {
                changed |= upsert_imap_checkpoint(transaction, claim, source)?;
            }
            ensure_payload_budget(transaction, claim.lease.intent_id)?;
            Ok(changed)
        }
    }
}

fn upsert_imap_checkpoint(
    transaction: &Transaction<'_>,
    claim: &RemoteClaim,
    source: &RemoteImapSource,
) -> Result<bool, DbFailure> {
    type ExistingSource = (Option<String>, Option<i64>, Option<String>, bool, bool);
    let existing: Option<ExistingSource> = transaction
        .query_row(
            "SELECT mailbox_object_id, modseq, email_id,
                    remote_seen, remote_flagged
             FROM remote_change_intent_imap_sources
             WHERE intent_id = ?1 AND folder_key = ?2
               AND uid_validity = ?3 AND uid = ?4",
            params![
                claim.lease.intent_id,
                &source.folder_key,
                i64::from(source.uid_validity),
                i64::from(source.uid),
            ],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()
        .map_err(DbFailure::database)?;
    if let Some(existing) = existing.as_ref() {
        reject_changed_stable_id(
            existing.0.as_deref(),
            source.mailbox_object_id.as_deref(),
            "IMAP mailbox object id",
        )?;
        reject_changed_stable_id(
            existing.2.as_deref(),
            source.email_id.as_deref(),
            "IMAP email id",
        )?;
    }
    let modseq = source
        .modseq
        .map(|value| checked_i64(value, "IMAP MODSEQ"))
        .transpose()?;
    let changed = existing.as_ref().is_none_or(|existing| {
        let next_mailbox_object_id = source
            .mailbox_object_id
            .as_deref()
            .or(existing.0.as_deref());
        let next_modseq = match (existing.1, modseq) {
            (Some(left), Some(right)) => Some(cmp::max(left, right)),
            (left, right) => left.or(right),
        };
        let next_email_id = source.email_id.as_deref().or(existing.2.as_deref());
        let accepts_flags = existing
            .1
            .is_none_or(|previous| modseq.is_some_and(|incoming| incoming >= previous));
        let next_seen = if accepts_flags {
            source.remote_seen
        } else {
            existing.3
        };
        let next_flagged = if accepts_flags {
            source.remote_flagged
        } else {
            existing.4
        };
        existing.0.as_deref() != next_mailbox_object_id
            || existing.1 != next_modseq
            || existing.2.as_deref() != next_email_id
            || existing.3 != next_seen
            || existing.4 != next_flagged
    });
    transaction
        .execute(
            "INSERT INTO remote_change_intent_imap_sources
                 (intent_id, folder_key, mailbox_object_id, uid_validity, uid,
                  modseq, email_id, remote_seen, remote_flagged)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(intent_id, folder_key, uid_validity, uid) DO UPDATE
             SET mailbox_object_id = coalesce(
                     excluded.mailbox_object_id,
                     remote_change_intent_imap_sources.mailbox_object_id
                 ),
                 modseq = CASE
                     WHEN remote_change_intent_imap_sources.modseq IS NULL THEN excluded.modseq
                     WHEN excluded.modseq IS NULL THEN remote_change_intent_imap_sources.modseq
                     ELSE max(remote_change_intent_imap_sources.modseq, excluded.modseq)
                 END,
                 email_id = coalesce(
                     excluded.email_id,
                     remote_change_intent_imap_sources.email_id
                 ),
                 remote_seen = CASE
                     WHEN remote_change_intent_imap_sources.modseq IS NOT NULL
                      AND (excluded.modseq IS NULL OR excluded.modseq
                           < remote_change_intent_imap_sources.modseq)
                     THEN remote_change_intent_imap_sources.remote_seen
                     ELSE excluded.remote_seen
                 END,
                 remote_flagged = CASE
                     WHEN remote_change_intent_imap_sources.modseq IS NOT NULL
                      AND (excluded.modseq IS NULL OR excluded.modseq
                           < remote_change_intent_imap_sources.modseq)
                     THEN remote_change_intent_imap_sources.remote_flagged
                     ELSE excluded.remote_flagged
                 END",
            params![
                claim.lease.intent_id,
                &source.folder_key,
                &source.mailbox_object_id,
                i64::from(source.uid_validity),
                i64::from(source.uid),
                modseq,
                &source.email_id,
                source.remote_seen,
                source.remote_flagged,
            ],
        )
        .map_err(map_journal_error)?;
    let delete_requested: bool = transaction
        .query_row(
            "SELECT delete_requested FROM remote_change_intents WHERE id = ?1",
            [claim.lease.intent_id],
            |row| row.get(0),
        )
        .map_err(DbFailure::database)?;
    if delete_requested {
        let existing_tombstone_ids: Option<(Option<String>, Option<String>)> = transaction
            .query_row(
                "SELECT mailbox_object_id, email_id
                 FROM message_tombstone_imap_locations
                 WHERE account_id = ?1 AND target_key = ?2 AND folder_key = ?3
                   AND uid_validity = ?4 AND uid = ?5",
                params![
                    claim.account_id,
                    &claim.target_key,
                    &source.folder_key,
                    i64::from(source.uid_validity),
                    i64::from(source.uid),
                ],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(DbFailure::database)?;
        if let Some((mailbox_object_id, email_id)) = existing_tombstone_ids {
            reject_changed_stable_id(
                mailbox_object_id.as_deref(),
                source.mailbox_object_id.as_deref(),
                "tombstone IMAP mailbox object id",
            )?;
            reject_changed_stable_id(
                email_id.as_deref(),
                source.email_id.as_deref(),
                "tombstone IMAP email id",
            )?;
        }
        transaction
            .execute(
                "INSERT INTO message_tombstone_imap_locations
                     (account_id, target_key, folder_key, mailbox_object_id,
                      uid_validity, uid, email_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(account_id, target_key, folder_key, uid_validity, uid)
                 DO UPDATE SET
                     mailbox_object_id = coalesce(
                         excluded.mailbox_object_id,
                         message_tombstone_imap_locations.mailbox_object_id
                     ),
                     email_id = coalesce(
                         excluded.email_id,
                         message_tombstone_imap_locations.email_id
                     )",
                params![
                    claim.account_id,
                    &claim.target_key,
                    &source.folder_key,
                    &source.mailbox_object_id,
                    i64::from(source.uid_validity),
                    i64::from(source.uid),
                    &source.email_id,
                ],
            )
            .map_err(map_journal_error)?;
    }
    transaction
        .execute(
            "INSERT INTO imap_message_locations
                 (message_id, folder_id, account_id, uid_validity, uid,
                  modseq, email_id, remote_seen, remote_flagged)
             SELECT i.message_id, f.id, i.account_id, ?3, ?4, ?5, ?6, ?7, ?8
             FROM remote_change_intents AS i
             JOIN folders AS f
               ON f.account_id = i.account_id AND f.remote_key = ?2
             JOIN message_folders AS mf
               ON mf.message_id = i.message_id AND mf.folder_id = f.id
             WHERE i.id = ?1 AND i.message_id IS NOT NULL
             ON CONFLICT(message_id, folder_id) DO UPDATE SET
                 uid_validity = excluded.uid_validity,
                 uid = excluded.uid,
                 modseq = CASE
                     WHEN imap_message_locations.uid_validity <> excluded.uid_validity
                       OR imap_message_locations.uid <> excluded.uid
                     THEN excluded.modseq
                     WHEN imap_message_locations.modseq IS NULL THEN excluded.modseq
                     WHEN excluded.modseq IS NULL THEN imap_message_locations.modseq
                     ELSE max(imap_message_locations.modseq, excluded.modseq)
                 END,
                 email_id = CASE
                     WHEN imap_message_locations.uid_validity <> excluded.uid_validity
                       OR imap_message_locations.uid <> excluded.uid
                     THEN excluded.email_id
                     ELSE coalesce(excluded.email_id, imap_message_locations.email_id)
                 END,
                 remote_seen = CASE
                     WHEN imap_message_locations.uid_validity = excluded.uid_validity
                      AND imap_message_locations.uid = excluded.uid
                      AND imap_message_locations.modseq IS NOT NULL
                      AND (excluded.modseq IS NULL OR excluded.modseq
                           < imap_message_locations.modseq)
                     THEN imap_message_locations.remote_seen
                     ELSE excluded.remote_seen
                 END,
                 remote_flagged = CASE
                     WHEN imap_message_locations.uid_validity = excluded.uid_validity
                      AND imap_message_locations.uid = excluded.uid
                      AND imap_message_locations.modseq IS NOT NULL
                      AND (excluded.modseq IS NULL OR excluded.modseq
                           < imap_message_locations.modseq)
                     THEN imap_message_locations.remote_flagged
                     ELSE excluded.remote_flagged
                 END",
            params![
                claim.lease.intent_id,
                &source.folder_key,
                i64::from(source.uid_validity),
                i64::from(source.uid),
                modseq,
                &source.email_id,
                source.remote_seen,
                source.remote_flagged,
            ],
        )
        .map_err(DbFailure::database)?;

    Ok(changed)
}

fn merge_checkpoint_into_claim(claim: &mut RemoteClaim, checkpoint: RemoteCheckpoint) {
    match checkpoint.kind {
        RemoteCheckpointKind::JmapEmailState(state_token) => {
            claim.jmap_email_state = Some(state_token);
        }
        RemoteCheckpointKind::ImapSources(sources) => {
            let mut merged = Vec::from(std::mem::take(&mut claim.imap_sources));
            for source in sources {
                if let Some(existing) = merged.iter_mut().find(|existing| {
                    existing.folder_key == source.folder_key
                        && existing.uid_validity == source.uid_validity
                        && existing.uid == source.uid
                }) {
                    let accepts_flags = existing
                        .modseq
                        .is_none_or(|previous| source.modseq.is_some_and(|next| next >= previous));
                    if source.mailbox_object_id.is_some() {
                        existing.mailbox_object_id = source.mailbox_object_id;
                    }
                    existing.modseq = match (existing.modseq, source.modseq) {
                        (Some(left), Some(right)) => Some(cmp::max(left, right)),
                        (left, right) => left.or(right),
                    };
                    if source.email_id.is_some() {
                        existing.email_id = source.email_id;
                    }
                    if accepts_flags {
                        existing.remote_seen = source.remote_seen;
                        existing.remote_flagged = source.remote_flagged;
                    }
                } else {
                    merged.push(source);
                }
            }
            claim.imap_sources = merged.into_boxed_slice();
        }
    }
}

fn reject_changed_stable_id(
    existing: Option<&str>,
    incoming: Option<&str>,
    field: &str,
) -> Result<(), DbFailure> {
    if existing.is_some() && incoming.is_some() && existing != incoming {
        Err(DbFailure::conflict(format!(
            "{field} changed for a stable IMAP locator"
        )))
    } else {
        Ok(())
    }
}

fn validate_imap_source(source: &RemoteImapSource) -> Result<(), DbFailure> {
    validate_text(&source.folder_key, MAX_TEXT_ID_BYTES, "IMAP folder key")?;
    if let Some(value) = source.mailbox_object_id.as_deref() {
        validate_text(value, MAX_TEXT_ID_BYTES, "IMAP mailbox object id")?;
    }
    if source.uid_validity == 0 || source.uid == 0 {
        return Err(DbFailure::conflict(
            "IMAP UIDVALIDITY and UID must be non-zero",
        ));
    }
    if source
        .modseq
        .is_some_and(|value| value == 0 || value > i64::MAX as u64)
    {
        return Err(DbFailure::resource_limit("invalid IMAP MODSEQ"));
    }
    if let Some(value) = source.email_id.as_deref() {
        validate_text(value, MAX_TEXT_ID_BYTES, "IMAP email id")?;
    }
    Ok(())
}

fn validate_text(value: &str, max_bytes: usize, field: &str) -> Result<(), DbFailure> {
    if value.is_empty() {
        Err(DbFailure::conflict(format!("{field} must not be empty")))
    } else if value.len() > max_bytes {
        Err(DbFailure::resource_limit(format!(
            "{field} exceeds {max_bytes} bytes"
        )))
    } else {
        Ok(())
    }
}

fn provider_name(provider: RemoteProvider) -> &'static str {
    match provider {
        RemoteProvider::Imap => "imap",
        RemoteProvider::Jmap => "jmap",
    }
}

fn checked_i64(value: u64, field: &str) -> Result<i64, DbFailure> {
    i64::try_from(value).map_err(|_| DbFailure::resource_limit(format!("invalid {field}")))
}

const _: () = {
    assert!(MAX_RETRY_DELAY_MS <= i32::MAX as u32);
    assert!(MAX_FOLDER_KEYS_PER_SIDE <= 256);
    assert!(MAX_IMAP_SOURCES <= 256);
};

#[cfg(test)]
mod tests;
