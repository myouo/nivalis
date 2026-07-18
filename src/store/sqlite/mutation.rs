use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use super::domain::{
    DbFailure, MessageId, MessageMutation, MessageState, MutationOutcome, TRASH_UNDO_TTL_MS,
    UndoReceipt, UndoToken,
};
use super::stats::{apply_transition, load_message_snapshot};

const MIN_TIMESTAMP_MS: i64 = -62_135_596_800_000;
const MAX_TIMESTAMP_MS: i64 = 253_402_300_799_999;
const MAX_UNDO_TOKEN: i64 = i64::MAX;
const MAX_UNDO_FOLDERS: i64 = 256;

pub(super) fn mutate_message(
    connection: &mut Connection,
    mutation: MessageMutation,
    now_ms: i64,
) -> Result<MutationOutcome, DbFailure> {
    match mutation {
        MessageMutation::SetUnread { id, unread } => set_unread(connection, id, unread),
        MessageMutation::SetStarred { id, starred } => set_starred(connection, id, starred),
        MessageMutation::Archive { id } => archive(connection, id),
        MessageMutation::MoveToTrash { id } => move_to_trash(connection, id, now_ms),
        MessageMutation::DeletePermanently { id } => delete_permanently(connection, id, now_ms),
        MessageMutation::UndoTrash { token } => undo_trash(connection, token, now_ms),
    }
}

fn set_unread(
    connection: &mut Connection,
    id: MessageId,
    unread: bool,
) -> Result<MutationOutcome, DbFailure> {
    let transaction = immediate_transaction(connection)?;
    let before = load_message_snapshot(&transaction, id)?;
    let changed = transaction
        .execute(
            "UPDATE messages
             SET unread = ?2, revision = revision + 1
             WHERE id = ?1 AND unread <> ?2",
            params![id.get(), unread],
        )
        .map_err(DbFailure::database)?
        != 0;
    let state = load_state(&transaction, id)?;
    let after = if changed {
        load_message_snapshot(&transaction, id)?
    } else {
        before
    };
    let stats_delta = apply_transition(&transaction, before, Some(after))?;
    transaction.commit().map_err(DbFailure::database)?;
    Ok(MutationOutcome::Updated {
        state,
        changed,
        stats_delta,
    })
}

fn set_starred(
    connection: &mut Connection,
    id: MessageId,
    starred: bool,
) -> Result<MutationOutcome, DbFailure> {
    let transaction = immediate_transaction(connection)?;
    let before = load_message_snapshot(&transaction, id)?;
    let changed = transaction
        .execute(
            "UPDATE messages
             SET starred = ?2, revision = revision + 1
             WHERE id = ?1 AND starred <> ?2",
            params![id.get(), starred],
        )
        .map_err(DbFailure::database)?
        != 0;
    let state = load_state(&transaction, id)?;
    let after = if changed {
        load_message_snapshot(&transaction, id)?
    } else {
        before
    };
    let stats_delta = apply_transition(&transaction, before, Some(after))?;
    transaction.commit().map_err(DbFailure::database)?;
    Ok(MutationOutcome::Updated {
        state,
        changed,
        stats_delta,
    })
}

fn archive(connection: &mut Connection, id: MessageId) -> Result<MutationOutcome, DbFailure> {
    let transaction = immediate_transaction(connection)?;
    let state = load_state(&transaction, id)?;
    let before = load_message_snapshot(&transaction, id)?;
    let archive_id = canonical_role_folder(&transaction, state.account_id, "archive")?;
    let blocked_placement_count: i64 = transaction
        .query_row(
            "SELECT count(*)
             FROM message_folders AS mf
             JOIN folders AS f ON f.id = mf.folder_id
             WHERE mf.message_id = ?1 AND f.role IN ('trash', 'sent', 'drafts')",
            [id.get()],
            |row| row.get(0),
        )
        .map_err(DbFailure::database)?;
    if blocked_placement_count != 0 {
        return Err(DbFailure::conflict(
            "messages in Trash, Sent, or Drafts cannot be archived",
        ));
    }

    let changed: bool = transaction
        .query_row(
            "SELECT
                 EXISTS (
                     SELECT 1
                     FROM message_folders AS mf
                     JOIN folders AS f ON f.id = mf.folder_id
                     WHERE mf.message_id = ?1
                       AND (f.role = 'inbox' OR (f.role = 'archive' AND f.id <> ?2))
                 )
                 OR NOT EXISTS (
                     SELECT 1 FROM message_folders
                     WHERE message_id = ?1 AND folder_id = ?2
                 )",
            params![id.get(), archive_id],
            |row| row.get(0),
        )
        .map_err(DbFailure::database)?;

    if changed {
        transaction
            .execute(
                "DELETE FROM message_folders
                 WHERE message_id = ?1
                   AND folder_id IN (
                       SELECT id FROM folders
                       WHERE account_id = ?2 AND role IN ('inbox', 'archive')
                   )",
                params![id.get(), state.account_id],
            )
            .map_err(DbFailure::database)?;
        transaction
            .execute(
                "INSERT INTO message_folders (message_id, folder_id, account_id)
                 VALUES (?1, ?2, ?3)",
                params![id.get(), archive_id, state.account_id],
            )
            .map_err(DbFailure::database)?;
        increment_revision(&transaction, id)?;
    }

    let state = load_state(&transaction, id)?;
    let after = if changed {
        load_message_snapshot(&transaction, id)?
    } else {
        before
    };
    let stats_delta = apply_transition(&transaction, before, Some(after))?;
    transaction.commit().map_err(DbFailure::database)?;
    Ok(MutationOutcome::Archived {
        state,
        changed,
        stats_delta,
    })
}

fn move_to_trash(
    connection: &mut Connection,
    id: MessageId,
    now_ms: i64,
) -> Result<MutationOutcome, DbFailure> {
    let expires_at_ms = checked_expiry(now_ms)?;
    let transaction = immediate_transaction(connection)?;
    let state = load_state(&transaction, id)?;
    let before = load_message_snapshot(&transaction, id)?;
    let (folder_count, trash_count) = membership_counts(&transaction, id)?;
    if folder_count == 0 {
        return Err(DbFailure::conflict(
            "message has no folder placement to restore",
        ));
    }
    if trash_count != 0 {
        return Err(DbFailure::conflict("message is already in Trash"));
    }
    if folder_count > MAX_UNDO_FOLDERS {
        return Err(DbFailure::resource_limit(format!(
            "message belongs to more than {MAX_UNDO_FOLDERS} folders"
        )));
    }
    let trash_id = canonical_role_folder(&transaction, state.account_id, "trash")?;

    clear_undo(&transaction)?;
    transaction
        .execute(
            "UPDATE trash_undo
             SET token = CASE WHEN token = ?1 THEN 1 ELSE token + 1 END,
                 message_id = ?2,
                 account_id = ?3,
                 expires_at_ms = ?4,
                 folder_count = ?5
             WHERE slot = 1",
            params![
                MAX_UNDO_TOKEN,
                id.get(),
                state.account_id,
                expires_at_ms,
                folder_count
            ],
        )
        .map_err(DbFailure::database)?;
    let raw_token: i64 = transaction
        .query_row("SELECT token FROM trash_undo WHERE slot = 1", [], |row| {
            row.get(0)
        })
        .map_err(DbFailure::database)?;
    let token = UndoToken::from_database(raw_token)?;
    let copied = transaction
        .execute(
            "INSERT INTO trash_undo_folders (slot, folder_id, account_id)
             SELECT 1, folder_id, account_id
             FROM message_folders
             WHERE message_id = ?1",
            [id.get()],
        )
        .map_err(DbFailure::database)?;
    if i64::try_from(copied).ok() != Some(folder_count) {
        return Err(DbFailure::conflict(
            "folder placement changed while creating undo snapshot",
        ));
    }

    transaction
        .execute(
            "DELETE FROM message_folders WHERE message_id = ?1",
            [id.get()],
        )
        .map_err(DbFailure::database)?;
    transaction
        .execute(
            "INSERT INTO message_folders (message_id, folder_id, account_id)
             VALUES (?1, ?2, ?3)",
            params![id.get(), trash_id, state.account_id],
        )
        .map_err(DbFailure::database)?;
    increment_revision(&transaction, id)?;
    let state = load_state(&transaction, id)?;
    let after = load_message_snapshot(&transaction, id)?;
    let stats_delta = apply_transition(&transaction, before, Some(after))?;
    transaction.commit().map_err(DbFailure::database)?;

    Ok(MutationOutcome::MovedToTrash {
        state,
        undo: UndoReceipt {
            token,
            expires_at_ms,
        },
        stats_delta,
    })
}

fn delete_permanently(
    connection: &mut Connection,
    id: MessageId,
    now_ms: i64,
) -> Result<MutationOutcome, DbFailure> {
    validate_timestamp(now_ms)?;
    let transaction = immediate_transaction(connection)?;
    let state = load_state(&transaction, id)?;
    let before = load_message_snapshot(&transaction, id)?;
    let (_, trash_count) = membership_counts(&transaction, id)?;
    if trash_count == 0 {
        return Err(DbFailure::conflict(
            "permanent deletion is only allowed from Trash",
        ));
    }
    let remote_key: String = transaction
        .query_row(
            "SELECT remote_key FROM messages WHERE id = ?1",
            [id.get()],
            |row| row.get(0),
        )
        .map_err(DbFailure::database)?;

    queue_message_files(&transaction, id, now_ms)?;
    transaction
        .execute(
            "INSERT INTO message_tombstones (account_id, remote_key, deleted_at_ms)
             VALUES (?1, ?2, ?3)
             ON CONFLICT (account_id, remote_key) DO UPDATE
             SET deleted_at_ms = excluded.deleted_at_ms",
            params![state.account_id, remote_key, now_ms],
        )
        .map_err(DbFailure::database)?;
    transaction
        .execute("DELETE FROM messages WHERE id = ?1", [id.get()])
        .map_err(DbFailure::database)?;
    let stats_delta = apply_transition(&transaction, before, None)?;
    transaction.commit().map_err(DbFailure::database)?;
    Ok(MutationOutcome::PermanentlyDeleted {
        id,
        account_id: state.account_id,
        stats_delta,
    })
}

fn undo_trash(
    connection: &mut Connection,
    token: UndoToken,
    now_ms: i64,
) -> Result<MutationOutcome, DbFailure> {
    validate_timestamp(now_ms)?;
    let transaction = immediate_transaction(connection)?;
    let snapshot = load_undo(&transaction)?;
    let Some(snapshot) = snapshot else {
        return Err(DbFailure::conflict("there is no Trash action to undo"));
    };
    if snapshot.token != token {
        return Err(DbFailure::conflict(
            "this Trash action has been replaced by a newer action",
        ));
    }
    if now_ms > snapshot.expires_at_ms {
        clear_undo(&transaction)?;
        transaction.commit().map_err(DbFailure::database)?;
        return Err(DbFailure::conflict("the Trash undo period has expired"));
    }

    let state = load_state(&transaction, snapshot.message_id)?;
    if state.account_id != snapshot.account_id {
        return Err(DbFailure::conflict(
            "the message account no longer matches its undo snapshot",
        ));
    }
    let (folder_count, trash_count) = membership_counts(&transaction, snapshot.message_id)?;
    if folder_count != 1 || trash_count != 1 {
        return Err(DbFailure::conflict(
            "the message is no longer in the expected Trash placement",
        ));
    }
    let before = load_message_snapshot(&transaction, snapshot.message_id)?;

    let (snapshot_count, valid_count, trash_snapshot_count): (i64, i64, i64) = transaction
        .query_row(
            "SELECT
                 count(*),
                 coalesce(sum(CASE
                     WHEN f.id IS NOT NULL AND f.account_id = ?1 AND uf.account_id = ?1
                     THEN 1 ELSE 0 END), 0),
                 coalesce(sum(CASE WHEN f.role = 'trash' THEN 1 ELSE 0 END), 0)
             FROM trash_undo_folders AS uf
             LEFT JOIN folders AS f ON f.id = uf.folder_id
             WHERE uf.slot = 1",
            [snapshot.account_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(DbFailure::database)?;
    if snapshot_count != snapshot.folder_count
        || valid_count != snapshot.folder_count
        || trash_snapshot_count != 0
    {
        return Err(DbFailure::conflict(
            "the original folder placement is no longer available",
        ));
    }

    transaction
        .execute(
            "DELETE FROM message_folders WHERE message_id = ?1",
            [snapshot.message_id.get()],
        )
        .map_err(DbFailure::database)?;
    let restored = transaction
        .execute(
            "INSERT INTO message_folders (message_id, folder_id, account_id)
             SELECT ?1, folder_id, account_id
             FROM trash_undo_folders
             WHERE slot = 1",
            [snapshot.message_id.get()],
        )
        .map_err(DbFailure::database)?;
    if i64::try_from(restored).ok() != Some(snapshot.folder_count) {
        return Err(DbFailure::conflict(
            "could not restore the complete folder placement",
        ));
    }
    increment_revision(&transaction, snapshot.message_id)?;
    clear_undo(&transaction)?;
    let state = load_state(&transaction, snapshot.message_id)?;
    let after = load_message_snapshot(&transaction, snapshot.message_id)?;
    let stats_delta = apply_transition(&transaction, before, Some(after))?;
    transaction.commit().map_err(DbFailure::database)?;
    Ok(MutationOutcome::Restored { state, stats_delta })
}

fn immediate_transaction(connection: &mut Connection) -> Result<Transaction<'_>, DbFailure> {
    connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(DbFailure::database)
}

fn load_state(transaction: &Transaction<'_>, id: MessageId) -> Result<MessageState, DbFailure> {
    let row: Option<(i64, i64, i64, bool, bool)> = transaction
        .query_row(
            "SELECT id, account_id, revision, unread, starred
             FROM messages WHERE id = ?1",
            [id.get()],
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
    let Some((raw_id, account_id, revision, unread, starred)) = row else {
        return Err(DbFailure::not_found("message no longer exists"));
    };
    let revision = u64::try_from(revision)
        .map_err(|_| DbFailure::resource_limit("invalid message revision"))?;
    Ok(MessageState {
        id: MessageId::from_database(raw_id),
        account_id,
        revision,
        unread,
        starred,
    })
}

fn membership_counts(
    transaction: &Transaction<'_>,
    id: MessageId,
) -> Result<(i64, i64), DbFailure> {
    transaction
        .query_row(
            "SELECT
                 count(*),
                 coalesce(sum(CASE WHEN f.role = 'trash' THEN 1 ELSE 0 END), 0)
             FROM message_folders AS mf
             JOIN folders AS f ON f.id = mf.folder_id
             WHERE mf.message_id = ?1",
            [id.get()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(DbFailure::database)
}

fn canonical_role_folder(
    transaction: &Transaction<'_>,
    account_id: i64,
    role: &str,
) -> Result<i64, DbFailure> {
    let folder_id: Option<i64> = transaction
        .query_row(
            "SELECT min(id)
             FROM folders
             WHERE account_id = ?1 AND role = ?2",
            params![account_id, role],
            |row| row.get(0),
        )
        .map_err(DbFailure::database)?;
    folder_id.ok_or_else(|| DbFailure::conflict(format!("account has no {role} folder")))
}

fn increment_revision(transaction: &Transaction<'_>, id: MessageId) -> Result<(), DbFailure> {
    let changed = transaction
        .execute(
            "UPDATE messages SET revision = revision + 1 WHERE id = ?1",
            [id.get()],
        )
        .map_err(DbFailure::database)?;
    if changed == 1 {
        Ok(())
    } else {
        Err(DbFailure::not_found("message no longer exists"))
    }
}

fn clear_undo(transaction: &Transaction<'_>) -> Result<(), DbFailure> {
    transaction
        .execute("DELETE FROM trash_undo_folders WHERE slot = 1", [])
        .map_err(DbFailure::database)?;
    transaction
        .execute(
            "UPDATE trash_undo
             SET message_id = NULL,
                 account_id = NULL,
                 expires_at_ms = NULL,
                 folder_count = 0
             WHERE slot = 1",
            [],
        )
        .map_err(DbFailure::database)?;
    Ok(())
}

struct UndoSnapshot {
    token: UndoToken,
    message_id: MessageId,
    account_id: i64,
    expires_at_ms: i64,
    folder_count: i64,
}

fn load_undo(transaction: &Transaction<'_>) -> Result<Option<UndoSnapshot>, DbFailure> {
    let (raw_token, raw_message_id, account_id, expires_at_ms, folder_count): (
        i64,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        i64,
    ) = transaction
        .query_row(
            "SELECT token, message_id, account_id, expires_at_ms, folder_count
             FROM trash_undo WHERE slot = 1",
            [],
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
        .map_err(DbFailure::database)?;
    let Some(raw_message_id) = raw_message_id else {
        return Ok(None);
    };
    let (Some(account_id), Some(expires_at_ms)) = (account_id, expires_at_ms) else {
        return Err(DbFailure::conflict("trash undo snapshot is incomplete"));
    };
    Ok(Some(UndoSnapshot {
        token: UndoToken::from_database(raw_token)?,
        message_id: MessageId::from_database(raw_message_id),
        account_id,
        expires_at_ms,
        folder_count,
    }))
}

fn queue_message_files(
    transaction: &Transaction<'_>,
    id: MessageId,
    queued_at_ms: i64,
) -> Result<(), DbFailure> {
    for sql in [
        "INSERT OR IGNORE INTO file_gc (file_key, queued_at_ms)
         SELECT source.body_file_key, ?2 FROM message_content AS source
         WHERE source.message_id = ?1 AND source.body_file_key IS NOT NULL
           AND NOT EXISTS (
               SELECT 1 FROM message_content
               WHERE message_id <> ?1 AND body_file_key = source.body_file_key
           )
           AND NOT EXISTS (
               SELECT 1 FROM attachments
               WHERE message_id <> ?1 AND file_key = source.body_file_key
           )
           AND NOT EXISTS (
               SELECT 1 FROM outbox
               WHERE message_id <> ?1 AND mime_file_key = source.body_file_key
           )",
        "INSERT OR IGNORE INTO file_gc (file_key, queued_at_ms)
         SELECT source.file_key, ?2 FROM attachments AS source
         WHERE source.message_id = ?1 AND source.file_key IS NOT NULL
           AND NOT EXISTS (
               SELECT 1 FROM message_content
               WHERE message_id <> ?1 AND body_file_key = source.file_key
           )
           AND NOT EXISTS (
               SELECT 1 FROM attachments
               WHERE message_id <> ?1 AND file_key = source.file_key
           )
           AND NOT EXISTS (
               SELECT 1 FROM outbox
               WHERE message_id <> ?1 AND mime_file_key = source.file_key
           )",
        "INSERT OR IGNORE INTO file_gc (file_key, queued_at_ms)
         SELECT source.mime_file_key, ?2 FROM outbox AS source
         WHERE source.message_id = ?1
           AND NOT EXISTS (
               SELECT 1 FROM message_content
               WHERE message_id <> ?1 AND body_file_key = source.mime_file_key
           )
           AND NOT EXISTS (
               SELECT 1 FROM attachments
               WHERE message_id <> ?1 AND file_key = source.mime_file_key
           )
           AND NOT EXISTS (
               SELECT 1 FROM outbox
               WHERE message_id <> ?1 AND mime_file_key = source.mime_file_key
           )",
    ] {
        transaction
            .execute(sql, params![id.get(), queued_at_ms])
            .map_err(DbFailure::database)?;
    }
    Ok(())
}

fn checked_expiry(now_ms: i64) -> Result<i64, DbFailure> {
    validate_timestamp(now_ms)?;
    let expires_at_ms = now_ms
        .checked_add(TRASH_UNDO_TTL_MS)
        .ok_or_else(|| DbFailure::resource_limit("trash undo expiry overflow"))?;
    validate_timestamp(expires_at_ms)?;
    Ok(expires_at_ms)
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

    const NOW_MS: i64 = 1_000_000;

    fn database() -> Connection {
        let mut connection = Connection::open_in_memory().unwrap();
        migrate(&mut connection).unwrap();
        connection
            .execute(
                "INSERT INTO accounts
                 (id, provider, remote_key, name, address, state, accent_rgb)
                 VALUES (1, 'imap', 'account-1', 'Personal', 'one@example.test', 'active', 0)",
                [],
            )
            .unwrap();
        for (id, remote_key, role) in [
            (1, "inbox", "inbox"),
            (2, "archive", "archive"),
            (3, "trash", "trash"),
            (4, "label", "label"),
            (5, "sent", "sent"),
            (6, "drafts", "drafts"),
        ] {
            connection
                .execute(
                    "INSERT INTO folders (id, account_id, remote_key, name, role)
                     VALUES (?1, 1, ?2, ?2, ?3)",
                    params![id, remote_key, role],
                )
                .unwrap();
        }
        connection
    }

    fn insert_message(connection: &Connection, id: i64, folders: &[i64]) {
        connection
            .execute(
                "INSERT INTO messages
                 (id, account_id, remote_key, subject, received_at_ms)
                 VALUES (?1, 1, ?2, 'Test', 0)",
                params![id, format!("message-{id}")],
            )
            .unwrap();
        for folder_id in folders {
            connection
                .execute(
                    "INSERT INTO message_folders (message_id, folder_id, account_id)
                     VALUES (?1, ?2, 1)",
                    params![id, folder_id],
                )
                .unwrap();
        }
        super::super::stats::rebuild_account(connection, 1).unwrap();
    }

    fn roles(connection: &Connection, id: i64) -> Vec<String> {
        connection
            .prepare(
                "SELECT f.role
                 FROM message_folders AS mf
                 JOIN folders AS f ON f.id = mf.folder_id
                 WHERE mf.message_id = ?1
                 ORDER BY f.id",
            )
            .unwrap()
            .query_map([id], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    fn stored_stats(connection: &Connection) -> [i64; 7] {
        connection
            .query_row(
                "SELECT inbox_total, inbox_unread, starred_total, sent_total,
                        drafts_total, archive_total, trash_total
                 FROM account_mailbox_stats WHERE account_id = 1",
                [],
                |row| {
                    Ok([
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ])
                },
            )
            .unwrap()
    }

    fn assert_stats_match_rebuild(connection: &Connection) {
        let before = stored_stats(connection);
        super::super::stats::rebuild_account(connection, 1).unwrap();
        assert_eq!(before, stored_stats(connection));
    }

    fn mutate(
        connection: &mut Connection,
        mutation: MessageMutation,
    ) -> Result<MutationOutcome, DbFailure> {
        mutate_message(connection, mutation, NOW_MS)
    }

    fn mutate_at(
        connection: &mut Connection,
        mutation: MessageMutation,
        now_ms: i64,
    ) -> Result<MutationOutcome, DbFailure> {
        mutate_message(connection, mutation, now_ms)
    }

    #[test]
    fn flag_updates_are_idempotent_and_only_changed_values_advance_revision() {
        let mut connection = database();
        insert_message(&connection, 1, &[1]);
        let id = MessageId::new(1).unwrap();

        let MutationOutcome::Updated {
            state,
            changed,
            stats_delta,
        } = mutate(&mut connection, MessageMutation::set_unread(id, true)).unwrap()
        else {
            panic!("expected flag result");
        };
        assert!(!changed);
        assert_eq!(stats_delta.inbox_unread, 0);
        assert_eq!(state.revision, 0);

        let MutationOutcome::Updated {
            state,
            changed,
            stats_delta,
        } = mutate(&mut connection, MessageMutation::set_unread(id, false)).unwrap()
        else {
            panic!("expected flag result");
        };
        assert!(changed);
        assert_eq!(stats_delta.inbox_unread, -1);
        assert_eq!(state.revision, 1);
        assert!(!state.unread);

        let MutationOutcome::Updated {
            state,
            changed,
            stats_delta,
        } = mutate(&mut connection, MessageMutation::set_starred(id, true)).unwrap()
        else {
            panic!("expected flag result");
        };
        assert!(changed);
        assert_eq!(stats_delta.starred_total, 1);
        assert_eq!(state.revision, 2);
        assert!(state.starred);

        let MutationOutcome::Updated { state, changed, .. } =
            mutate(&mut connection, MessageMutation::set_starred(id, true)).unwrap()
        else {
            panic!("expected flag result");
        };
        assert!(!changed);
        assert_eq!(state.revision, 2);
    }

    #[test]
    fn sequential_mutations_keep_persistent_stats_in_sync() {
        let mut connection = database();
        insert_message(&connection, 1, &[1, 4]);
        let id = MessageId::new(1).unwrap();
        assert_stats_match_rebuild(&connection);

        mutate(&mut connection, MessageMutation::set_starred(id, true)).unwrap();
        assert_eq!(stored_stats(&connection), [1, 1, 1, 0, 0, 0, 0]);
        assert_stats_match_rebuild(&connection);

        mutate(&mut connection, MessageMutation::archive(id)).unwrap();
        assert_eq!(stored_stats(&connection), [0, 0, 1, 0, 0, 1, 0]);
        assert_stats_match_rebuild(&connection);

        let MutationOutcome::MovedToTrash { undo, .. } =
            mutate(&mut connection, MessageMutation::move_to_trash(id)).unwrap()
        else {
            panic!("expected Trash result");
        };
        assert_eq!(stored_stats(&connection), [0, 0, 0, 0, 0, 0, 1]);
        assert_stats_match_rebuild(&connection);

        mutate(&mut connection, MessageMutation::set_unread(id, false)).unwrap();
        assert_eq!(stored_stats(&connection), [0, 0, 0, 0, 0, 0, 1]);
        assert_stats_match_rebuild(&connection);

        mutate_at(
            &mut connection,
            MessageMutation::undo_trash(undo.token),
            NOW_MS + 1,
        )
        .unwrap();
        assert_eq!(stored_stats(&connection), [0, 0, 1, 0, 0, 1, 0]);
        assert_stats_match_rebuild(&connection);

        mutate(&mut connection, MessageMutation::move_to_trash(id)).unwrap();
        mutate(&mut connection, MessageMutation::delete_permanently(id)).unwrap();
        assert_eq!(stored_stats(&connection), [0; 7]);
        assert_stats_match_rebuild(&connection);
    }

    #[test]
    fn revision_overflow_rolls_back_the_flag_change() {
        let mut connection = database();
        insert_message(&connection, 1, &[1]);
        connection
            .execute(
                "UPDATE messages SET revision = 9223372036854775807 WHERE id = 1",
                [],
            )
            .unwrap();

        let error = mutate(
            &mut connection,
            MessageMutation::set_unread(MessageId::new(1).unwrap(), false),
        )
        .unwrap_err();

        assert_eq!(error.kind, FailureKind::Database);
        let (unread, revision): (bool, i64) = connection
            .query_row(
                "SELECT unread, revision FROM messages WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert!(unread);
        assert_eq!(revision, i64::MAX);
    }

    #[test]
    fn archive_replaces_inbox_and_preserves_custom_labels() {
        let mut connection = database();
        insert_message(&connection, 1, &[1, 4]);
        let id = MessageId::new(1).unwrap();

        let MutationOutcome::Archived {
            state,
            changed,
            stats_delta,
        } = mutate(&mut connection, MessageMutation::archive(id)).unwrap()
        else {
            panic!("expected archive result");
        };
        assert!(changed);
        assert_eq!(stats_delta.inbox_total, -1);
        assert_eq!(stats_delta.archive_total, 1);
        assert_eq!(state.revision, 1);
        assert_eq!(roles(&connection, 1), ["archive", "label"]);

        let MutationOutcome::Archived { state, changed, .. } =
            mutate(&mut connection, MessageMutation::archive(id)).unwrap()
        else {
            panic!("expected archive result");
        };
        assert!(!changed);
        assert_eq!(state.revision, 1);
    }

    #[test]
    fn archive_rejects_sent_and_draft_placements() {
        let mut connection = database();
        insert_message(&connection, 1, &[5]);
        insert_message(&connection, 2, &[6]);

        for id in [1, 2] {
            let error = mutate(
                &mut connection,
                MessageMutation::archive(MessageId::new(id).unwrap()),
            )
            .unwrap_err();
            assert_eq!(error.kind, FailureKind::Conflict);
        }
        assert_eq!(roles(&connection, 1), ["sent"]);
        assert_eq!(roles(&connection, 2), ["drafts"]);
    }

    #[test]
    fn legacy_duplicate_archive_uses_the_lowest_stable_folder_id() {
        let mut connection = database();
        connection
            .execute("DROP TRIGGER reject_duplicate_system_role_insert", [])
            .unwrap();
        connection
            .execute(
                "INSERT INTO folders (id, account_id, remote_key, name, role)
                 VALUES (7, 1, 'archive-legacy', 'Legacy Archive', 'archive')",
                [],
            )
            .unwrap();
        insert_message(&connection, 1, &[1]);

        mutate(
            &mut connection,
            MessageMutation::archive(MessageId::new(1).unwrap()),
        )
        .unwrap();

        let folder_id: i64 = connection
            .query_row(
                "SELECT folder_id FROM message_folders WHERE message_id = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(folder_id, 2);
    }

    #[test]
    fn trash_undo_restores_all_folders_and_keeps_later_flag_changes() {
        let mut connection = database();
        insert_message(&connection, 1, &[1, 4]);
        let id = MessageId::new(1).unwrap();

        let MutationOutcome::MovedToTrash { state, undo, .. } =
            mutate(&mut connection, MessageMutation::move_to_trash(id)).unwrap()
        else {
            panic!("expected Trash result");
        };
        assert_eq!(state.revision, 1);
        assert_eq!(roles(&connection, 1), ["trash"]);

        mutate(&mut connection, MessageMutation::set_starred(id, true)).unwrap();
        let MutationOutcome::Restored { state, .. } = mutate_at(
            &mut connection,
            MessageMutation::undo_trash(undo.token),
            NOW_MS + 1,
        )
        .unwrap() else {
            panic!("expected restored result");
        };
        assert_eq!(roles(&connection, 1), ["inbox", "label"]);
        assert!(state.starred);
        assert_eq!(state.revision, 3);

        let error = mutate_at(
            &mut connection,
            MessageMutation::undo_trash(undo.token),
            NOW_MS + 2,
        )
        .unwrap_err();
        assert_eq!(error.kind, FailureKind::Conflict);
    }

    #[test]
    fn newer_trash_action_replaces_the_previous_undo_token() {
        let mut connection = database();
        insert_message(&connection, 1, &[1]);
        insert_message(&connection, 2, &[1]);
        let first = MessageId::new(1).unwrap();
        let second = MessageId::new(2).unwrap();
        let MutationOutcome::MovedToTrash {
            undo: first_undo, ..
        } = mutate_at(
            &mut connection,
            MessageMutation::move_to_trash(first),
            NOW_MS,
        )
        .unwrap()
        else {
            panic!("expected first Trash result");
        };
        let MutationOutcome::MovedToTrash {
            undo: second_undo, ..
        } = mutate_at(
            &mut connection,
            MessageMutation::move_to_trash(second),
            NOW_MS + 1,
        )
        .unwrap()
        else {
            panic!("expected second Trash result");
        };

        let error = mutate_at(
            &mut connection,
            MessageMutation::undo_trash(first_undo.token),
            NOW_MS + 2,
        )
        .unwrap_err();
        assert_eq!(error.kind, FailureKind::Conflict);
        assert_eq!(roles(&connection, 1), ["trash"]);
        assert_eq!(roles(&connection, 2), ["trash"]);

        mutate_at(
            &mut connection,
            MessageMutation::undo_trash(second_undo.token),
            NOW_MS + 2,
        )
        .unwrap();
        assert_eq!(roles(&connection, 2), ["inbox"]);
    }

    #[test]
    fn expired_undo_is_consumed_without_changing_the_message() {
        let mut connection = database();
        insert_message(&connection, 1, &[1]);
        let id = MessageId::new(1).unwrap();
        let MutationOutcome::MovedToTrash { undo, .. } =
            mutate(&mut connection, MessageMutation::move_to_trash(id)).unwrap()
        else {
            panic!("expected Trash result");
        };

        let error = mutate_at(
            &mut connection,
            MessageMutation::undo_trash(undo.token),
            undo.expires_at_ms + 1,
        )
        .unwrap_err();
        assert_eq!(error.kind, FailureKind::Conflict);
        assert_eq!(roles(&connection, 1), ["trash"]);

        let active: Option<i64> = connection
            .query_row(
                "SELECT message_id FROM trash_undo WHERE slot = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(active, None);
    }

    #[test]
    fn trash_snapshot_rejects_pathological_folder_counts_before_writing() {
        let mut connection = database();
        insert_message(&connection, 1, &[]);
        for offset in 0..=MAX_UNDO_FOLDERS {
            let folder_id = 100 + offset;
            connection
                .execute(
                    "INSERT INTO folders (id, account_id, remote_key, name, role)
                     VALUES (?1, 1, ?2, 'Label', 'label')",
                    params![folder_id, format!("label-{offset}")],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO message_folders (message_id, folder_id, account_id)
                     VALUES (1, ?1, 1)",
                    [folder_id],
                )
                .unwrap();
        }

        let error = mutate(
            &mut connection,
            MessageMutation::move_to_trash(MessageId::new(1).unwrap()),
        )
        .unwrap_err();

        assert_eq!(error.kind, FailureKind::ResourceLimit);
        let membership_count: i64 = connection
            .query_row(
                "SELECT count(*) FROM message_folders WHERE message_id = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let undo_message: Option<i64> = connection
            .query_row(
                "SELECT message_id FROM trash_undo WHERE slot = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(membership_count, MAX_UNDO_FOLDERS + 1);
        assert_eq!(undo_message, None);
    }

    #[test]
    fn permanent_delete_requires_trash_and_queues_files_and_tombstone() {
        let mut connection = database();
        insert_message(&connection, 1, &[1]);
        let id = MessageId::new(1).unwrap();
        let error = mutate(&mut connection, MessageMutation::delete_permanently(id)).unwrap_err();
        assert_eq!(error.kind, FailureKind::Conflict);
        assert_eq!(roles(&connection, 1), ["inbox"]);

        mutate(&mut connection, MessageMutation::move_to_trash(id)).unwrap();
        connection
            .execute(
                "INSERT INTO message_content (message_id, body_file_key)
                 VALUES (1, 'body/1.eml')",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO attachments (id, message_id, ordinal, file_key)
                 VALUES (1, 1, 0, 'attachments/1.bin')",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO outbox
                 (message_id, mime_file_key, envelope_from, wire_byte_count, state)
                 VALUES (1, 'outbox/1.eml', 'one@example.test', 1, 'pending')",
                [],
            )
            .unwrap();

        let MutationOutcome::PermanentlyDeleted {
            id: deleted_id,
            account_id,
            stats_delta,
        } = mutate(&mut connection, MessageMutation::delete_permanently(id)).unwrap()
        else {
            panic!("expected permanent deletion result");
        };
        assert_eq!(deleted_id, id);
        assert_eq!(account_id, 1);
        assert_eq!(stats_delta.trash_total, -1);
        let message_count: i64 = connection
            .query_row("SELECT count(*) FROM messages", [], |row| row.get(0))
            .unwrap();
        let tombstone_count: i64 = connection
            .query_row("SELECT count(*) FROM message_tombstones", [], |row| {
                row.get(0)
            })
            .unwrap();
        let file_count: i64 = connection
            .query_row("SELECT count(*) FROM file_gc", [], |row| row.get(0))
            .unwrap();
        assert_eq!(message_count, 0);
        assert_eq!(tombstone_count, 1);
        assert_eq!(file_count, 3);
        let undo_message: Option<i64> = connection
            .query_row(
                "SELECT message_id FROM trash_undo WHERE slot = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let undo_folder_count: i64 = connection
            .query_row("SELECT count(*) FROM trash_undo_folders", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(undo_message, None);
        assert_eq!(undo_folder_count, 0);
    }

    #[test]
    fn permanent_delete_handles_legacy_trash_labels_without_queueing_shared_files() {
        let mut connection = database();
        insert_message(&connection, 1, &[3, 4]);
        insert_message(&connection, 2, &[1]);
        connection
            .execute(
                "INSERT INTO message_content (message_id, body_file_key)
                 VALUES (1, 'shared/file.eml')",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO attachments (id, message_id, ordinal, file_key)
                 VALUES (1, 1, 0, 'unique/file.bin'),
                        (2, 2, 0, 'shared/file.eml')",
                [],
            )
            .unwrap();

        mutate(
            &mut connection,
            MessageMutation::delete_permanently(MessageId::new(1).unwrap()),
        )
        .unwrap();

        let queued = connection
            .prepare("SELECT file_key FROM file_gc ORDER BY file_key")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(queued, ["unique/file.bin"]);
        let survivor_count: i64 = connection
            .query_row("SELECT count(*) FROM messages WHERE id = 2", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(survivor_count, 1);
    }

    #[test]
    fn missing_archive_folder_rolls_back_without_partial_changes() {
        let mut connection = database();
        insert_message(&connection, 1, &[1, 4]);
        connection
            .execute("DELETE FROM folders WHERE id = 2", [])
            .unwrap();

        let error = mutate(
            &mut connection,
            MessageMutation::archive(MessageId::new(1).unwrap()),
        )
        .unwrap_err();

        assert_eq!(error.kind, FailureKind::Conflict);
        assert_eq!(roles(&connection, 1), ["inbox", "label"]);
        let revision: i64 = connection
            .query_row("SELECT revision FROM messages WHERE id = 1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(revision, 0);
    }
}
