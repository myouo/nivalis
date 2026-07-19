use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use super::{
    domain::DbFailure,
    journal::{ensure_payload_budget, map_journal_error},
};

const MIN_TIMESTAMP_MS: i64 = -62_135_596_800_000;
const MAX_TIMESTAMP_MS: i64 = 253_402_300_799_999;
const REMOTE_LEASE_TTL_MS: i64 = 30_000;
const MAX_IMAP_SOURCES: usize = 256;
const MAX_FOLDER_KEYS_PER_SIDE: usize = 256;
const MAX_ATTEMPTS: i64 = 1_000;

const UNREAD_MASK: i64 = 1;
const STARRED_MASK: i64 = 2;
const PLACEMENT_MASK: i64 = 4;
const DELETE_MASK: i64 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RemoteProvider {
    Imap,
    Jmap,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RemoteWorkMode {
    Apply,
    Reconcile,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RemoteLease {
    pub(crate) intent_id: i64,
    pub(crate) leased_version: u64,
    pub(crate) claim_epoch: u64,
    pub(crate) expires_at_ms: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RemoteFlagDelta {
    pub(crate) base: bool,
    pub(crate) desired: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RemotePlacementDelta {
    pub(crate) base: Box<[Box<str>]>,
    pub(crate) desired: Box<[Box<str>]>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RemoteImapSource {
    pub(crate) folder_key: Box<str>,
    pub(crate) mailbox_object_id: Option<Box<str>>,
    pub(crate) uid_validity: u32,
    pub(crate) uid: u32,
    pub(crate) modseq: Option<u64>,
    pub(crate) email_id: Option<Box<str>>,
    pub(crate) remote_seen: bool,
    pub(crate) remote_flagged: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RemoteClaim {
    pub(crate) lease: RemoteLease,
    pub(crate) account_id: i64,
    pub(crate) provider: RemoteProvider,
    pub(crate) target_key: Box<str>,
    pub(crate) local_revision: u64,
    pub(crate) attempt_count: u16,
    pub(crate) mode: RemoteWorkMode,
    pub(crate) unread: Option<RemoteFlagDelta>,
    pub(crate) starred: Option<RemoteFlagDelta>,
    pub(crate) placement: Option<RemotePlacementDelta>,
    pub(crate) delete_requested: bool,
    pub(crate) imap_sources: Box<[RemoteImapSource]>,
    pub(crate) jmap_email_state: Option<Box<str>>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RemoteClaimOutcome {
    Claimed(Box<RemoteClaim>),
    Idle { wake_at_ms: Option<i64> },
    AccountReconciliationRequired,
}

struct Candidate {
    id: i64,
    state: String,
    claim_epoch: i64,
}

pub(super) fn claim_remote(
    connection: &mut Connection,
    account_id: i64,
    now_ms: i64,
) -> Result<RemoteClaimOutcome, DbFailure> {
    if account_id <= 0 {
        return Err(DbFailure::conflict("remote claim account must be positive"));
    }
    validate_timestamp(now_ms)?;
    let expires_at_ms = now_ms
        .checked_add(REMOTE_LEASE_TTL_MS)
        .ok_or_else(|| DbFailure::resource_limit("remote lease expiry overflow"))?;
    validate_timestamp(expires_at_ms)?;

    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(DbFailure::database)?;
    let provider = require_supported_account(&transaction, account_id)?;
    let (transaction, provider) = if recover_one_expired_lease(&transaction, now_ms)? {
        transaction.commit().map_err(DbFailure::database)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(DbFailure::database)?;
        let provider = require_supported_account(&transaction, account_id)?;
        (transaction, provider)
    } else {
        (transaction, provider)
    };

    if account_reconciliation_required(&transaction, account_id)? {
        transaction.commit().map_err(DbFailure::database)?;
        return Ok(RemoteClaimOutcome::AccountReconciliationRequired);
    }

    if let Some(wake_at_ms) = active_lease_expiry(&transaction)? {
        transaction.commit().map_err(DbFailure::database)?;
        return Ok(RemoteClaimOutcome::Idle {
            wake_at_ms: Some(wake_at_ms),
        });
    }

    block_exhausted_intents(&transaction, account_id, now_ms)?;
    let Some(candidate) = select_candidate(&transaction, account_id, now_ms)? else {
        let wake_at_ms = next_account_wake(&transaction, account_id)?;
        transaction.commit().map_err(DbFailure::database)?;
        return Ok(RemoteClaimOutcome::Idle { wake_at_ms });
    };
    if candidate.claim_epoch == i64::MAX {
        return Err(DbFailure::resource_limit("remote claim epoch exhausted"));
    }
    ensure_payload_budget(&transaction, candidate.id)?;
    let work_mode = claim_work_mode(&transaction, &candidate, provider)?;
    let folder_reserve = placement_folder_reserve(&transaction, candidate.id, work_mode)?;

    let active_mask: i64 = transaction
        .query_row(
            "SELECT
                 (unread_base IS NOT NULL) * ?2
                 | (starred_base IS NOT NULL) * ?3
                 | placement_active * ?4
                 | delete_requested * ?5
             FROM remote_change_intents WHERE id = ?1",
            params![
                candidate.id,
                UNREAD_MASK,
                STARRED_MASK,
                PLACEMENT_MASK,
                DELETE_MASK,
            ],
            |row| row.get(0),
        )
        .map_err(DbFailure::database)?;
    let updated = transaction
        .execute(
            "UPDATE remote_change_intents
             SET state = 'in_flight',
                 leased_version = intent_version,
                 claim_epoch = claim_epoch + 1,
                 lease_expires_at_ms = ?3,
                 attempt_count = attempt_count + 1,
                 dispatched_mask = dispatched_mask | ?4,
                 leased_folder_reserve = ?8,
                 updated_at_ms = ?2
             WHERE id = ?1
               AND state = ?5
               AND not_before_ms <= ?2
               AND attempt_count < ?6
               AND claim_epoch = ?7",
            params![
                candidate.id,
                now_ms,
                expires_at_ms,
                if work_mode == RemoteWorkMode::Apply {
                    active_mask
                } else {
                    0
                },
                candidate.state,
                MAX_ATTEMPTS,
                candidate.claim_epoch,
                folder_reserve,
            ],
        )
        .map_err(map_journal_error)?;
    if updated != 1 {
        return Err(DbFailure::conflict(
            "remote intent changed while it was being claimed",
        ));
    }

    let claim = load_claim(&transaction, candidate.id, work_mode)?;
    transaction.commit().map_err(DbFailure::database)?;
    Ok(RemoteClaimOutcome::Claimed(Box::new(claim)))
}

fn recover_one_expired_lease(
    transaction: &Transaction<'_>,
    now_ms: i64,
) -> Result<bool, DbFailure> {
    let expired: Option<(i64, String, i64, bool, i64)> = transaction
        .query_row(
            "SELECT i.id, a.provider, i.dispatched_mask,
                    i.reconcile_requested, i.attempt_count
             FROM remote_change_intents AS i
             JOIN accounts AS a ON a.id = i.account_id
             WHERE i.state = 'in_flight' AND i.lease_expires_at_ms <= ?1
             ORDER BY i.lease_expires_at_ms, i.id
             LIMIT 1",
            [now_ms],
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
    let Some((id, provider, dispatched_mask, reconcile_requested, attempts)) = expired else {
        return Ok(false);
    };

    let exhausted = attempts >= MAX_ATTEMPTS;
    let is_imap = provider.eq_ignore_ascii_case("imap");
    let is_jmap = provider.eq_ignore_ascii_case("jmap");
    let unsupported_provider = !is_imap && !is_jmap;
    let ambiguous_imap = is_imap && dispatched_mask & (PLACEMENT_MASK | DELETE_MASK) != 0;
    let state = if exhausted || unsupported_provider {
        "blocked"
    } else if ambiguous_imap || reconcile_requested {
        "reconcile"
    } else {
        "ready"
    };
    let error_class = if exhausted || unsupported_provider {
        "permanent"
    } else if ambiguous_imap {
        "conflict"
    } else {
        "network"
    };
    let error_code = if exhausted {
        "attempt_limit"
    } else if unsupported_provider {
        "unsupported_provider"
    } else if ambiguous_imap {
        "lease_expired_ambiguous"
    } else {
        "lease_expired_replay"
    };
    let error_detail = if exhausted {
        "Remote synchronization stopped after 1,000 attempts; review the account before retrying."
    } else if unsupported_provider {
        "The account uses an unsupported remote provider; update or remove the account before retrying."
    } else if ambiguous_imap {
        "An IMAP placement or deletion lease expired; synchronize remote state before another write."
    } else {
        "The previous idempotent remote attempt did not report completion and can be retried safely."
    };
    let updated = transaction
        .execute(
            "UPDATE remote_change_intents
             SET state = ?2,
                 leased_version = NULL,
                 lease_expires_at_ms = NULL,
                 leased_folder_reserve = 0,
                 reconcile_requested = CASE
                     WHEN ?3 AND delete_requested = 0 THEN 1
                     ELSE reconcile_requested
                 END,
                 not_before_ms = ?4,
                 error_class = ?5,
                 error_code = ?6,
                 error_detail = ?7,
                 updated_at_ms = ?4
             WHERE id = ?1 AND state = 'in_flight' AND lease_expires_at_ms <= ?4",
            params![
                id,
                state,
                ambiguous_imap,
                now_ms,
                error_class,
                error_code,
                error_detail,
            ],
        )
        .map_err(DbFailure::database)?;
    Ok(updated == 1)
}

fn require_supported_account(
    transaction: &Transaction<'_>,
    account_id: i64,
) -> Result<RemoteProvider, DbFailure> {
    let provider = transaction
        .query_row(
            "SELECT provider FROM accounts WHERE id = ?1",
            [account_id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(DbFailure::database)?
        .ok_or_else(|| DbFailure::not_found("remote claim account does not exist"))?;
    parse_provider(&provider)
}

fn account_reconciliation_required(
    transaction: &Transaction<'_>,
    account_id: i64,
) -> Result<bool, DbFailure> {
    transaction
        .query_row(
            "SELECT EXISTS (
                 SELECT 1 FROM remote_account_reconciliations WHERE account_id = ?1
             )",
            [account_id],
            |row| row.get(0),
        )
        .map_err(DbFailure::database)
}

fn active_lease_expiry(transaction: &Transaction<'_>) -> Result<Option<i64>, DbFailure> {
    transaction
        .query_row(
            "SELECT min(lease_expires_at_ms)
             FROM remote_change_intents WHERE state = 'in_flight'",
            [],
            |row| row.get(0),
        )
        .map_err(DbFailure::database)
}

fn block_exhausted_intents(
    transaction: &Transaction<'_>,
    account_id: i64,
    now_ms: i64,
) -> Result<(), DbFailure> {
    transaction
        .execute(
            "UPDATE remote_change_intents
             SET state = 'blocked',
                 error_class = 'permanent',
                 error_code = 'attempt_limit',
                 error_detail =
                     'Remote synchronization stopped after 1,000 attempts; review the account before retrying.',
                 updated_at_ms = ?2
             WHERE account_id = ?1
               AND state IN ('ready', 'retry_wait', 'reconcile')
               AND attempt_count >= ?3",
            params![account_id, now_ms, MAX_ATTEMPTS],
        )
        .map_err(DbFailure::database)?;
    Ok(())
}

fn select_candidate(
    transaction: &Transaction<'_>,
    account_id: i64,
    now_ms: i64,
) -> Result<Option<Candidate>, DbFailure> {
    transaction
        .query_row(
            "SELECT id, state, claim_epoch
             FROM remote_change_intents
             WHERE account_id = ?1
               AND state IN ('ready', 'retry_wait', 'reconcile')
               AND not_before_ms <= ?2
               AND attempt_count < ?3
             ORDER BY not_before_ms, id
             LIMIT 1",
            params![account_id, now_ms, MAX_ATTEMPTS],
            |row| {
                Ok(Candidate {
                    id: row.get(0)?,
                    state: row.get(1)?,
                    claim_epoch: row.get(2)?,
                })
            },
        )
        .optional()
        .map_err(DbFailure::database)
}

fn claim_work_mode(
    transaction: &Transaction<'_>,
    candidate: &Candidate,
    provider: RemoteProvider,
) -> Result<RemoteWorkMode, DbFailure> {
    if candidate.state == "reconcile" {
        return Ok(RemoteWorkMode::Reconcile);
    }
    let reconcile_requested: bool = transaction
        .query_row(
            "SELECT reconcile_requested FROM remote_change_intents WHERE id = ?1",
            [candidate.id],
            |row| row.get(0),
        )
        .map_err(DbFailure::database)?;
    if reconcile_requested {
        return Ok(RemoteWorkMode::Reconcile);
    }
    let has_required_state: bool = match provider {
        RemoteProvider::Imap => transaction
            .query_row(
                "SELECT EXISTS (
                     SELECT 1 FROM remote_change_intent_imap_sources WHERE intent_id = ?1
                 )",
                [candidate.id],
                |row| row.get(0),
            )
            .map_err(DbFailure::database)?,
        RemoteProvider::Jmap => transaction
            .query_row(
                "SELECT EXISTS (
                     SELECT 1 FROM account_object_states AS states
                     JOIN remote_change_intents AS intents
                       ON intents.account_id = states.account_id
                     WHERE intents.id = ?1 AND states.object_kind = 'email'
                 )",
                [candidate.id],
                |row| row.get(0),
            )
            .map_err(DbFailure::database)?,
    };
    Ok(if has_required_state {
        RemoteWorkMode::Apply
    } else {
        RemoteWorkMode::Reconcile
    })
}

fn placement_folder_reserve(
    transaction: &Transaction<'_>,
    intent_id: i64,
    mode: RemoteWorkMode,
) -> Result<i64, DbFailure> {
    transaction
        .query_row(
            "SELECT CASE WHEN placement_active THEN
                 CASE WHEN ?2 THEN
                     (SELECT count(*) FROM remote_change_intent_folders
                      WHERE intent_id = ?1)
                 ELSE max(
                     0,
                     (SELECT count(*) FROM remote_change_intent_folders
                      WHERE intent_id = ?1 AND side = 'desired')
                     -
                     (SELECT count(*) FROM remote_change_intent_folders
                      WHERE intent_id = ?1 AND side = 'base')
                 ) END
             ELSE 0 END
             FROM remote_change_intents WHERE id = ?1",
            params![intent_id, mode == RemoteWorkMode::Reconcile],
            |row| row.get(0),
        )
        .map_err(DbFailure::database)
}

fn next_account_wake(
    transaction: &Transaction<'_>,
    account_id: i64,
) -> Result<Option<i64>, DbFailure> {
    transaction
        .query_row(
            "SELECT min(not_before_ms)
             FROM remote_change_intents
             WHERE account_id = ?1
               AND state IN ('ready', 'retry_wait', 'reconcile')
               AND attempt_count < ?2",
            params![account_id, MAX_ATTEMPTS],
            |row| row.get(0),
        )
        .map_err(DbFailure::database)
}

fn load_claim(
    transaction: &Transaction<'_>,
    intent_id: i64,
    mode: RemoteWorkMode,
) -> Result<RemoteClaim, DbFailure> {
    type ParentRow = (
        i64,
        String,
        String,
        i64,
        i64,
        i64,
        i64,
        Option<bool>,
        Option<bool>,
        Option<bool>,
        Option<bool>,
        bool,
        bool,
        i64,
    );
    let parent: ParentRow = transaction
        .query_row(
            "SELECT i.account_id, a.provider, i.target_key, i.local_revision,
                    i.leased_version, i.claim_epoch, i.lease_expires_at_ms,
                    i.unread_base, i.unread_desired,
                    i.starred_base, i.starred_desired,
                    i.placement_active, i.delete_requested,
                    i.attempt_count
             FROM remote_change_intents AS i
             JOIN accounts AS a ON a.id = i.account_id
             WHERE i.id = ?1 AND i.state = 'in_flight'",
            [intent_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                    row.get(10)?,
                    row.get(11)?,
                    row.get(12)?,
                    row.get(13)?,
                ))
            },
        )
        .map_err(DbFailure::database)?;
    let (
        account_id,
        provider,
        target_key,
        local_revision,
        leased_version,
        claim_epoch,
        expires_at_ms,
        unread_base,
        unread_desired,
        starred_base,
        starred_desired,
        placement_active,
        delete_requested,
        attempt_count,
    ) = parent;

    let provider = parse_provider(&provider)?;
    let placement = if placement_active {
        Some(load_placement(transaction, intent_id)?)
    } else {
        None
    };
    let imap_sources = load_imap_sources(transaction, intent_id)?;
    let jmap_email_state = if provider == RemoteProvider::Jmap {
        transaction
            .query_row(
                "SELECT state_token FROM account_object_states
                 WHERE account_id = ?1 AND object_kind = 'email'",
                [account_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(DbFailure::database)?
            .map(String::into_boxed_str)
    } else {
        None
    };
    Ok(RemoteClaim {
        lease: RemoteLease {
            intent_id,
            leased_version: checked_u64(leased_version, "remote leased version")?,
            claim_epoch: checked_u64(claim_epoch, "remote claim epoch")?,
            expires_at_ms,
        },
        account_id,
        provider,
        target_key: target_key.into_boxed_str(),
        local_revision: checked_u64(local_revision, "remote local revision")?,
        attempt_count: u16::try_from(attempt_count)
            .map_err(|_| DbFailure::resource_limit("remote attempt count exceeds bounds"))?,
        mode,
        unread: flag_delta(unread_base, unread_desired, "unread")?,
        starred: flag_delta(starred_base, starred_desired, "starred")?,
        placement,
        delete_requested,
        imap_sources,
        jmap_email_state,
    })
}

fn load_placement(
    transaction: &Transaction<'_>,
    intent_id: i64,
) -> Result<RemotePlacementDelta, DbFailure> {
    let (base_count, desired_count): (i64, i64) = transaction
        .query_row(
            "SELECT
                 coalesce(sum(CASE WHEN side = 'base' THEN 1 ELSE 0 END), 0),
                 coalesce(sum(CASE WHEN side = 'desired' THEN 1 ELSE 0 END), 0)
             FROM remote_change_intent_folders WHERE intent_id = ?1",
            [intent_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(DbFailure::database)?;
    let base_count = bounded_count(
        base_count,
        MAX_FOLDER_KEYS_PER_SIDE,
        "remote placement base folder count",
    )?;
    let desired_count = bounded_count(
        desired_count,
        MAX_FOLDER_KEYS_PER_SIDE,
        "remote placement desired folder count",
    )?;
    let mut statement = transaction
        .prepare(
            "SELECT side, folder_key
             FROM remote_change_intent_folders
             WHERE intent_id = ?1
             ORDER BY side, folder_key",
        )
        .map_err(DbFailure::database)?;
    let rows = statement
        .query_map([intent_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(DbFailure::database)?;
    let mut base = Vec::with_capacity(base_count);
    let mut desired = Vec::with_capacity(desired_count);
    for row in rows {
        let (side, key) = row.map_err(DbFailure::database)?;
        let destination = if side == "base" {
            &mut base
        } else if side == "desired" {
            &mut desired
        } else {
            return Err(DbFailure::conflict("remote placement side is invalid"));
        };
        if destination.len() == MAX_FOLDER_KEYS_PER_SIDE {
            return Err(DbFailure::resource_limit(
                "remote placement exceeds folder bounds",
            ));
        }
        destination.push(key.into_boxed_str());
    }
    Ok(RemotePlacementDelta {
        base: base.into_boxed_slice(),
        desired: desired.into_boxed_slice(),
    })
}

fn load_imap_sources(
    transaction: &Transaction<'_>,
    intent_id: i64,
) -> Result<Box<[RemoteImapSource]>, DbFailure> {
    let source_count: i64 = transaction
        .query_row(
            "SELECT count(*) FROM remote_change_intent_imap_sources WHERE intent_id = ?1",
            [intent_id],
            |row| row.get(0),
        )
        .map_err(DbFailure::database)?;
    let source_count = bounded_count(
        source_count,
        MAX_IMAP_SOURCES,
        "remote claim IMAP source count",
    )?;
    let mut statement = transaction
        .prepare(
            "SELECT folder_key, mailbox_object_id, uid_validity, uid, modseq,
                    email_id, remote_seen, remote_flagged
             FROM remote_change_intent_imap_sources
             WHERE intent_id = ?1
             ORDER BY folder_key, uid_validity, uid",
        )
        .map_err(DbFailure::database)?;
    let rows = statement
        .query_map([intent_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, bool>(6)?,
                row.get::<_, bool>(7)?,
            ))
        })
        .map_err(DbFailure::database)?;
    let mut sources = Vec::with_capacity(source_count);
    for row in rows {
        if sources.len() == MAX_IMAP_SOURCES {
            return Err(DbFailure::resource_limit(
                "remote claim exceeds IMAP source bounds",
            ));
        }
        let (folder_key, mailbox_object_id, uid_validity, uid, modseq, email_id, seen, flagged) =
            row.map_err(DbFailure::database)?;
        sources.push(RemoteImapSource {
            folder_key: folder_key.into_boxed_str(),
            mailbox_object_id: mailbox_object_id.map(String::into_boxed_str),
            uid_validity: checked_u32(uid_validity, "IMAP UIDVALIDITY")?,
            uid: checked_u32(uid, "IMAP UID")?,
            modseq: modseq
                .map(|value| checked_u64(value, "IMAP MODSEQ"))
                .transpose()?,
            email_id: email_id.map(String::into_boxed_str),
            remote_seen: seen,
            remote_flagged: flagged,
        });
    }
    Ok(sources.into_boxed_slice())
}

fn flag_delta(
    base: Option<bool>,
    desired: Option<bool>,
    name: &str,
) -> Result<Option<RemoteFlagDelta>, DbFailure> {
    match (base, desired) {
        (None, None) => Ok(None),
        (Some(base), Some(desired)) => Ok(Some(RemoteFlagDelta { base, desired })),
        _ => Err(DbFailure::conflict(format!(
            "remote {name} delta is incomplete"
        ))),
    }
}

fn parse_provider(provider: &str) -> Result<RemoteProvider, DbFailure> {
    if provider.eq_ignore_ascii_case("imap") {
        Ok(RemoteProvider::Imap)
    } else if provider.eq_ignore_ascii_case("jmap") {
        Ok(RemoteProvider::Jmap)
    } else {
        Err(DbFailure::conflict(format!(
            "unsupported remote provider: {provider}"
        )))
    }
}

fn checked_u64(value: i64, field: &str) -> Result<u64, DbFailure> {
    u64::try_from(value).map_err(|_| DbFailure::resource_limit(format!("invalid {field}")))
}

fn checked_u32(value: i64, field: &str) -> Result<u32, DbFailure> {
    u32::try_from(value).map_err(|_| DbFailure::resource_limit(format!("invalid {field}")))
}

fn bounded_count(value: i64, maximum: usize, field: &str) -> Result<usize, DbFailure> {
    let value = usize::try_from(value)
        .map_err(|_| DbFailure::resource_limit(format!("invalid {field}")))?;
    if value > maximum {
        Err(DbFailure::resource_limit(format!("{field} exceeds bounds")))
    } else {
        Ok(value)
    }
}

fn validate_timestamp(timestamp_ms: i64) -> Result<(), DbFailure> {
    if (MIN_TIMESTAMP_MS..=MAX_TIMESTAMP_MS).contains(&timestamp_ms) {
        Ok(())
    } else {
        Err(DbFailure::resource_limit(
            "timestamp is outside SQLite bounds",
        ))
    }
}

#[cfg(test)]
mod tests {
    use rusqlite::{Connection, params};

    use super::*;
    use crate::store::sqlite::{domain::FailureKind, migrations::migrate};

    const NOW_MS: i64 = 1_000;

    fn database() -> Connection {
        let mut connection = Connection::open_in_memory().expect("open in-memory database");
        migrate(&mut connection).expect("apply migrations");
        connection
    }

    fn insert_account(connection: &Connection, id: i64, provider: &str) {
        connection
            .execute(
                "INSERT INTO accounts
                     (id, provider, remote_key, name, address, state, accent_rgb)
                 VALUES (?1, ?2, ?3, 'Test', ?4, 'active', 0)",
                params![
                    id,
                    provider,
                    format!("account-{id}"),
                    format!("{id}@example.test")
                ],
            )
            .expect("insert account");
    }

    fn insert_flag_intent(
        connection: &Connection,
        account_id: i64,
        target_key: &str,
        not_before_ms: i64,
    ) -> i64 {
        connection
            .execute(
                "INSERT INTO remote_change_intents
                     (account_id, target_key, local_revision,
                      unread_base, unread_desired, not_before_ms,
                      created_at_ms, updated_at_ms)
                 VALUES (?1, ?2, 7, 1, 0, ?3, 0, 0)",
                params![account_id, target_key, not_before_ms],
            )
            .expect("insert flag intent");
        connection.last_insert_rowid()
    }

    fn insert_source(connection: &Connection, intent_id: i64, uid: i64) {
        connection
            .execute(
                "INSERT INTO remote_change_intent_imap_sources
                     (intent_id, folder_key, mailbox_object_id, uid_validity, uid,
                      modseq, email_id, remote_seen, remote_flagged)
                 VALUES (?1, 'inbox', 'mailbox', 11, ?2, 23, 'email', 0, 1)",
                params![intent_id, uid],
            )
            .expect("insert IMAP source");
    }

    fn insert_max_sources(
        connection: &mut Connection,
        intent_id: i64,
        mailbox_bytes: usize,
        email_bytes: usize,
    ) {
        let mailbox_object_id = "m".repeat(mailbox_bytes);
        let email_id = "e".repeat(email_bytes);
        let transaction = connection.transaction().unwrap();
        {
            let mut insert = transaction
                .prepare(
                    "INSERT INTO remote_change_intent_imap_sources
                         (intent_id, folder_key, mailbox_object_id,
                          uid_validity, uid, email_id,
                          remote_seen, remote_flagged)
                     VALUES (?1, ?2, ?3, 1, ?4, ?5, 0, 0)",
                )
                .unwrap();
            for index in 0..MAX_IMAP_SOURCES {
                let folder_key = format!("{index:03}{}", "f".repeat(509));
                insert
                    .execute(params![
                        intent_id,
                        folder_key,
                        mailbox_object_id,
                        index as i64 + 1,
                        email_id,
                    ])
                    .unwrap();
            }
        }
        transaction.commit().unwrap();
    }

    fn claimed(outcome: RemoteClaimOutcome) -> Box<RemoteClaim> {
        let RemoteClaimOutcome::Claimed(claim) = outcome else {
            panic!("expected a claimed remote intent");
        };
        claim
    }

    #[test]
    fn claims_one_due_intent_with_a_bounded_snapshot() {
        let mut connection = database();
        insert_account(&connection, 1, "imap");
        let intent_id = insert_flag_intent(&connection, 1, "message-1", 0);
        insert_source(&connection, intent_id, 42);

        let claim = claimed(claim_remote(&mut connection, 1, NOW_MS).unwrap());

        assert_eq!(claim.lease.intent_id, intent_id);
        assert_eq!(claim.lease.leased_version, 1);
        assert_eq!(claim.lease.claim_epoch, 1);
        assert_eq!(claim.lease.expires_at_ms, 31_000);
        assert_eq!(claim.account_id, 1);
        assert_eq!(claim.provider, RemoteProvider::Imap);
        assert_eq!(&*claim.target_key, "message-1");
        assert_eq!(claim.local_revision, 7);
        assert_eq!(claim.attempt_count, 1);
        assert_eq!(claim.mode, RemoteWorkMode::Apply);
        assert_eq!(
            claim.unread,
            Some(RemoteFlagDelta {
                base: true,
                desired: false,
            })
        );
        assert_eq!(claim.starred, None);
        assert_eq!(claim.placement, None);
        assert!(!claim.delete_requested);
        assert_eq!(claim.imap_sources.len(), 1);
        assert_eq!(claim.imap_sources[0].uid, 42);
        assert_eq!(claim.jmap_email_state, None);

        let stored: (String, i64, i64, i64, i64, i64) = connection
            .query_row(
                "SELECT state, leased_version, claim_epoch, lease_expires_at_ms,
                        attempt_count, dispatched_mask
                 FROM remote_change_intents WHERE id = ?1",
                [intent_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(stored, ("in_flight".into(), 1, 1, 31_000, 1, UNREAD_MASK));
    }

    #[test]
    fn account_reconciliation_gate_prevents_leasing() {
        let mut connection = database();
        insert_account(&connection, 1, "imap");
        let intent_id = insert_flag_intent(&connection, 1, "message-1", 0);
        connection
            .execute(
                "INSERT INTO remote_account_reconciliations
                     (account_id, reason, requested_at_ms)
                 VALUES (1, 'legacy_journal_bootstrap', 0)",
                [],
            )
            .unwrap();

        assert_eq!(
            claim_remote(&mut connection, 1, NOW_MS).unwrap(),
            RemoteClaimOutcome::AccountReconciliationRequired
        );
        let stored: (String, Option<i64>, i64) = connection
            .query_row(
                "SELECT state, leased_version, attempt_count
                 FROM remote_change_intents WHERE id = ?1",
                [intent_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(stored, ("ready".into(), None, 0));
    }

    #[test]
    fn one_global_active_lease_bounds_provider_memory() {
        let mut connection = database();
        insert_account(&connection, 1, "imap");
        insert_account(&connection, 2, "jmap");
        let active_id = insert_flag_intent(&connection, 1, "active", 0);
        connection
            .execute(
                "UPDATE remote_change_intents
                 SET state = 'in_flight', leased_version = 1, claim_epoch = 1,
                     lease_expires_at_ms = 50_000, attempt_count = 1
                 WHERE id = ?1",
                [active_id],
            )
            .unwrap();
        let waiting_id = insert_flag_intent(&connection, 2, "waiting", 0);

        assert_eq!(
            claim_remote(&mut connection, 2, NOW_MS).unwrap(),
            RemoteClaimOutcome::Idle {
                wake_at_ms: Some(50_000),
            }
        );
        let waiting: (String, Option<i64>, i64) = connection
            .query_row(
                "SELECT state, leased_version, attempt_count
                 FROM remote_change_intents WHERE id = ?1",
                [waiting_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(waiting, ("ready".into(), None, 0));
    }

    #[test]
    fn expired_imap_placement_is_reconciled_before_another_write() {
        let mut connection = database();
        insert_account(&connection, 1, "imap");
        connection
            .execute(
                "INSERT INTO remote_change_intents
                     (account_id, target_key, local_revision, placement_active,
                      dispatched_mask, state, leased_version, claim_epoch,
                      lease_expires_at_ms, attempt_count, not_before_ms,
                      created_at_ms, updated_at_ms)
                 VALUES (1, 'message-1', 3, 1, ?1, 'in_flight', 1, 1,
                         500, 1, 0, 0, 0)",
                [PLACEMENT_MASK],
            )
            .unwrap();
        let intent_id = connection.last_insert_rowid();
        connection
            .execute(
                "INSERT INTO remote_change_intent_folders (intent_id, side, folder_key)
                 VALUES (?1, 'base', 'inbox'), (?1, 'desired', 'archive')",
                [intent_id],
            )
            .unwrap();

        let claim = claimed(claim_remote(&mut connection, 1, NOW_MS).unwrap());

        assert_eq!(claim.mode, RemoteWorkMode::Reconcile);
        assert_eq!(claim.attempt_count, 2);
        assert_eq!(claim.placement.as_ref().unwrap().base.len(), 1);
        assert_eq!(claim.placement.as_ref().unwrap().desired.len(), 1);
        let stored: (bool, String, Option<String>) = connection
            .query_row(
                "SELECT reconcile_requested, error_code, error_detail
                 FROM remote_change_intents WHERE id = ?1",
                [intent_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert!(stored.0);
        assert_eq!(stored.1, "lease_expired_ambiguous");
        assert!(stored.2.is_some());
    }

    #[test]
    fn placement_claim_reserves_rebase_capacity_until_recovery() {
        let mut connection = database();
        insert_account(&connection, 1, "imap");
        connection
            .execute(
                "INSERT INTO remote_change_intents
                     (account_id, target_key, local_revision, placement_active,
                      not_before_ms, created_at_ms, updated_at_ms)
                 VALUES (1, 'message-1', 3, 1, 0, 0, 0)",
                [],
            )
            .unwrap();
        let intent_id = connection.last_insert_rowid();
        connection
            .execute(
                "INSERT INTO remote_change_intent_folders (intent_id, side, folder_key)
                 VALUES (?1, 'desired', 'archive'), (?1, 'desired', 'label')",
                [intent_id],
            )
            .unwrap();
        insert_source(&connection, intent_id, 42);

        let claim = claimed(claim_remote(&mut connection, 1, NOW_MS).unwrap());

        assert_eq!(claim.mode, RemoteWorkMode::Apply);
        let usage: (i64, i64) = connection
            .query_row(
                "SELECT child_count, reserved_count
                 FROM remote_journal_usage WHERE singleton = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(usage, (3, 2));
        let reserve: i64 = connection
            .query_row(
                "SELECT leased_folder_reserve FROM remote_change_intents WHERE id = ?1",
                [intent_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(reserve, 2);

        connection
            .execute(
                "INSERT INTO remote_account_reconciliations
                     (account_id, reason, requested_at_ms)
                 VALUES (1, 'legacy_journal_bootstrap', 0)",
                [],
            )
            .unwrap();
        assert_eq!(
            claim_remote(&mut connection, 1, claim.lease.expires_at_ms).unwrap(),
            RemoteClaimOutcome::AccountReconciliationRequired
        );
        let recovered: (String, Option<i64>, i64) = connection
            .query_row(
                "SELECT state, leased_version, leased_folder_reserve
                 FROM remote_change_intents WHERE id = ?1",
                [intent_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(recovered, ("reconcile".into(), None, 0));
        let reserved_after_recovery: i64 = connection
            .query_row(
                "SELECT reserved_count FROM remote_journal_usage WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(reserved_after_recovery, 0);
    }

    #[test]
    fn reconciliation_claim_reserves_the_complete_placement_snapshot() {
        let mut connection = database();
        insert_account(&connection, 1, "imap");
        connection
            .execute(
                "INSERT INTO remote_change_intents
                     (account_id, target_key, local_revision, placement_active,
                      not_before_ms, created_at_ms, updated_at_ms)
                 VALUES (1, 'message-1', 3, 1, 0, 0, 0)",
                [],
            )
            .unwrap();
        let intent_id = connection.last_insert_rowid();
        connection
            .execute(
                "INSERT INTO remote_change_intent_folders (intent_id, side, folder_key)
                 VALUES (?1, 'base', 'inbox'), (?1, 'desired', 'archive')",
                [intent_id],
            )
            .unwrap();

        let claim = claimed(claim_remote(&mut connection, 1, NOW_MS).unwrap());

        assert_eq!(claim.mode, RemoteWorkMode::Reconcile);
        let usage: (i64, i64) = connection
            .query_row(
                "SELECT child_count, reserved_count
                 FROM remote_journal_usage WHERE singleton = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(usage, (2, 2));
        let reserve: i64 = connection
            .query_row(
                "SELECT leased_folder_reserve FROM remote_change_intents WHERE id = ?1",
                [intent_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(reserve, 2);
    }

    #[test]
    fn placement_claim_rolls_back_when_rebase_capacity_is_full() {
        let mut connection = database();
        insert_account(&connection, 1, "imap");
        connection
            .execute(
                "INSERT INTO remote_change_intents
                     (account_id, target_key, local_revision, placement_active,
                      not_before_ms, created_at_ms, updated_at_ms)
                 VALUES (1, 'message-1', 3, 1, 0, 0, 0)",
                [],
            )
            .unwrap();
        let intent_id = connection.last_insert_rowid();
        connection
            .execute(
                "INSERT INTO remote_change_intent_folders (intent_id, side, folder_key)
                 VALUES (?1, 'desired', 'archive'), (?1, 'desired', 'label')",
                [intent_id],
            )
            .unwrap();
        insert_source(&connection, intent_id, 42);
        connection
            .execute(
                "UPDATE remote_journal_usage SET child_count = 65535 WHERE singleton = 1",
                [],
            )
            .unwrap();

        let failure = claim_remote(&mut connection, 1, NOW_MS).unwrap_err();

        assert_eq!(failure.kind, FailureKind::ResourceLimit);
        let stored: (String, Option<i64>, i64, i64) = connection
            .query_row(
                "SELECT state, leased_version, attempt_count, leased_folder_reserve
                 FROM remote_change_intents WHERE id = ?1",
                [intent_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(stored, ("ready".into(), None, 0, 0));
        let reserved: i64 = connection
            .query_row(
                "SELECT reserved_count FROM remote_journal_usage WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(reserved, 0);
    }

    #[test]
    fn expired_imap_flag_write_is_replayed_idempotently() {
        let mut connection = database();
        insert_account(&connection, 1, "imap");
        let intent_id = insert_flag_intent(&connection, 1, "message-1", 0);
        insert_source(&connection, intent_id, 42);
        connection
            .execute(
                "UPDATE remote_change_intents
                 SET state = 'in_flight', leased_version = 1, claim_epoch = 1,
                     lease_expires_at_ms = 500, attempt_count = 1,
                     dispatched_mask = ?2
                 WHERE id = ?1",
                params![intent_id, UNREAD_MASK],
            )
            .unwrap();

        let claim = claimed(claim_remote(&mut connection, 1, NOW_MS).unwrap());

        assert_eq!(claim.mode, RemoteWorkMode::Apply);
        assert_eq!(claim.attempt_count, 2);
        let error_code: String = connection
            .query_row(
                "SELECT error_code FROM remote_change_intents WHERE id = ?1",
                [intent_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(error_code, "lease_expired_replay");
    }

    #[test]
    fn unsupported_expired_provider_is_blocked_conservatively() {
        let mut connection = database();
        insert_account(&connection, 1, "imap");
        insert_account(&connection, 2, "unknown");
        let unknown_id = insert_flag_intent(&connection, 2, "unknown-message", 0);
        connection
            .execute(
                "UPDATE remote_change_intents
                 SET state = 'in_flight', leased_version = 1, claim_epoch = 1,
                     lease_expires_at_ms = 500, attempt_count = 1,
                     dispatched_mask = ?2
                 WHERE id = ?1",
                params![unknown_id, UNREAD_MASK],
            )
            .unwrap();
        let valid_id = insert_flag_intent(&connection, 1, "valid-message", 0);

        let claim = claimed(claim_remote(&mut connection, 1, NOW_MS).unwrap());

        assert_eq!(claim.lease.intent_id, valid_id);
        let unknown: (String, Option<i64>, String) = connection
            .query_row(
                "SELECT state, leased_version, error_code
                 FROM remote_change_intents WHERE id = ?1",
                [unknown_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            unknown,
            ("blocked".into(), None, "unsupported_provider".into())
        );
    }

    #[test]
    fn exhausted_and_epoch_overflow_intents_are_never_leased() {
        let mut connection = database();
        insert_account(&connection, 1, "imap");
        let exhausted_id = insert_flag_intent(&connection, 1, "exhausted", 0);
        connection
            .execute(
                "UPDATE remote_change_intents SET attempt_count = ?2 WHERE id = ?1",
                params![exhausted_id, MAX_ATTEMPTS],
            )
            .unwrap();
        let second_exhausted_id = insert_flag_intent(&connection, 1, "also-exhausted", 0);
        connection
            .execute(
                "UPDATE remote_change_intents SET attempt_count = ?2 WHERE id = ?1",
                params![second_exhausted_id, MAX_ATTEMPTS],
            )
            .unwrap();

        assert_eq!(
            claim_remote(&mut connection, 1, NOW_MS).unwrap(),
            RemoteClaimOutcome::Idle { wake_at_ms: None }
        );
        let exhausted: (String, String) = connection
            .query_row(
                "SELECT state, error_code FROM remote_change_intents WHERE id = ?1",
                [exhausted_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(exhausted, ("blocked".into(), "attempt_limit".into()));
        let second_state: String = connection
            .query_row(
                "SELECT state FROM remote_change_intents WHERE id = ?1",
                [second_exhausted_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(second_state, "blocked");

        let epoch_id = insert_flag_intent(&connection, 1, "epoch-overflow", 0);
        connection
            .execute(
                "UPDATE remote_change_intents SET claim_epoch = ?2 WHERE id = ?1",
                params![epoch_id, i64::MAX],
            )
            .unwrap();
        let failure = claim_remote(&mut connection, 1, NOW_MS).unwrap_err();
        assert_eq!(failure.kind, FailureKind::ResourceLimit);
        let epoch: (String, Option<i64>, i64) = connection
            .query_row(
                "SELECT state, leased_version, attempt_count
                 FROM remote_change_intents WHERE id = ?1",
                [epoch_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(epoch, ("ready".into(), None, 0));
    }

    #[test]
    fn oversized_provider_snapshot_rolls_back_without_a_lease() {
        let mut connection = database();
        insert_account(&connection, 1, "imap");
        let intent_id = insert_flag_intent(&connection, 1, "large-message", 0);
        insert_max_sources(&mut connection, intent_id, 512, 512);

        let failure = claim_remote(&mut connection, 1, NOW_MS).unwrap_err();

        assert_eq!(failure.kind, FailureKind::ResourceLimit);
        let stored: (String, Option<i64>, Option<i64>, i64, i64) = connection
            .query_row(
                "SELECT state, leased_version, lease_expires_at_ms,
                        claim_epoch, attempt_count
                 FROM remote_change_intents WHERE id = ?1",
                [intent_id],
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
            .unwrap();
        assert_eq!(stored, ("ready".into(), None, None, 0, 0));
    }

    #[test]
    fn expired_lease_recovery_commits_before_a_reclaim_failure() {
        let mut connection = database();
        insert_account(&connection, 1, "imap");
        let intent_id = insert_flag_intent(&connection, 1, "large-message", 0);
        insert_max_sources(&mut connection, intent_id, 512, 512);
        connection
            .execute(
                "UPDATE remote_change_intents
                 SET state = 'in_flight', leased_version = 1, claim_epoch = 1,
                     lease_expires_at_ms = 500, attempt_count = 1,
                     dispatched_mask = ?2
                 WHERE id = ?1",
                params![intent_id, UNREAD_MASK],
            )
            .unwrap();

        let failure = claim_remote(&mut connection, 1, NOW_MS).unwrap_err();

        assert_eq!(failure.kind, FailureKind::ResourceLimit);
        let recovered: (String, Option<i64>, Option<i64>, i64, String) = connection
            .query_row(
                "SELECT state, leased_version, lease_expires_at_ms,
                        leased_folder_reserve, error_code
                 FROM remote_change_intents WHERE id = ?1",
                [intent_id],
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
            .unwrap();
        assert_eq!(
            recovered,
            ("ready".into(), None, None, 0, "lease_expired_replay".into(),)
        );
        let reserved: i64 = connection
            .query_row(
                "SELECT reserved_count FROM remote_journal_usage WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(reserved, 0);
    }

    #[test]
    fn jmap_budget_reserves_the_largest_future_state_token() {
        let mut connection = database();
        insert_account(&connection, 1, "jmap");
        let intent_id = insert_flag_intent(&connection, 1, "x", 0);
        // The raw snapshot is 511 bytes below the cap; a maximal state token needs 512.
        insert_max_sources(&mut connection, intent_id, 512, 125);

        let failure = claim_remote(&mut connection, 1, NOW_MS).unwrap_err();

        assert_eq!(failure.kind, FailureKind::ResourceLimit);
        let stored: (String, Option<i64>, i64) = connection
            .query_row(
                "SELECT state, leased_version, attempt_count
                 FROM remote_change_intents WHERE id = ?1",
                [intent_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(stored, ("ready".into(), None, 0));
    }

    #[test]
    fn jmap_without_an_email_state_requires_reconciliation() {
        for (state_token, expected_mode) in [
            (None, RemoteWorkMode::Reconcile),
            (Some("email-state"), RemoteWorkMode::Apply),
        ] {
            let mut connection = database();
            insert_account(&connection, 1, "jmap");
            insert_flag_intent(&connection, 1, "message-1", 0);
            if let Some(state_token) = state_token {
                connection
                    .execute(
                        "INSERT INTO account_object_states
                             (account_id, object_kind, state_token, updated_at_ms)
                         VALUES (1, 'email', ?1, 0)",
                        [state_token],
                    )
                    .unwrap();
            }

            let claim = claimed(claim_remote(&mut connection, 1, NOW_MS).unwrap());

            assert_eq!(claim.mode, expected_mode);
            assert_eq!(claim.jmap_email_state.as_deref(), state_token);
            let dispatched_mask: i64 = connection
                .query_row(
                    "SELECT dispatched_mask FROM remote_change_intents
                     WHERE id = ?1",
                    [claim.lease.intent_id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                dispatched_mask,
                if expected_mode == RemoteWorkMode::Apply {
                    UNREAD_MASK
                } else {
                    0
                }
            );
        }
    }

    #[test]
    fn idle_claim_returns_the_exact_next_due_time() {
        let mut connection = database();
        insert_account(&connection, 1, "imap");
        insert_flag_intent(&connection, 1, "future", 5_000);

        assert_eq!(
            claim_remote(&mut connection, 1, NOW_MS).unwrap(),
            RemoteClaimOutcome::Idle {
                wake_at_ms: Some(5_000),
            }
        );
    }

    #[test]
    fn invalid_account_or_provider_fails_before_leasing() {
        let mut connection = database();
        let missing = claim_remote(&mut connection, 1, NOW_MS).unwrap_err();
        assert_eq!(missing.kind, FailureKind::NotFound);

        insert_account(&connection, 1, "unsupported");
        let intent_id = insert_flag_intent(&connection, 1, "message-1", 0);
        let unsupported = claim_remote(&mut connection, 1, NOW_MS).unwrap_err();
        assert_eq!(unsupported.kind, FailureKind::Conflict);
        let state: String = connection
            .query_row(
                "SELECT state FROM remote_change_intents WHERE id = ?1",
                [intent_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(state, "ready");
    }
}
