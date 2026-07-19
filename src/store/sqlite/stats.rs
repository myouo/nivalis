use rusqlite::{Connection, OptionalExtension, Transaction, params};

use super::domain::{
    AccountScope, AccountStatsDelta, AccountUnreadDto, DbFailure, FolderScope, MAX_ACCOUNT_STATS,
    MailboxStatsDto, MessageId, PageSpec,
};

const COUNTER_COUNT: usize = 7;
const INBOX_TOTAL: usize = 0;
const INBOX_UNREAD: usize = 1;
const STARRED_TOTAL: usize = 2;
const SENT_TOTAL: usize = 3;
const DRAFTS_TOTAL: usize = 4;
const ARCHIVE_TOTAL: usize = 5;
const TRASH_TOTAL: usize = 6;

const MESSAGE_CLASSIFICATION_SQL: &str = "
    SELECT m.account_id, m.unread, m.starred,
           count(mf.folder_id) <> 0,
           max(CASE WHEN f.role = 'inbox' THEN 1 ELSE 0 END),
           max(CASE WHEN f.role = 'sent' THEN 1 ELSE 0 END),
           max(CASE WHEN f.role = 'drafts' THEN 1 ELSE 0 END),
           max(CASE WHEN f.role = 'archive' THEN 1 ELSE 0 END),
           max(CASE WHEN f.role = 'trash' THEN 1 ELSE 0 END),
           s.dirty
      FROM messages AS m
      LEFT JOIN account_mailbox_stats AS s
        ON s.account_id = m.account_id
      LEFT JOIN message_folders AS mf
        ON mf.message_id = m.id
       AND mf.account_id = m.account_id
      LEFT JOIN folders AS f
        ON f.id = mf.folder_id
       AND f.account_id = mf.account_id
     WHERE m.id = ?1
     GROUP BY m.id";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct MessageStatsSnapshot {
    account_id: i64,
    values: [i8; COUNTER_COUNT],
    stats_dirty: Option<bool>,
}

pub(super) fn load_message_snapshot(
    transaction: &Transaction<'_>,
    id: MessageId,
) -> Result<MessageStatsSnapshot, DbFailure> {
    transaction
        .query_row(MESSAGE_CLASSIFICATION_SQL, [id.get()], classify_row)
        .optional()
        .map_err(DbFailure::database)?
        .ok_or_else(|| DbFailure::not_found("message no longer exists"))
}

pub(super) fn apply_transition(
    transaction: &Transaction<'_>,
    before: MessageStatsSnapshot,
    after: Option<MessageStatsSnapshot>,
) -> Result<AccountStatsDelta, DbFailure> {
    if before.stats_dirty != Some(false) {
        return Err(DbFailure::conflict(
            "mailbox statistics must be rebuilt before applying a mutation",
        ));
    }
    if after.is_some_and(|snapshot| snapshot.account_id != before.account_id) {
        return Err(DbFailure::conflict(
            "message account changed during mailbox statistics update",
        ));
    }

    let after_values = after.map_or([0; COUNTER_COUNT], |snapshot| snapshot.values);
    let mut delta = [0_i8; COUNTER_COUNT];
    for (index, value) in delta.iter_mut().enumerate() {
        *value = after_values[index] - before.values[index];
    }
    apply_delta(transaction, before.account_id, delta)?;
    Ok(AccountStatsDelta::from_values(before.account_id, delta))
}

fn apply_delta(
    transaction: &Transaction<'_>,
    account_id: i64,
    delta: [i8; COUNTER_COUNT],
) -> Result<(), DbFailure> {
    let changed = transaction
        .execute(
            "UPDATE account_mailbox_stats
             SET inbox_total = inbox_total + ?2,
                 inbox_unread = inbox_unread + ?3,
                 starred_total = starred_total + ?4,
                 sent_total = sent_total + ?5,
                 drafts_total = drafts_total + ?6,
                 archive_total = archive_total + ?7,
                 trash_total = trash_total + ?8,
                 dirty = 0
             WHERE account_id = ?1
               AND (?2 >= 0 OR inbox_total > 0)
               AND (?2 <= 0 OR inbox_total < 9223372036854775807)
               AND (?3 >= 0 OR inbox_unread > 0)
               AND (?3 <= 0 OR inbox_unread < 9223372036854775807)
               AND (?4 >= 0 OR starred_total > 0)
               AND (?4 <= 0 OR starred_total < 9223372036854775807)
               AND (?5 >= 0 OR sent_total > 0)
               AND (?5 <= 0 OR sent_total < 9223372036854775807)
               AND (?6 >= 0 OR drafts_total > 0)
               AND (?6 <= 0 OR drafts_total < 9223372036854775807)
               AND (?7 >= 0 OR archive_total > 0)
               AND (?7 <= 0 OR archive_total < 9223372036854775807)
               AND (?8 >= 0 OR trash_total > 0)
               AND (?8 <= 0 OR trash_total < 9223372036854775807)
               AND inbox_unread + ?3 <= inbox_total + ?2",
            params![
                account_id,
                delta[INBOX_TOTAL],
                delta[INBOX_UNREAD],
                delta[STARRED_TOTAL],
                delta[SENT_TOTAL],
                delta[DRAFTS_TOTAL],
                delta[ARCHIVE_TOTAL],
                delta[TRASH_TOTAL],
            ],
        )
        .map_err(DbFailure::database)?;
    if changed == 1 {
        Ok(())
    } else {
        Err(DbFailure::conflict(
            "mailbox statistics are missing or inconsistent",
        ))
    }
}

pub(super) fn rebuild_account(connection: &Connection, account_id: i64) -> Result<(), DbFailure> {
    let changed = connection
        .execute(
            "WITH classified AS (
                 SELECT m.unread, m.starred,
                        count(mf.folder_id) <> 0 AS has_membership,
                        max(CASE WHEN f.role = 'inbox' THEN 1 ELSE 0 END) AS has_inbox,
                        max(CASE WHEN f.role = 'sent' THEN 1 ELSE 0 END) AS has_sent,
                        max(CASE WHEN f.role = 'drafts' THEN 1 ELSE 0 END) AS has_drafts,
                        max(CASE WHEN f.role = 'archive' THEN 1 ELSE 0 END) AS has_archive,
                        max(CASE WHEN f.role = 'trash' THEN 1 ELSE 0 END) AS has_trash
                   FROM messages AS m
                   LEFT JOIN message_folders AS mf
                     ON mf.message_id = m.id
                    AND mf.account_id = m.account_id
                   LEFT JOIN folders AS f
                     ON f.id = mf.folder_id
                    AND f.account_id = mf.account_id
                  WHERE m.account_id = ?1
                  GROUP BY m.id
             ), totals AS (
                 SELECT
                     coalesce(sum(has_inbox AND NOT has_trash), 0) AS inbox_total,
                     coalesce(sum(unread AND has_inbox AND NOT has_trash), 0) AS inbox_unread,
                     coalesce(sum(starred AND has_membership AND NOT has_trash), 0)
                         AS starred_total,
                     coalesce(sum(has_sent AND NOT has_trash), 0) AS sent_total,
                     coalesce(sum(has_drafts AND NOT has_trash), 0) AS drafts_total,
                     coalesce(sum(has_archive AND NOT has_trash), 0) AS archive_total,
                     coalesce(sum(has_trash), 0) AS trash_total
                 FROM classified
             )
             UPDATE account_mailbox_stats
             SET (inbox_total, inbox_unread, starred_total, sent_total,
                  drafts_total, archive_total, trash_total, dirty) = (
                 SELECT inbox_total, inbox_unread, starred_total, sent_total,
                        drafts_total, archive_total, trash_total, 0
                 FROM totals
             )
             WHERE account_id = ?1",
            [account_id],
        )
        .map_err(DbFailure::database)?;
    if changed == 1 {
        Ok(())
    } else {
        Err(DbFailure::not_found("mail account no longer exists"))
    }
}

pub(super) fn query_mailbox_stats(
    connection: &Connection,
    spec: &PageSpec,
) -> Result<MailboxStatsDto, DbFailure> {
    let row_limit = i64::try_from(MAX_ACCOUNT_STATS + 1)
        .map_err(|_| DbFailure::resource_limit("account statistics limit is invalid"))?;
    let mut statement = connection
        .prepare(
            "SELECT a.id, s.account_id, s.inbox_total, s.inbox_unread,
                    s.starred_total, s.sent_total, s.drafts_total,
                    s.archive_total, s.trash_total, s.dirty
             FROM accounts AS a
             LEFT JOIN account_mailbox_stats AS s ON s.account_id = a.id
             ORDER BY a.sort_order, a.id
             LIMIT ?1",
        )
        .map_err(DbFailure::database)?;
    let mapped = statement
        .query_map([row_limit], |row| {
            let stats_account_id: Option<i64> = row.get(1)?;
            let values = if stats_account_id.is_some() {
                Some([
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                ])
            } else {
                None
            };
            Ok((
                row.get::<_, i64>(0)?,
                values,
                row.get::<_, Option<bool>>(9)?,
            ))
        })
        .map_err(DbFailure::database)?;

    let mut accounts = Vec::with_capacity(MAX_ACCOUNT_STATS + 1);
    for row in mapped {
        accounts.push(row.map_err(DbFailure::database)?);
    }
    if accounts.len() > MAX_ACCOUNT_STATS {
        return Err(DbFailure::resource_limit(format!(
            "mail account count exceeds the {MAX_ACCOUNT_STATS}-account limit"
        )));
    }

    let mut total = [0_u64; COUNTER_COUNT];
    let mut account_unread = Vec::with_capacity(accounts.len());
    let mut selected = None;
    for (account_id, raw_values, dirty) in accounts {
        let raw_values = raw_values
            .ok_or_else(|| DbFailure::conflict("mail account is missing its statistics row"))?;
        if dirty != Some(false) {
            return Err(DbFailure::conflict(
                "mailbox statistics are stale and must be rebuilt",
            ));
        }
        let values = convert_counts(raw_values)?;
        for (sum, value) in total.iter_mut().zip(values) {
            *sum = sum
                .checked_add(value)
                .ok_or_else(|| DbFailure::resource_limit("mailbox statistics overflow"))?;
        }
        account_unread.push(AccountUnreadDto {
            account_id,
            unread: values[INBOX_UNREAD],
        });
        if spec.account.database_id() == Some(account_id) {
            selected = Some(values);
        }
    }

    let scoped = match spec.account {
        AccountScope::All => total,
        AccountScope::Account(_) => {
            selected.ok_or_else(|| DbFailure::not_found("mail account no longer exists"))?
        }
    };
    let selected_total = spec.search.is_none().then(|| match spec.folder {
        FolderScope::Inbox => scoped[INBOX_TOTAL],
        FolderScope::Unread => scoped[INBOX_UNREAD],
        FolderScope::Starred => scoped[STARRED_TOTAL],
        FolderScope::Sent => scoped[SENT_TOTAL],
        FolderScope::Drafts => scoped[DRAFTS_TOTAL],
        FolderScope::Archive => scoped[ARCHIVE_TOTAL],
        FolderScope::Trash => scoped[TRASH_TOTAL],
    });

    Ok(MailboxStatsDto {
        selected_total,
        inbox_unread: scoped[INBOX_UNREAD],
        starred_total: scoped[STARRED_TOTAL],
        drafts_total: scoped[DRAFTS_TOTAL],
        account_unread: account_unread.into_boxed_slice(),
    })
}

fn classify_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<MessageStatsSnapshot> {
    let account_id = row.get(0)?;
    let unread: bool = row.get(1)?;
    let starred: bool = row.get(2)?;
    let has_membership: bool = row.get(3)?;
    let has_inbox: bool = row.get(4)?;
    let has_sent: bool = row.get(5)?;
    let has_drafts: bool = row.get(6)?;
    let has_archive: bool = row.get(7)?;
    let has_trash: bool = row.get(8)?;
    let stats_dirty: Option<bool> = row.get(9)?;
    let active = !has_trash;
    Ok(MessageStatsSnapshot {
        account_id,
        values: [
            flag(active && has_inbox),
            flag(active && has_inbox && unread),
            flag(active && has_membership && starred),
            flag(active && has_sent),
            flag(active && has_drafts),
            flag(active && has_archive),
            flag(has_trash),
        ],
        stats_dirty,
    })
}

fn flag(value: bool) -> i8 {
    if value { 1 } else { 0 }
}

#[cfg(test)]
fn read_counter_array(row: &rusqlite::Row<'_>) -> rusqlite::Result<[i64; COUNTER_COUNT]> {
    Ok([
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
    ])
}

fn convert_counts(values: [i64; COUNTER_COUNT]) -> Result<[u64; COUNTER_COUNT], DbFailure> {
    let mut converted = [0_u64; COUNTER_COUNT];
    for (output, value) in converted.iter_mut().zip(values) {
        *output = u64::try_from(value)
            .map_err(|_| DbFailure::resource_limit("invalid negative mailbox statistic"))?;
    }
    Ok(converted)
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::*;
    use crate::store::sqlite::PageBoundary;
    use crate::store::sqlite::{
        domain::{FailureKind, PageSpec},
        migrations::migrate,
    };

    fn database() -> Connection {
        let mut connection = Connection::open_in_memory().unwrap();
        migrate(&mut connection).unwrap();
        connection
            .execute_batch(
                "DROP TRIGGER reject_duplicate_system_role_insert;
                 INSERT INTO accounts
                     (id, provider, remote_key, name, address, state, accent_rgb)
                 VALUES (1, 'imap', 'one', 'One', 'one@example.test', 'active', 0),
                        (2, 'imap', 'two', 'Two', 'two@example.test', 'active', 0);
                 INSERT INTO folders (id, account_id, remote_key, name, role)
                 VALUES (1, 1, 'inbox', 'Inbox', 'inbox'),
                        (2, 1, 'inbox-legacy', 'Legacy Inbox', 'inbox'),
                        (3, 1, 'archive', 'Archive', 'archive'),
                        (4, 1, 'trash', 'Trash', 'trash');
                 INSERT INTO messages
                     (id, account_id, remote_key, received_at_ms, unread, starred)
                 VALUES (1, 1, 'message', 0, 1, 1);
                 INSERT INTO message_folders (message_id, folder_id, account_id)
                 VALUES (1, 1, 1), (1, 2, 1);",
            )
            .unwrap();
        rebuild_account(&connection, 1).unwrap();
        connection
    }

    fn stored(connection: &Connection, account_id: i64) -> [i64; COUNTER_COUNT] {
        connection
            .query_row(
                "SELECT inbox_total, inbox_unread, starred_total, sent_total,
                        drafts_total, archive_total, trash_total
                 FROM account_mailbox_stats WHERE account_id = ?1",
                [account_id],
                read_counter_array,
            )
            .unwrap()
    }

    #[test]
    fn fixed_delta_tracks_flags_archive_and_trash_precedence() {
        let mut connection = database();
        assert_eq!(stored(&connection, 1), [1, 1, 1, 0, 0, 0, 0]);

        let transaction = connection.transaction().unwrap();
        let before = load_message_snapshot(&transaction, MessageId::new(1).unwrap()).unwrap();
        transaction
            .execute("UPDATE messages SET unread = 0 WHERE id = 1", [])
            .unwrap();
        let after = load_message_snapshot(&transaction, MessageId::new(1).unwrap()).unwrap();
        let delta = apply_transition(&transaction, before, Some(after)).unwrap();
        assert_eq!(delta.inbox_unread, -1);
        transaction.commit().unwrap();
        assert_eq!(stored(&connection, 1), [1, 0, 1, 0, 0, 0, 0]);

        let transaction = connection.transaction().unwrap();
        let before = load_message_snapshot(&transaction, MessageId::new(1).unwrap()).unwrap();
        transaction
            .execute("DELETE FROM message_folders WHERE message_id = 1", [])
            .unwrap();
        transaction
            .execute(
                "INSERT INTO message_folders (message_id, folder_id, account_id)
                 VALUES (1, 3, 1)",
                [],
            )
            .unwrap();
        let after = load_message_snapshot(&transaction, MessageId::new(1).unwrap()).unwrap();
        apply_transition(&transaction, before, Some(after)).unwrap();
        transaction.commit().unwrap();
        assert_eq!(stored(&connection, 1), [0, 0, 1, 0, 0, 1, 0]);

        let transaction = connection.transaction().unwrap();
        let before = load_message_snapshot(&transaction, MessageId::new(1).unwrap()).unwrap();
        transaction
            .execute(
                "INSERT INTO message_folders (message_id, folder_id, account_id)
                 VALUES (1, 4, 1)",
                [],
            )
            .unwrap();
        let after = load_message_snapshot(&transaction, MessageId::new(1).unwrap()).unwrap();
        apply_transition(&transaction, before, Some(after)).unwrap();
        transaction.commit().unwrap();
        assert_eq!(stored(&connection, 1), [0, 0, 0, 0, 0, 0, 1]);
    }

    #[test]
    fn delete_delta_removes_the_message_contribution() {
        let mut connection = database();
        let transaction = connection.transaction().unwrap();
        let before = load_message_snapshot(&transaction, MessageId::new(1).unwrap()).unwrap();
        transaction
            .execute("DELETE FROM messages WHERE id = 1", [])
            .unwrap();

        let delta = apply_transition(&transaction, before, None).unwrap();

        assert_eq!(delta.inbox_total, -1);
        assert_eq!(delta.inbox_unread, -1);
        assert_eq!(delta.starred_total, -1);
        transaction.commit().unwrap();
        assert_eq!(stored(&connection, 1), [0; COUNTER_COUNT]);
    }

    #[test]
    fn inconsistent_stats_reject_delta_and_rebuild_repairs_them() {
        let mut connection = database();
        connection
            .execute(
                "UPDATE account_mailbox_stats
                 SET inbox_total = 0, inbox_unread = 0, starred_total = 0
                 WHERE account_id = 1",
                [],
            )
            .unwrap();
        let transaction = connection.transaction().unwrap();
        let before = load_message_snapshot(&transaction, MessageId::new(1).unwrap()).unwrap();
        transaction
            .execute("UPDATE messages SET unread = 0 WHERE id = 1", [])
            .unwrap();
        let after = load_message_snapshot(&transaction, MessageId::new(1).unwrap()).unwrap();
        assert!(apply_transition(&transaction, before, Some(after)).is_err());
        transaction.rollback().unwrap();

        rebuild_account(&connection, 1).unwrap();

        assert_eq!(stored(&connection, 1), [1, 1, 1, 0, 0, 0, 0]);
    }

    #[test]
    fn raw_mailbox_writes_are_detected_until_stats_are_rebuilt() {
        let connection = database();
        let inbox = PageSpec::new(
            AccountScope::All,
            FolderScope::Inbox,
            None,
            PageBoundary::First,
            50,
        )
        .unwrap();

        connection
            .execute("UPDATE messages SET starred = 0 WHERE id = 1", [])
            .unwrap();

        let error = query_mailbox_stats(&connection, &inbox).unwrap_err();
        assert_eq!(error.kind, FailureKind::Conflict);

        rebuild_account(&connection, 1).unwrap();
        let stats = query_mailbox_stats(&connection, &inbox).unwrap();
        assert_eq!(stats.selected_total, Some(1));
        assert_eq!(stats.starred_total, 0);
    }

    #[test]
    fn mailbox_stats_are_scoped_bounded_and_skip_search_counts() {
        let connection = database();
        let inbox = PageSpec::new(
            AccountScope::All,
            FolderScope::Inbox,
            None,
            PageBoundary::First,
            50,
        )
        .unwrap();
        let stats = query_mailbox_stats(&connection, &inbox).unwrap();
        assert_eq!(stats.selected_total, Some(1));
        assert_eq!(stats.inbox_unread, 1);
        assert_eq!(stats.starred_total, 1);
        assert_eq!(stats.account_unread.len(), 2);

        let search = PageSpec::new(
            AccountScope::account(1).unwrap(),
            FolderScope::Inbox,
            Some("message"),
            PageBoundary::First,
            50,
        )
        .unwrap();
        assert_eq!(
            query_mailbox_stats(&connection, &search)
                .unwrap()
                .selected_total,
            None
        );

        for id in 3..=64 {
            connection
                .execute(
                    "INSERT INTO accounts
                     (id, provider, remote_key, name, address, state, accent_rgb)
                     VALUES (?1, 'imap', ?2, 'Account', ?2, 'active', 0)",
                    params![id, format!("account-{id}")],
                )
                .unwrap();
        }
        let error = connection
            .execute(
                "INSERT INTO accounts
                 (id, provider, remote_key, name, address, state, accent_rgb)
                 VALUES (65, 'imap', 'account-65', 'Account', 'account-65', 'active', 0)",
                [],
            )
            .unwrap_err();
        assert_eq!(
            error.sqlite_error_code(),
            Some(rusqlite::ErrorCode::ConstraintViolation)
        );
        let stats = query_mailbox_stats(&connection, &inbox).unwrap();
        assert_eq!(stats.account_unread.len(), MAX_ACCOUNT_STATS);
    }
}
