use rusqlite::{Error as SqliteError, OptionalExtension, Transaction, params};

use super::domain::{DbFailure, MessageId};

const MIN_TIMESTAMP_MS: i64 = -62_135_596_800_000;
const MAX_TIMESTAMP_MS: i64 = 253_402_300_799_999;
const MAX_INTENTS_PER_ACCOUNT: i64 = 4_096;
const MAX_INTENTS_GLOBAL: i64 = 16_384;
const MAX_FOLDER_KEYS_PER_SIDE: i64 = 256;
const MAX_IMAP_SOURCES: i64 = 256;
const MAX_JOURNAL_CHILDREN: i64 = 65_536;
const MAX_PROVIDER_PAYLOAD_BYTES: i64 = 320 * 1_024;
const MISSING_IMAP_LOCATOR: &str = "missing_imap_locator";
const MISSING_IMAP_LOCATOR_DETAIL: &str =
    "No confirmed IMAP locator is available; synchronize the account before retrying.";

const UNREAD_MASK: i64 = 1;
const STARRED_MASK: i64 = 2;
const PLACEMENT_MASK: i64 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FlagDimension {
    Unread,
    Starred,
}

impl FlagDimension {
    fn mask(self) -> i64 {
        match self {
            Self::Unread => UNREAD_MASK,
            Self::Starred => STARRED_MASK,
        }
    }
}

pub(super) struct PlacementDraft {
    intent_id: i64,
    message_id: MessageId,
    local_revision: i64,
    now_ms: i64,
    eligible_at_ms: i64,
    existing: bool,
}

struct MessageIdentity {
    account_id: i64,
    target_key: String,
    provider: String,
    revision: i64,
    legacy_reconcile: bool,
}

struct IntentRow {
    id: i64,
    intent_version: i64,
    local_revision: i64,
    unread_base: Option<bool>,
    unread_desired: Option<bool>,
    starred_base: Option<bool>,
    starred_desired: Option<bool>,
    placement_active: bool,
    reconcile_requested: bool,
    delete_requested: bool,
    dispatched_mask: i64,
    state: String,
    error_code: Option<String>,
}

pub(super) fn merge_flag(
    transaction: &Transaction<'_>,
    id: MessageId,
    dimension: FlagDimension,
    before: bool,
    desired: bool,
    local_revision: u64,
    now_ms: i64,
) -> Result<(), DbFailure> {
    validate_timestamp(now_ms)?;
    let local_revision = checked_revision(local_revision)?;
    let identity = load_identity(transaction, id)?;
    require_committed_revision(&identity, local_revision)?;
    let existing = load_intent(transaction, &identity)?;

    if let Some(intent) = existing {
        reject_terminal_or_regressed(&intent, local_revision)?;
        ensure_version_capacity(intent.intent_version)?;

        let (base, final_desired) = match dimension {
            FlagDimension::Unread => (intent.unread_base.unwrap_or(before), desired),
            FlagDimension::Starred => (intent.starred_base.unwrap_or(before), desired),
        };
        let cancel = base == final_desired && intent.dispatched_mask & dimension.mask() == 0;
        let (unread_base, unread_desired, starred_base, starred_desired) = match dimension {
            FlagDimension::Unread => (
                (!cancel).then_some(base),
                (!cancel).then_some(final_desired),
                intent.starred_base,
                intent.starred_desired,
            ),
            FlagDimension::Starred => (
                intent.unread_base,
                intent.unread_desired,
                (!cancel).then_some(base),
                (!cancel).then_some(final_desired),
            ),
        };

        let has_pending_change =
            unread_base.is_some() || starred_base.is_some() || intent.placement_active;
        let missing_sources = union_source_count(transaction, intent.id, id)? == 0;
        let automatic_reconcile = intent.error_code.as_deref() == Some(MISSING_IMAP_LOCATOR);
        let request_reconcile = identity.legacy_reconcile
            || intent.reconcile_requested && !automatic_reconcile
            || has_pending_change
                && (automatic_reconcile || is_imap(&identity.provider) && missing_sources);
        let mark_missing_locator = request_reconcile
            && !identity.legacy_reconcile
            && !intent.reconcile_requested
            && intent.error_code.is_none()
            && is_imap(&identity.provider)
            && missing_sources;

        let empty = unread_base.is_none()
            && starred_base.is_none()
            && !intent.placement_active
            && !request_reconcile;
        if empty {
            if intent.state == "in_flight" {
                return Err(DbFailure::conflict(
                    "an in-flight remote intent has no dispatched desired state",
                ));
            }
            transaction
                .execute(
                    "DELETE FROM remote_change_intents WHERE id = ?1",
                    [intent.id],
                )
                .map_err(map_journal_error)?;
            return Ok(());
        }

        snapshot_sources(transaction, intent.id, id)?;
        transaction
            .execute(
                "UPDATE remote_change_intents
                 SET local_revision = ?2,
                     unread_base = ?3,
                     unread_desired = ?4,
                     starred_base = ?5,
                     starred_desired = ?6,
                     reconcile_requested = ?7,
                     state = CASE
                         WHEN ?7 = 1 AND state <> 'in_flight' THEN 'reconcile'
                         ELSE state
                     END,
                     error_class = CASE WHEN ?8 THEN 'conflict' ELSE error_class END,
                     error_code = CASE WHEN ?8 THEN ?9 ELSE error_code END,
                     error_detail = CASE WHEN ?8 THEN ?10 ELSE error_detail END,
                     intent_version = intent_version + 1,
                     not_before_ms = max(not_before_ms, ?11),
                     updated_at_ms = ?11
                 WHERE id = ?1",
                params![
                    intent.id,
                    local_revision,
                    unread_base,
                    unread_desired,
                    starred_base,
                    starred_desired,
                    request_reconcile,
                    mark_missing_locator,
                    MISSING_IMAP_LOCATOR,
                    MISSING_IMAP_LOCATOR_DETAIL,
                    now_ms,
                ],
            )
            .map_err(map_journal_error)?;
        ensure_payload_budget(transaction, intent.id)
    } else {
        ensure_parent_capacity(transaction, identity.account_id)?;
        let source_count = current_source_count(transaction, id)?;
        if source_count > MAX_IMAP_SOURCES {
            return Err(resource_limit("IMAP source locator limit exceeded"));
        }
        ensure_global_child_capacity(transaction, source_count)?;
        let missing_locator =
            !identity.legacy_reconcile && is_imap(&identity.provider) && source_count == 0;
        let request_reconcile = identity.legacy_reconcile || missing_locator;
        let (unread_base, unread_desired, starred_base, starred_desired) = match dimension {
            FlagDimension::Unread => (Some(before), Some(desired), None, None),
            FlagDimension::Starred => (None, None, Some(before), Some(desired)),
        };
        transaction
            .execute(
                "INSERT INTO remote_change_intents
                     (account_id, message_id, target_key, local_revision,
                      unread_base, unread_desired, starred_base, starred_desired,
                      reconcile_requested, state, error_class, error_code, error_detail,
                      not_before_ms,
                      created_at_ms, updated_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9,
                         CASE WHEN ?9 THEN 'reconcile' ELSE 'ready' END,
                         CASE WHEN ?10 THEN 'conflict' END,
                         CASE WHEN ?10 THEN ?11 END,
                         CASE WHEN ?10 THEN ?12 END,
                         ?13, ?13, ?13)",
                params![
                    identity.account_id,
                    id.get(),
                    identity.target_key,
                    local_revision,
                    unread_base,
                    unread_desired,
                    starred_base,
                    starred_desired,
                    request_reconcile,
                    missing_locator,
                    MISSING_IMAP_LOCATOR,
                    MISSING_IMAP_LOCATOR_DETAIL,
                    now_ms,
                ],
            )
            .map_err(map_journal_error)?;
        let intent_id = transaction.last_insert_rowid();
        snapshot_sources(transaction, intent_id, id)?;
        ensure_payload_budget(transaction, intent_id)
    }
}

pub(super) fn prepare_placement(
    transaction: &Transaction<'_>,
    id: MessageId,
    local_revision: u64,
    now_ms: i64,
    eligible_at_ms: i64,
) -> Result<PlacementDraft, DbFailure> {
    validate_timestamp(now_ms)?;
    validate_timestamp(eligible_at_ms)?;
    let local_revision = checked_revision(local_revision)?;
    let identity = load_identity(transaction, id)?;
    if local_revision < identity.revision {
        return Err(DbFailure::conflict(
            "message revision moved backwards while preparing remote placement",
        ));
    }
    let existing = load_intent(transaction, &identity)?;
    let (intent_id, was_existing, placement_active) = if let Some(intent) = existing.as_ref() {
        reject_terminal_or_regressed(intent, local_revision)?;
        ensure_version_capacity(intent.intent_version)?;
        (intent.id, true, intent.placement_active)
    } else {
        ensure_parent_capacity(transaction, identity.account_id)?;
        transaction
            .execute(
                "INSERT INTO remote_change_intents
                     (account_id, message_id, target_key, local_revision,
                      placement_active, not_before_ms, created_at_ms, updated_at_ms)
                 VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6, ?6)",
                params![
                    identity.account_id,
                    id.get(),
                    identity.target_key,
                    local_revision,
                    eligible_at_ms,
                    now_ms,
                ],
            )
            .map_err(map_journal_error)?;
        (transaction.last_insert_rowid(), false, false)
    };

    if !placement_active {
        let base_count = membership_count(transaction, id)?;
        if base_count > MAX_FOLDER_KEYS_PER_SIDE {
            return Err(resource_limit("remote placement folder limit exceeded"));
        }
        ensure_global_child_capacity(transaction, base_count)?;
        if was_existing {
            transaction
                .execute(
                    "UPDATE remote_change_intents SET placement_active = 1 WHERE id = ?1",
                    [intent_id],
                )
                .map_err(map_journal_error)?;
        }
        let inserted = transaction
            .execute(
                "INSERT INTO remote_change_intent_folders (intent_id, side, folder_key)
                 SELECT ?1, 'base', f.remote_key
                 FROM message_folders AS mf
                 JOIN folders AS f ON f.id = mf.folder_id AND f.account_id = mf.account_id
                 WHERE mf.message_id = ?2",
                params![intent_id, id.get()],
            )
            .map_err(map_journal_error)?;
        require_insert_count(inserted, base_count, "base folder snapshot")?;
    }

    snapshot_sources(transaction, intent_id, id)?;
    let source_count = stored_source_count(transaction, intent_id)?;
    let missing_locator = is_imap(&identity.provider) && source_count == 0;
    let request_reconcile = existing
        .as_ref()
        .is_some_and(|intent| intent.reconcile_requested)
        || identity.legacy_reconcile
        || missing_locator;
    let mark_missing_locator = missing_locator
        && !identity.legacy_reconcile
        && existing
            .as_ref()
            .is_none_or(|intent| !intent.reconcile_requested && intent.error_code.is_none());
    if request_reconcile {
        transaction
            .execute(
                "UPDATE remote_change_intents
                 SET reconcile_requested = 1,
                     state = CASE
                         WHEN state = 'in_flight' THEN state ELSE 'reconcile'
                     END,
                     error_class = CASE WHEN ?2 THEN 'conflict' ELSE error_class END,
                     error_code = CASE WHEN ?2 THEN ?3 ELSE error_code END,
                     error_detail = CASE WHEN ?2 THEN ?4 ELSE error_detail END
                 WHERE id = ?1",
                params![
                    intent_id,
                    mark_missing_locator,
                    MISSING_IMAP_LOCATOR,
                    MISSING_IMAP_LOCATOR_DETAIL,
                ],
            )
            .map_err(map_journal_error)?;
    }

    Ok(PlacementDraft {
        intent_id,
        message_id: id,
        local_revision,
        now_ms,
        eligible_at_ms,
        existing: was_existing,
    })
}

pub(super) fn finish_placement(
    transaction: &Transaction<'_>,
    draft: PlacementDraft,
    id: MessageId,
) -> Result<(), DbFailure> {
    if id != draft.message_id {
        return Err(DbFailure::conflict(
            "remote placement draft belongs to another message",
        ));
    }
    let identity = load_identity(transaction, id)?;
    require_committed_revision(&identity, draft.local_revision)?;
    let desired_count = membership_count(transaction, id)?;
    if desired_count > MAX_FOLDER_KEYS_PER_SIDE {
        return Err(resource_limit("remote placement folder limit exceeded"));
    }

    let (
        current_matches_base,
        dispatched_mask,
        state,
        has_other_change,
        reconcile_requested,
        error_code,
    ): (bool, i64, String, bool, bool, Option<String>) = transaction
        .query_row(
            "SELECT
                 NOT EXISTS (
                     SELECT f.remote_key
                     FROM message_folders AS mf
                     JOIN folders AS f
                       ON f.id = mf.folder_id AND f.account_id = mf.account_id
                     WHERE mf.message_id = ?2
                     EXCEPT
                     SELECT folder_key
                     FROM remote_change_intent_folders
                     WHERE intent_id = ?1 AND side = 'base'
                 ) AND NOT EXISTS (
                     SELECT folder_key
                     FROM remote_change_intent_folders
                     WHERE intent_id = ?1 AND side = 'base'
                     EXCEPT
                     SELECT f.remote_key
                     FROM message_folders AS mf
                     JOIN folders AS f
                       ON f.id = mf.folder_id AND f.account_id = mf.account_id
                     WHERE mf.message_id = ?2
                 ),
                 dispatched_mask,
                 state,
                 unread_base IS NOT NULL
                     OR starred_base IS NOT NULL
                     OR delete_requested = 1,
                 reconcile_requested,
                 error_code
             FROM remote_change_intents WHERE id = ?1",
            params![draft.intent_id, id.get()],
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
        .map_err(DbFailure::database)?;

    if current_matches_base && dispatched_mask & PLACEMENT_MASK == 0 {
        transaction
            .execute(
                "DELETE FROM remote_change_intent_folders WHERE intent_id = ?1",
                [draft.intent_id],
            )
            .map_err(map_journal_error)?;
        let automatic_reconcile = error_code.as_deref() == Some(MISSING_IMAP_LOCATOR);
        let retain_reconcile = identity.legacy_reconcile
            || reconcile_requested && (!automatic_reconcile || has_other_change);
        if !has_other_change && !retain_reconcile {
            if state == "in_flight" {
                return Err(DbFailure::conflict(
                    "an in-flight remote intent lost its desired state",
                ));
            }
            transaction
                .execute(
                    "DELETE FROM remote_change_intents WHERE id = ?1",
                    [draft.intent_id],
                )
                .map_err(map_journal_error)?;
            return Ok(());
        }
        transaction
            .execute(
                "UPDATE remote_change_intents SET placement_active = 0 WHERE id = ?1",
                [draft.intent_id],
            )
            .map_err(map_journal_error)?;
    } else {
        transaction
            .execute(
                "DELETE FROM remote_change_intent_folders
                 WHERE intent_id = ?1 AND side = 'desired'",
                [draft.intent_id],
            )
            .map_err(map_journal_error)?;
        ensure_global_child_capacity(transaction, desired_count)?;
        let inserted = transaction
            .execute(
                "INSERT INTO remote_change_intent_folders (intent_id, side, folder_key)
                 SELECT ?1, 'desired', f.remote_key
                 FROM message_folders AS mf
                 JOIN folders AS f ON f.id = mf.folder_id AND f.account_id = mf.account_id
                 WHERE mf.message_id = ?2",
                params![draft.intent_id, id.get()],
            )
            .map_err(map_journal_error)?;
        require_insert_count(inserted, desired_count, "desired folder snapshot")?;
    }

    transaction
        .execute(
            "UPDATE remote_change_intents
             SET local_revision = ?2,
                 intent_version = intent_version + ?3,
                 not_before_ms = max(not_before_ms, ?4),
                 updated_at_ms = ?5
             WHERE id = ?1",
            params![
                draft.intent_id,
                draft.local_revision,
                i64::from(draft.existing),
                draft.eligible_at_ms,
                draft.now_ms,
            ],
        )
        .map_err(map_journal_error)?;
    ensure_payload_budget(transaction, draft.intent_id)
}

pub(super) fn merge_terminal_delete(
    transaction: &Transaction<'_>,
    id: MessageId,
    local_revision: u64,
    account_id: i64,
    target_key: &str,
    now_ms: i64,
) -> Result<(), DbFailure> {
    validate_timestamp(now_ms)?;
    let local_revision = checked_revision(local_revision)?;
    let identity = load_identity(transaction, id)?;
    require_committed_revision(&identity, local_revision)?;
    if identity.account_id != account_id || identity.target_key != target_key {
        return Err(DbFailure::conflict(
            "permanent deletion remote identity no longer matches the message",
        ));
    }

    let existing = load_intent(transaction, &identity)?;
    let intent_id = if let Some(intent) = existing.as_ref() {
        if intent.delete_requested {
            return Err(DbFailure::conflict(
                "message already has a terminal remote intent",
            ));
        }
        if local_revision < intent.local_revision {
            return Err(DbFailure::conflict(
                "remote intent revision moved backwards",
            ));
        }
        ensure_version_capacity(intent.intent_version)?;
        transaction
            .execute(
                "DELETE FROM remote_change_intent_folders WHERE intent_id = ?1",
                [intent.id],
            )
            .map_err(map_journal_error)?;
        intent.id
    } else {
        ensure_parent_capacity(transaction, account_id)?;
        let current_sources = current_source_count(transaction, id)?;
        let reconcile = is_imap(&identity.provider) && current_sources == 0;
        transaction
            .execute(
                "INSERT INTO remote_change_intents
                     (account_id, message_id, target_key, local_revision,
                      delete_requested, state, not_before_ms,
                      created_at_ms, updated_at_ms)
                 VALUES (?1, ?2, ?3, ?4, 1,
                         CASE WHEN ?5 THEN 'reconcile' ELSE 'ready' END,
                         ?6, ?6, ?6)",
                params![
                    account_id,
                    id.get(),
                    target_key,
                    local_revision,
                    reconcile,
                    now_ms
                ],
            )
            .map_err(map_journal_error)?;
        transaction.last_insert_rowid()
    };

    snapshot_sources(transaction, intent_id, id)?;
    let source_count = stored_source_count(transaction, intent_id)?;
    transaction
        .execute(
            "INSERT INTO message_tombstones (account_id, remote_key, deleted_at_ms)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(account_id, remote_key) DO UPDATE
             SET deleted_at_ms = excluded.deleted_at_ms",
            params![account_id, target_key, now_ms],
        )
        .map_err(map_journal_error)?;
    snapshot_tombstone_sources(transaction, intent_id, account_id, target_key)?;

    if existing.is_some() {
        let reconcile = is_imap(&identity.provider) && source_count == 0;
        transaction
            .execute(
                "UPDATE remote_change_intents
                 SET local_revision = ?2,
                     unread_base = NULL,
                     unread_desired = NULL,
                     starred_base = NULL,
                     starred_desired = NULL,
                     placement_active = 0,
                     reconcile_requested = 0,
                     delete_requested = 1,
                     state = CASE
                         WHEN state = 'in_flight' THEN state
                         WHEN ?3 THEN 'reconcile'
                         ELSE state
                     END,
                     intent_version = intent_version + 1,
                     not_before_ms = CASE
                         WHEN state = 'in_flight' THEN not_before_ms ELSE ?4
                     END,
                     updated_at_ms = ?4
                 WHERE id = ?1",
                params![intent_id, local_revision, reconcile, now_ms],
            )
            .map_err(map_journal_error)?;
    }
    ensure_payload_budget(transaction, intent_id)
}

fn load_identity(
    transaction: &Transaction<'_>,
    id: MessageId,
) -> Result<MessageIdentity, DbFailure> {
    transaction
        .query_row(
            "SELECT m.account_id, m.remote_key, a.provider, m.revision,
                    m.legacy_reconcile_revision IS NOT NULL
             FROM messages AS m
             JOIN accounts AS a ON a.id = m.account_id
             WHERE m.id = ?1",
            [id.get()],
            |row| {
                Ok(MessageIdentity {
                    account_id: row.get(0)?,
                    target_key: row.get(1)?,
                    provider: row.get(2)?,
                    revision: row.get(3)?,
                    legacy_reconcile: row.get(4)?,
                })
            },
        )
        .optional()
        .map_err(DbFailure::database)?
        .ok_or_else(|| DbFailure::not_found("message no longer exists"))
}

fn load_intent(
    transaction: &Transaction<'_>,
    identity: &MessageIdentity,
) -> Result<Option<IntentRow>, DbFailure> {
    transaction
        .query_row(
            "SELECT id, intent_version, local_revision,
                    unread_base, unread_desired, starred_base, starred_desired,
                    placement_active, reconcile_requested, delete_requested,
                    dispatched_mask, state, error_code
             FROM remote_change_intents
             WHERE account_id = ?1 AND target_key = ?2",
            params![identity.account_id, identity.target_key],
            |row| {
                Ok(IntentRow {
                    id: row.get(0)?,
                    intent_version: row.get(1)?,
                    local_revision: row.get(2)?,
                    unread_base: row.get(3)?,
                    unread_desired: row.get(4)?,
                    starred_base: row.get(5)?,
                    starred_desired: row.get(6)?,
                    placement_active: row.get(7)?,
                    reconcile_requested: row.get(8)?,
                    delete_requested: row.get(9)?,
                    dispatched_mask: row.get(10)?,
                    state: row.get(11)?,
                    error_code: row.get(12)?,
                })
            },
        )
        .optional()
        .map_err(DbFailure::database)
}

fn snapshot_sources(
    transaction: &Transaction<'_>,
    intent_id: i64,
    id: MessageId,
) -> Result<(), DbFailure> {
    let stored = stored_source_count(transaction, intent_id)?;
    let additions = transaction
        .query_row(
            "SELECT count(*) FROM (
                 SELECT 1
                 FROM imap_message_locations AS l
                 JOIN folders AS f
                   ON f.id = l.folder_id AND f.account_id = l.account_id
                 WHERE l.message_id = ?2
                   AND NOT EXISTS (
                       SELECT 1 FROM remote_change_intent_imap_sources AS source
                       WHERE source.intent_id = ?1
                         AND source.folder_key = f.remote_key
                         AND source.uid_validity = l.uid_validity
                         AND source.uid = l.uid
                   )
                 LIMIT 257
             )",
            params![intent_id, id.get()],
            |row| row.get::<_, i64>(0),
        )
        .map_err(DbFailure::database)?;
    if stored + additions > MAX_IMAP_SOURCES {
        return Err(resource_limit("IMAP source locator limit exceeded"));
    }
    ensure_global_child_capacity(transaction, additions)?;
    let inserted = transaction
        .execute(
            "INSERT OR IGNORE INTO remote_change_intent_imap_sources
                 (intent_id, folder_key, mailbox_object_id, uid_validity, uid,
                  modseq, email_id, remote_seen, remote_flagged)
             SELECT ?1, f.remote_key, s.mailbox_object_id,
                    l.uid_validity, l.uid, l.modseq, l.email_id,
                    l.remote_seen, l.remote_flagged
             FROM imap_message_locations AS l
             JOIN folders AS f ON f.id = l.folder_id AND f.account_id = l.account_id
             LEFT JOIN sync_state AS s ON s.folder_id = l.folder_id
             WHERE l.message_id = ?2",
            params![intent_id, id.get()],
        )
        .map_err(map_journal_error)?;
    require_insert_count(inserted, additions, "IMAP source snapshot")
}

fn snapshot_tombstone_sources(
    transaction: &Transaction<'_>,
    intent_id: i64,
    account_id: i64,
    target_key: &str,
) -> Result<(), DbFailure> {
    let stored: i64 = transaction
        .query_row(
            "SELECT count(*) FROM message_tombstone_imap_locations
             WHERE account_id = ?1 AND target_key = ?2",
            params![account_id, target_key],
            |row| row.get(0),
        )
        .map_err(DbFailure::database)?;
    let additions: i64 = transaction
        .query_row(
            "SELECT count(*) FROM (
                 SELECT 1 FROM remote_change_intent_imap_sources AS source
                 WHERE source.intent_id = ?1
                   AND NOT EXISTS (
                       SELECT 1 FROM message_tombstone_imap_locations AS tombstone
                       WHERE tombstone.account_id = ?2
                         AND tombstone.target_key = ?3
                         AND tombstone.folder_key = source.folder_key
                         AND tombstone.uid_validity = source.uid_validity
                         AND tombstone.uid = source.uid
                   )
                 LIMIT 257
             )",
            params![intent_id, account_id, target_key],
            |row| row.get(0),
        )
        .map_err(DbFailure::database)?;
    if stored + additions > MAX_IMAP_SOURCES {
        return Err(resource_limit("tombstone locator limit exceeded"));
    }
    ensure_global_child_capacity(transaction, additions)?;
    let inserted = transaction
        .execute(
            "INSERT OR IGNORE INTO message_tombstone_imap_locations
                 (account_id, target_key, folder_key, mailbox_object_id,
                  uid_validity, uid, email_id)
             SELECT ?2, ?3, folder_key, mailbox_object_id,
                    uid_validity, uid, email_id
             FROM remote_change_intent_imap_sources
             WHERE intent_id = ?1",
            params![intent_id, account_id, target_key],
        )
        .map_err(map_journal_error)?;
    require_insert_count(inserted, additions, "tombstone locator snapshot")
}

fn ensure_parent_capacity(transaction: &Transaction<'_>, account_id: i64) -> Result<(), DbFailure> {
    let (account_full, global_full): (bool, bool) = transaction
        .query_row(
            "SELECT
                 EXISTS (
                     SELECT 1 FROM remote_change_intents
                     WHERE account_id = ?1 LIMIT 1 OFFSET ?2
                 ),
                 EXISTS (
                     SELECT 1 FROM remote_change_intents LIMIT 1 OFFSET ?3
                 )",
            params![
                account_id,
                MAX_INTENTS_PER_ACCOUNT - 1,
                MAX_INTENTS_GLOBAL - 1,
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(DbFailure::database)?;
    if account_full || global_full {
        Err(resource_limit("remote intent limit exceeded"))
    } else {
        Ok(())
    }
}

fn ensure_global_child_capacity(
    transaction: &Transaction<'_>,
    additions: i64,
) -> Result<(), DbFailure> {
    let used: i64 = transaction
        .query_row(
            "SELECT child_count FROM remote_journal_usage WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .map_err(DbFailure::database)?;
    if additions < 0
        || used
            .checked_add(additions)
            .is_none_or(|total| total > MAX_JOURNAL_CHILDREN)
    {
        Err(resource_limit("remote journal child limit exceeded"))
    } else {
        Ok(())
    }
}

pub(super) fn ensure_payload_budget(
    transaction: &Transaction<'_>,
    intent_id: i64,
) -> Result<(), DbFailure> {
    let bytes: i64 = transaction
        .query_row(
            "SELECT
                 256 + length(CAST(i.target_key AS BLOB))
                 + coalesce((
                     SELECT sum(32 + length(CAST(folder_key AS BLOB)))
                     FROM remote_change_intent_folders WHERE intent_id = ?1
                   ), 0)
                 + coalesce((
                     SELECT sum(
                         128
                         + length(CAST(folder_key AS BLOB))
                         + coalesce(length(CAST(mailbox_object_id AS BLOB)), 0)
                         + coalesce(length(CAST(email_id AS BLOB)), 0)
                     )
                     FROM remote_change_intent_imap_sources WHERE intent_id = ?1
                   ), 0)
                 + CASE WHEN a.provider COLLATE NOCASE = 'jmap' THEN 512 ELSE 0 END
             FROM remote_change_intents AS i
             JOIN accounts AS a ON a.id = i.account_id
             WHERE i.id = ?1",
            [intent_id],
            |row| row.get(0),
        )
        .map_err(DbFailure::database)?;
    if bytes > MAX_PROVIDER_PAYLOAD_BYTES {
        Err(resource_limit("remote provider payload limit exceeded"))
    } else {
        Ok(())
    }
}

fn membership_count(transaction: &Transaction<'_>, id: MessageId) -> Result<i64, DbFailure> {
    transaction
        .query_row(
            "SELECT count(*) FROM (
                 SELECT 1 FROM message_folders WHERE message_id = ?1 LIMIT 257
             )",
            [id.get()],
            |row| row.get(0),
        )
        .map_err(DbFailure::database)
}

fn current_source_count(transaction: &Transaction<'_>, id: MessageId) -> Result<i64, DbFailure> {
    transaction
        .query_row(
            "SELECT count(*) FROM (
                 SELECT 1 FROM imap_message_locations WHERE message_id = ?1 LIMIT 257
             )",
            [id.get()],
            |row| row.get(0),
        )
        .map_err(DbFailure::database)
}

fn stored_source_count(transaction: &Transaction<'_>, intent_id: i64) -> Result<i64, DbFailure> {
    transaction
        .query_row(
            "SELECT count(*) FROM remote_change_intent_imap_sources WHERE intent_id = ?1",
            [intent_id],
            |row| row.get(0),
        )
        .map_err(DbFailure::database)
}

fn union_source_count(
    transaction: &Transaction<'_>,
    intent_id: i64,
    id: MessageId,
) -> Result<i64, DbFailure> {
    let stored = stored_source_count(transaction, intent_id)?;
    let additions: i64 = transaction
        .query_row(
            "SELECT count(*) FROM (
                 SELECT 1
                 FROM imap_message_locations AS l
                 JOIN folders AS f ON f.id = l.folder_id AND f.account_id = l.account_id
                 WHERE l.message_id = ?2
                   AND NOT EXISTS (
                       SELECT 1 FROM remote_change_intent_imap_sources AS source
                       WHERE source.intent_id = ?1
                         AND source.folder_key = f.remote_key
                         AND source.uid_validity = l.uid_validity
                         AND source.uid = l.uid
                   )
                 LIMIT 257
             )",
            params![intent_id, id.get()],
            |row| row.get(0),
        )
        .map_err(DbFailure::database)?;
    let total = stored.saturating_add(additions);
    if total > MAX_IMAP_SOURCES {
        Err(resource_limit("IMAP source locator limit exceeded"))
    } else {
        Ok(total)
    }
}

fn reject_terminal_or_regressed(intent: &IntentRow, local_revision: i64) -> Result<(), DbFailure> {
    if intent.delete_requested {
        Err(DbFailure::conflict(
            "message already has a terminal remote intent",
        ))
    } else if local_revision < intent.local_revision {
        Err(DbFailure::conflict(
            "remote intent revision moved backwards",
        ))
    } else {
        Ok(())
    }
}

fn ensure_version_capacity(intent_version: i64) -> Result<(), DbFailure> {
    if intent_version == i64::MAX {
        Err(resource_limit("remote intent version exhausted"))
    } else {
        Ok(())
    }
}

fn checked_revision(revision: u64) -> Result<i64, DbFailure> {
    i64::try_from(revision).map_err(|_| resource_limit("message revision exceeds SQLite bounds"))
}

fn require_committed_revision(identity: &MessageIdentity, revision: i64) -> Result<(), DbFailure> {
    if identity.revision == revision {
        Ok(())
    } else {
        Err(DbFailure::conflict(
            "message revision changed while merging its remote intent",
        ))
    }
}

fn require_insert_count(inserted: usize, expected: i64, snapshot: &str) -> Result<(), DbFailure> {
    if i64::try_from(inserted).ok() == Some(expected) {
        Ok(())
    } else {
        Err(DbFailure::conflict(format!(
            "{snapshot} changed while it was being frozen"
        )))
    }
}

fn validate_timestamp(timestamp_ms: i64) -> Result<(), DbFailure> {
    if (MIN_TIMESTAMP_MS..=MAX_TIMESTAMP_MS).contains(&timestamp_ms) {
        Ok(())
    } else {
        Err(resource_limit("timestamp is outside SQLite bounds"))
    }
}

fn is_imap(provider: &str) -> bool {
    provider.eq_ignore_ascii_case("imap")
}

fn resource_limit(message: impl Into<Box<str>>) -> DbFailure {
    DbFailure::resource_limit(message)
}

fn map_journal_error(error: SqliteError) -> DbFailure {
    let message = error.to_string();
    if [
        "remote intent limit exceeded",
        "remote intent folder limit exceeded",
        "remote intent source limit exceeded",
        "tombstone location limit exceeded",
        "remote journal child limit exceeded",
    ]
    .iter()
    .any(|needle| message.contains(needle))
    {
        resource_limit(message)
    } else {
        DbFailure::database(message)
    }
}
