use std::fmt::Write;

use rusqlite::{
    Connection, OptionalExtension, params_from_iter,
    types::{Type, Value},
};

use super::domain::{
    AccountDirectory, AccountSummaryDto, DbFailure, FolderScope, MAX_ACCOUNT_STATS, MailSummaryDto,
    MailboxPage, MessageDetail, MessageId, PageBoundary, PageCursor, PageSpec,
};
use super::stats::query_mailbox_stats;

const ACCOUNT_DIRECTORY_SQL: &str = "
    SELECT a.id, a.name, a.address, a.state, a.accent_rgb,
           s.account_id, s.inbox_unread, s.dirty
      FROM accounts AS a
      LEFT JOIN account_mailbox_stats AS s ON s.account_id = a.id
     ORDER BY a.sort_order, a.id
     LIMIT ?1";

const MAILBOX_SELECT: &str = "
    SELECT m.id, m.account_id, m.sender_name, m.sender_address, m.subject, m.preview,
           m.received_at_ms, m.unread, m.starred, m.has_attachment
      FROM messages AS m
     WHERE ";

const OPEN_MESSAGE_SQL: &str = "
    SELECT m.id, m.account_id, m.sender_name, m.sender_address, m.subject, m.received_at_ms,
           m.unread, m.starred, m.has_attachment, coalesce(c.reader_excerpt, ''),
           coalesce(c.truncated, 0), coalesce(c.body_byte_count, 0), c.body_file_key
      FROM messages AS m
      LEFT JOIN message_content AS c ON c.message_id = m.id
     WHERE m.id = ?1";

pub(super) fn query_account_directory(
    connection: &Connection,
) -> Result<AccountDirectory, DbFailure> {
    let row_limit = i64::try_from(MAX_ACCOUNT_STATS + 1)
        .map_err(|_| DbFailure::resource_limit("account directory limit is invalid"))?;
    let mut statement = connection
        .prepare(ACCOUNT_DIRECTORY_SQL)
        .map_err(DbFailure::database)?;
    let mapped = statement
        .query_map([row_limit], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, Option<i64>>(5)?,
                row.get::<_, Option<i64>>(6)?,
                row.get::<_, Option<bool>>(7)?,
            ))
        })
        .map_err(DbFailure::database)?;

    let mut rows = Vec::with_capacity(MAX_ACCOUNT_STATS + 1);
    for row in mapped {
        let (id, name, address, state, accent_rgb, stats_account_id, inbox_unread, dirty) =
            row.map_err(DbFailure::database)?;
        let (Some(stats_account_id), Some(inbox_unread), Some(dirty)) =
            (stats_account_id, inbox_unread, dirty)
        else {
            return Err(DbFailure::conflict(
                "mail account is missing its statistics row",
            ));
        };
        if stats_account_id != id {
            return Err(DbFailure::conflict(
                "mail account statistics identity does not match",
            ));
        }
        if dirty {
            return Err(DbFailure::conflict(
                "mailbox statistics are stale and must be rebuilt",
            ));
        }
        rows.push(AccountSummaryDto {
            id,
            name: name.into_boxed_str(),
            address: address.into_boxed_str(),
            state: state.into_boxed_str(),
            accent_rgb: u32::try_from(accent_rgb)
                .map_err(|_| DbFailure::resource_limit("invalid account accent color"))?,
            inbox_unread: u64::try_from(inbox_unread)
                .map_err(|_| DbFailure::resource_limit("invalid negative inbox statistic"))?,
        });
    }
    if rows.len() > MAX_ACCOUNT_STATS {
        return Err(DbFailure::resource_limit(format!(
            "mail account count exceeds the {MAX_ACCOUNT_STATS}-account limit"
        )));
    }

    Ok(AccountDirectory {
        rows: rows.into_boxed_slice(),
    })
}

pub(super) fn query_mailbox(
    connection: &Connection,
    spec: &PageSpec,
) -> Result<MailboxPage, DbFailure> {
    let stats = query_mailbox_stats(connection, spec)?;
    let (sql, parameters) = mailbox_query(spec);
    let mut statement = connection.prepare(&sql).map_err(DbFailure::database)?;
    let mapped = statement
        .query_map(params_from_iter(parameters.iter()), |row| {
            Ok(MailSummaryDto {
                id: MessageId::from_database(row.get(0)?),
                account_id: row.get(1)?,
                sender_name: row.get::<_, String>(2)?.into_boxed_str(),
                sender_address: row.get::<_, String>(3)?.into_boxed_str(),
                subject: row.get::<_, String>(4)?.into_boxed_str(),
                preview: row.get::<_, String>(5)?.into_boxed_str(),
                received_at_ms: row.get(6)?,
                unread: row.get(7)?,
                starred: row.get(8)?,
                has_attachment: row.get(9)?,
            })
        })
        .map_err(DbFailure::database)?;
    let mut rows = Vec::with_capacity(usize::from(spec.limit) + 1);
    for row in mapped {
        rows.push(row.map_err(DbFailure::database)?);
    }

    let has_more = rows.len() > usize::from(spec.limit);
    rows.truncate(usize::from(spec.limit));
    if matches!(spec.boundary, PageBoundary::Before(_)) {
        rows.reverse();
    }

    let first_cursor = rows.first().map(page_cursor);
    let last_cursor = rows.last().map(page_cursor);
    let (previous_cursor, next_cursor) = match spec.boundary {
        PageBoundary::First => (None, has_more.then_some(last_cursor).flatten()),
        PageBoundary::After(_) => (first_cursor, has_more.then_some(last_cursor).flatten()),
        PageBoundary::Before(_) => (has_more.then_some(first_cursor).flatten(), last_cursor),
    };

    Ok(MailboxPage {
        rows: rows.into_boxed_slice(),
        previous_cursor,
        next_cursor,
        stats,
    })
}

fn page_cursor(row: &MailSummaryDto) -> PageCursor {
    PageCursor {
        received_at_ms: row.received_at_ms,
        message_id: row.id,
    }
}

fn mailbox_query(spec: &PageSpec) -> (String, Vec<Value>) {
    let mut sql = String::with_capacity(MAILBOX_SELECT.len() + 768);
    let mut parameters = Vec::with_capacity(5);
    sql.push_str(MAILBOX_SELECT);
    sql.push_str(match spec.folder {
        FolderScope::Inbox => {
            "EXISTS (
                SELECT 1 FROM message_folders AS mf
                JOIN folders AS f ON f.id = mf.folder_id AND f.account_id = mf.account_id
                WHERE mf.message_id = m.id AND mf.account_id = m.account_id
                  AND f.role = 'inbox'
            )"
        }
        FolderScope::Starred => {
            "m.starred = 1 AND EXISTS (
                SELECT 1 FROM message_folders AS mf
                WHERE mf.message_id = m.id AND mf.account_id = m.account_id
            )"
        }
        FolderScope::Unread => {
            "m.unread = 1 AND EXISTS (
                SELECT 1 FROM message_folders AS mf
                JOIN folders AS f ON f.id = mf.folder_id AND f.account_id = mf.account_id
                WHERE mf.message_id = m.id AND mf.account_id = m.account_id
                  AND f.role = 'inbox'
            )"
        }
        FolderScope::Sent => {
            "EXISTS (
                SELECT 1 FROM message_folders AS mf
                JOIN folders AS f ON f.id = mf.folder_id AND f.account_id = mf.account_id
                WHERE mf.message_id = m.id AND mf.account_id = m.account_id
                  AND f.role = 'sent'
            )"
        }
        FolderScope::Drafts => {
            "EXISTS (
                SELECT 1 FROM message_folders AS mf
                JOIN folders AS f ON f.id = mf.folder_id AND f.account_id = mf.account_id
                WHERE mf.message_id = m.id AND mf.account_id = m.account_id
                  AND f.role = 'drafts'
            )"
        }
        FolderScope::Archive => {
            "EXISTS (
                SELECT 1 FROM message_folders AS mf
                JOIN folders AS f ON f.id = mf.folder_id AND f.account_id = mf.account_id
                WHERE mf.message_id = m.id AND mf.account_id = m.account_id
                  AND f.role = 'archive'
            )"
        }
        FolderScope::Trash => {
            "EXISTS (
                SELECT 1 FROM message_folders AS mf
                JOIN folders AS f ON f.id = mf.folder_id AND f.account_id = mf.account_id
                WHERE mf.message_id = m.id AND mf.account_id = m.account_id
                  AND f.role = 'trash'
            )"
        }
    });

    if spec.folder != FolderScope::Trash {
        sql.push_str(
            " AND NOT EXISTS (
                SELECT 1 FROM message_folders AS trash_mf
                JOIN folders AS trash_f
                  ON trash_f.id = trash_mf.folder_id
                 AND trash_f.account_id = trash_mf.account_id
                WHERE trash_mf.message_id = m.id
                  AND trash_mf.account_id = m.account_id
                  AND trash_f.role = 'trash'
            )",
        );
    }

    if let Some(account_id) = spec.account.database_id() {
        parameters.push(Value::Integer(account_id));
        write!(sql, " AND m.account_id = ?{}", parameters.len())
            .expect("writing SQL to a String cannot fail");
    }

    if let Some(search) = spec.search.as_deref() {
        parameters.push(Value::Text(like_pattern(search)));
        let parameter = parameters.len();
        write!(
            sql,
            " AND (m.sender_name LIKE ?{parameter} ESCAPE '\\' COLLATE NOCASE \
             OR m.sender_address LIKE ?{parameter} ESCAPE '\\' COLLATE NOCASE \
             OR m.subject LIKE ?{parameter} ESCAPE '\\' COLLATE NOCASE \
             OR m.preview LIKE ?{parameter} ESCAPE '\\' COLLATE NOCASE)"
        )
        .expect("writing SQL to a String cannot fail");
    }

    if let PageBoundary::After(cursor) | PageBoundary::Before(cursor) = spec.boundary {
        parameters.push(Value::Integer(cursor.received_at_ms));
        let time_parameter = parameters.len();
        parameters.push(Value::Integer(cursor.message_id.get()));
        let id_parameter = parameters.len();
        let comparison = if matches!(spec.boundary, PageBoundary::Before(_)) {
            ">"
        } else {
            "<"
        };
        write!(
            sql,
            " AND (m.received_at_ms, m.id) {comparison} (?{time_parameter}, ?{id_parameter})"
        )
        .expect("writing SQL to a String cannot fail");
    }

    parameters.push(Value::Integer(i64::from(spec.limit) + 1));
    let order = if matches!(spec.boundary, PageBoundary::Before(_)) {
        "ASC"
    } else {
        "DESC"
    };
    write!(
        sql,
        " ORDER BY m.received_at_ms {order}, m.id {order} LIMIT ?{}",
        parameters.len(),
    )
    .expect("writing SQL to a String cannot fail");
    (sql, parameters)
}

pub(super) fn open_message(
    connection: &Connection,
    id: MessageId,
) -> Result<Option<MessageDetail>, DbFailure> {
    connection
        .query_row(OPEN_MESSAGE_SQL, [id.get()], |row| {
            let byte_count: i64 = row.get(11)?;
            Ok(MessageDetail {
                id: MessageId::from_database(row.get(0)?),
                account_id: row.get(1)?,
                sender_name: row.get::<_, String>(2)?.into_boxed_str(),
                sender_address: row.get::<_, String>(3)?.into_boxed_str(),
                subject: row.get::<_, String>(4)?.into_boxed_str(),
                received_at_ms: row.get(5)?,
                unread: row.get(6)?,
                starred: row.get(7)?,
                has_attachment: row.get(8)?,
                reader_excerpt: row.get::<_, String>(9)?.into_boxed_str(),
                body_truncated: row.get(10)?,
                body_byte_count: u64::try_from(byte_count).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(11, Type::Integer, Box::new(error))
                })?,
                body_file_key: row
                    .get::<_, Option<String>>(12)?
                    .map(String::into_boxed_str),
            })
        })
        .optional()
        .map_err(DbFailure::database)
}

fn like_pattern(search: &str) -> String {
    let mut pattern = String::with_capacity(search.len() + 2);
    pattern.push('%');
    for character in search.chars() {
        if matches!(character, '%' | '_' | '\\') {
            pattern.push('\\');
        }
        pattern.push(character);
    }
    pattern.push('%');
    pattern
}

#[cfg(test)]
mod tests {
    use rusqlite::{Connection, params};

    use super::*;
    use crate::store::sqlite::{
        domain::{AccountScope, FailureKind, FolderScope, PageSpec},
        migrations::migrate,
    };

    fn empty_connection() -> Connection {
        let mut connection = Connection::open_in_memory().unwrap();
        migrate(&mut connection).unwrap();
        connection
    }

    fn seeded_connection(count: i64) -> Connection {
        let connection = empty_connection();
        connection
            .execute(
                "INSERT INTO accounts
                 (id, provider, remote_key, name, address, state, accent_rgb)
                 VALUES (1, 'imap', 'account', 'Personal', 'user@example.test', 'active', 0)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO folders (id, account_id, remote_key, name, role)
                 VALUES (1, 1, 'inbox', 'Inbox', 'inbox')",
                [],
            )
            .unwrap();
        for id in 1..=count {
            connection
                .execute(
                    "INSERT INTO messages
                     (id, account_id, remote_key, sender_name, sender_address,
                      subject, preview, received_at_ms, unread, starred)
                     VALUES (?1, 1, ?2, 'Ada', 'ada@example.test', ?3, 'Preview', ?4, 1, 0)",
                    params![
                        id,
                        format!("message-{id}"),
                        format!("Status {id}"),
                        10_000 - id
                    ],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO message_folders (message_id, folder_id, account_id)
                     VALUES (?1, 1, 1)",
                    [id],
                )
                .unwrap();
        }
        super::super::stats::rebuild_account(&connection, 1).unwrap();
        connection
    }

    fn same_timestamp_connection(count: i64) -> Connection {
        let connection = seeded_connection(count);
        connection
            .execute("UPDATE messages SET received_at_ms = 10_000", [])
            .unwrap();
        super::super::stats::rebuild_account(&connection, 1).unwrap();
        connection
    }

    fn inbox_page(connection: &Connection, boundary: PageBoundary, limit: u8) -> MailboxPage {
        let spec =
            PageSpec::new(AccountScope::All, FolderScope::Inbox, None, boundary, limit).unwrap();
        query_mailbox(connection, &spec).unwrap()
    }

    fn message_ids(page: &MailboxPage) -> Vec<i64> {
        page.rows.iter().map(|row| row.id.get()).collect()
    }

    #[test]
    fn account_directory_is_stably_sorted_and_uses_only_persistent_stats() {
        let connection = empty_connection();
        connection
            .execute_batch(
                "INSERT INTO accounts
                     (id, provider, remote_key, name, address, sort_order, state, accent_rgb)
                 VALUES
                     (2, 'imap', 'two', 'Two', 'two@example.test', 10, 'active', 258),
                     (3, 'jmap', 'three', 'Three', 'three@example.test', 0, 'offline', 515),
                     (1, 'imap', 'one', 'One', 'one@example.test', 10, 'auth_required', 1);
                 UPDATE account_mailbox_stats
                 SET inbox_total = CASE account_id
                         WHEN 1 THEN 4 WHEN 2 THEN 6 ELSE 2 END,
                     inbox_unread = CASE account_id
                     WHEN 1 THEN 4 WHEN 2 THEN 6 ELSE 2 END;",
            )
            .unwrap();

        let directory = query_account_directory(&connection).unwrap();

        assert!(!ACCOUNT_DIRECTORY_SQL.contains("messages"));
        assert!(!ACCOUNT_DIRECTORY_SQL.contains("folders"));
        assert!(!ACCOUNT_DIRECTORY_SQL.contains("count("));
        assert_eq!(
            directory.rows.as_ref(),
            [
                AccountSummaryDto {
                    id: 3,
                    name: "Three".into(),
                    address: "three@example.test".into(),
                    state: "offline".into(),
                    accent_rgb: 515,
                    inbox_unread: 2,
                },
                AccountSummaryDto {
                    id: 1,
                    name: "One".into(),
                    address: "one@example.test".into(),
                    state: "auth_required".into(),
                    accent_rgb: 1,
                    inbox_unread: 4,
                },
                AccountSummaryDto {
                    id: 2,
                    name: "Two".into(),
                    address: "two@example.test".into(),
                    state: "active".into(),
                    accent_rgb: 258,
                    inbox_unread: 6,
                },
            ]
        );
    }

    #[test]
    fn account_directory_rejects_dirty_or_missing_statistics() {
        let connection = empty_connection();
        connection
            .execute(
                "INSERT INTO accounts
                     (id, provider, remote_key, name, address, state, accent_rgb)
                 VALUES (1, 'imap', 'one', 'One', 'one@example.test', 'active', 0)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "UPDATE account_mailbox_stats SET dirty = 1 WHERE account_id = 1",
                [],
            )
            .unwrap();

        let failure = query_account_directory(&connection).unwrap_err();
        assert_eq!(failure.kind, FailureKind::Conflict);

        connection
            .execute("DELETE FROM account_mailbox_stats WHERE account_id = 1", [])
            .unwrap();
        let failure = query_account_directory(&connection).unwrap_err();
        assert_eq!(failure.kind, FailureKind::Conflict);
    }

    #[test]
    fn account_directory_detects_legacy_overflow_with_one_extra_row() {
        let connection = empty_connection();
        connection
            .execute("DROP TRIGGER reject_account_limit_insert", [])
            .unwrap();
        for id in 1..=i64::try_from(MAX_ACCOUNT_STATS + 1).unwrap() {
            connection
                .execute(
                    "INSERT INTO accounts
                         (id, provider, remote_key, name, address, state, accent_rgb)
                     VALUES (?1, 'imap', ?2, 'Account', 'user@example.test', 'active', 0)",
                    params![id, format!("account-{id}")],
                )
                .unwrap();
        }

        let failure = query_account_directory(&connection).unwrap_err();

        assert_eq!(failure.kind, FailureKind::ResourceLimit);
        assert!(failure.message.contains("64-account limit"));
    }

    #[test]
    fn account_directory_accepts_the_exact_account_limit() {
        let connection = empty_connection();
        for id in 1..=i64::try_from(MAX_ACCOUNT_STATS).unwrap() {
            connection
                .execute(
                    "INSERT INTO accounts
                         (id, provider, remote_key, name, address, state, accent_rgb)
                     VALUES (?1, 'imap', ?2, 'Account', 'user@example.test', 'active', 0)",
                    params![id, format!("account-{id}")],
                )
                .unwrap();
        }

        let directory = query_account_directory(&connection).unwrap();

        assert_eq!(directory.rows.len(), MAX_ACCOUNT_STATS);
        assert_eq!(directory.rows.first().map(|account| account.id), Some(1));
        assert_eq!(
            directory.rows.last().map(|account| account.id),
            Some(i64::try_from(MAX_ACCOUNT_STATS).unwrap())
        );
    }

    #[test]
    fn keyset_pages_are_bounded_and_do_not_repeat_rows() {
        let connection = seeded_connection(51);
        let first_spec = PageSpec::new(
            AccountScope::All,
            FolderScope::Inbox,
            None,
            PageBoundary::First,
            50,
        )
        .unwrap();
        let first = query_mailbox(&connection, &first_spec).unwrap();
        assert_eq!(first.rows.len(), 50);
        assert_eq!(first.stats.selected_total, Some(51));
        assert_eq!(first.stats.inbox_unread, 51);
        assert_eq!(first.stats.account_unread.len(), 1);
        assert!(first.previous_cursor.is_none());
        let cursor = first.next_cursor.expect("second page cursor");

        let second_spec = PageSpec::new(
            AccountScope::All,
            FolderScope::Inbox,
            None,
            PageBoundary::After(cursor),
            50,
        )
        .unwrap();
        let second = query_mailbox(&connection, &second_spec).unwrap();
        assert_eq!(second.rows.len(), 1);
        assert_eq!(second.stats.selected_total, Some(51));
        assert!(second.previous_cursor.is_some());
        assert!(second.next_cursor.is_none());
        assert!(!first.rows.iter().any(|row| row.id == second.rows[0].id));
    }

    #[test]
    fn bidirectional_keyset_round_trip_handles_equal_timestamps() {
        let connection = same_timestamp_connection(151);
        let mut boundary = PageBoundary::First;
        let mut forward = Vec::new();

        let mut previous_cursor = loop {
            let page = inbox_page(&connection, boundary, 50);
            assert!(page.rows.len() <= 50);
            assert!(
                page.rows
                    .windows(2)
                    .all(|rows| rows[0].id.get() > rows[1].id.get())
            );
            if forward.is_empty() {
                assert!(page.previous_cursor.is_none());
            } else {
                assert!(page.previous_cursor.is_some());
            }

            let next_cursor = page.next_cursor;
            let page_previous_cursor = page.previous_cursor;
            forward.push(message_ids(&page));
            let Some(cursor) = next_cursor else {
                break page_previous_cursor;
            };
            boundary = PageBoundary::After(cursor);
        };

        assert_eq!(
            forward.iter().map(Vec::len).collect::<Vec<_>>(),
            [50, 50, 50, 1]
        );
        assert_eq!(
            forward.iter().flatten().copied().collect::<Vec<_>>(),
            (1_i64..=151).rev().collect::<Vec<_>>()
        );

        let mut backward = Vec::new();
        while let Some(cursor) = previous_cursor {
            let page = inbox_page(&connection, PageBoundary::Before(cursor), 50);
            assert!(page.rows.len() <= 50);
            assert!(page.next_cursor.is_some());
            previous_cursor = page.previous_cursor;
            backward.push(message_ids(&page));
        }

        assert_eq!(
            backward,
            forward[..forward.len() - 1]
                .iter()
                .rev()
                .cloned()
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn bidirectional_keyset_round_trip_handles_mixed_timestamps() {
        let connection = seeded_connection(121);
        let mut boundary = PageBoundary::First;
        let mut forward = Vec::new();

        let mut previous_cursor = loop {
            let page = inbox_page(&connection, boundary, 50);
            let next_cursor = page.next_cursor;
            let page_previous_cursor = page.previous_cursor;
            forward.push(message_ids(&page));
            let Some(cursor) = next_cursor else {
                break page_previous_cursor;
            };
            boundary = PageBoundary::After(cursor);
        };

        assert_eq!(
            forward.iter().map(Vec::len).collect::<Vec<_>>(),
            [50, 50, 21]
        );
        assert_eq!(
            forward.iter().flatten().copied().collect::<Vec<_>>(),
            (1_i64..=121).collect::<Vec<_>>()
        );

        let mut backward = Vec::new();
        while let Some(cursor) = previous_cursor {
            let page = inbox_page(&connection, PageBoundary::Before(cursor), 50);
            previous_cursor = page.previous_cursor;
            backward.push(message_ids(&page));
        }
        assert_eq!(
            backward,
            forward[..forward.len() - 1]
                .iter()
                .rev()
                .cloned()
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn underfull_first_page_has_no_navigation_cursors() {
        let connection = same_timestamp_connection(7);

        let page = inbox_page(&connection, PageBoundary::First, 50);

        assert_eq!(message_ids(&page), (1_i64..=7).rev().collect::<Vec<_>>());
        assert!(page.previous_cursor.is_none());
        assert!(page.next_cursor.is_none());
    }

    #[test]
    fn deleted_keyset_boundaries_remain_bounded_without_repeats() {
        let connection = same_timestamp_connection(101);
        let first = inbox_page(&connection, PageBoundary::First, 50);
        let first_ids = message_ids(&first);
        let forward_boundary = first.next_cursor.expect("second page cursor");
        connection
            .execute(
                "DELETE FROM messages WHERE id = ?1",
                [forward_boundary.message_id.get()],
            )
            .unwrap();
        super::super::stats::rebuild_account(&connection, 1).unwrap();

        let second = inbox_page(&connection, PageBoundary::After(forward_boundary), 50);
        let second_ids = message_ids(&second);
        assert_eq!(second.rows.len(), 50);
        assert!(second_ids.windows(2).all(|ids| ids[0] > ids[1]));
        assert!(!second_ids.iter().any(|id| first_ids.contains(id)));

        let backward_boundary = second.previous_cursor.expect("first page cursor");
        connection
            .execute(
                "DELETE FROM messages WHERE id = ?1",
                [backward_boundary.message_id.get()],
            )
            .unwrap();
        super::super::stats::rebuild_account(&connection, 1).unwrap();

        let previous = inbox_page(&connection, PageBoundary::Before(backward_boundary), 50);
        let previous_ids = message_ids(&previous);
        assert_eq!(previous.rows.len(), 49);
        assert!(previous_ids.windows(2).all(|ids| ids[0] > ids[1]));
        assert!(!previous_ids.iter().any(|id| second_ids.contains(id)));
        assert!(previous.previous_cursor.is_none());
        assert!(previous.next_cursor.is_some());
    }

    #[test]
    fn before_query_uses_ascending_scan_for_nearest_rows() {
        let spec = PageSpec::new(
            AccountScope::All,
            FolderScope::Inbox,
            None,
            PageBoundary::Before(PageCursor::new(10_000, 37).unwrap()),
            50,
        )
        .unwrap();

        let (sql, parameters) = mailbox_query(&spec);

        assert!(sql.contains("(m.received_at_ms, m.id) >"));
        assert!(sql.contains("ORDER BY m.received_at_ms ASC, m.id ASC"));
        assert_eq!(
            parameters,
            [
                Value::Integer(10_000),
                Value::Integer(37),
                Value::Integer(51)
            ]
        );
    }

    #[test]
    fn search_treats_like_metacharacters_as_text() {
        let connection = seeded_connection(1);
        connection
            .execute(
                "UPDATE messages
                 SET subject = 'Unrelated', preview = 'Budget 100%_final\\copy'",
                [],
            )
            .unwrap();
        let spec = PageSpec::new(
            AccountScope::All,
            FolderScope::Inbox,
            Some("%_final\\"),
            PageBoundary::First,
            50,
        )
        .unwrap();

        let page = query_mailbox(&connection, &spec).unwrap();

        assert_eq!(page.rows.len(), 1);
        assert_eq!(page.stats.selected_total, None);
    }

    #[test]
    fn message_detail_materializes_only_bounded_excerpt() {
        let connection = seeded_connection(1);
        connection
            .execute(
                "INSERT INTO message_content
                 (message_id, reader_excerpt, truncated, body_byte_count, body_file_key)
                 VALUES (1, 'Bounded body', 1, 70000, 'body/1.txt')",
                [],
            )
            .unwrap();

        let detail = open_message(&connection, MessageId::new(1).unwrap())
            .unwrap()
            .unwrap();

        assert_eq!(&*detail.reader_excerpt, "Bounded body");
        assert_eq!(detail.account_id, 1);
        assert!(detail.unread);
        assert!(!detail.starred);
        assert!(!detail.has_attachment);
        assert!(detail.body_truncated);
        assert_eq!(detail.body_byte_count, 70_000);
    }

    #[test]
    fn message_detail_carries_current_reader_actions_without_page_state() {
        let connection = seeded_connection(1);
        connection
            .execute(
                "UPDATE messages
                 SET unread = 0, starred = 1, has_attachment = 1
                 WHERE id = 1",
                [],
            )
            .unwrap();

        let detail = open_message(&connection, MessageId::new(1).unwrap())
            .unwrap()
            .unwrap();

        assert_eq!(detail.id, MessageId::new(1).unwrap());
        assert_eq!(detail.account_id, 1);
        assert!(!detail.unread);
        assert!(detail.starred);
        assert!(detail.has_attachment);
    }

    #[test]
    fn account_page_uses_bounded_metadata_index_without_count_or_body_join() {
        let connection = seeded_connection(1);
        let spec = PageSpec::new(
            AccountScope::account(1).unwrap(),
            FolderScope::Inbox,
            None,
            PageBoundary::First,
            50,
        )
        .unwrap();
        let (sql, parameters) = mailbox_query(&spec);
        assert!(!sql.contains("count("));
        assert!(!sql.contains("message_content"));

        let plan = connection
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap()
            .query_map(params_from_iter(parameters.iter()), |row| {
                row.get::<_, String>(3)
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert!(
            plan.iter()
                .any(|step| step.contains("idx_messages_account_time")),
            "unexpected query plan: {plan:?}"
        );
    }

    #[test]
    fn virtual_folders_keep_trash_out_of_starred_and_unread() {
        let connection = seeded_connection(1);
        connection
            .execute("UPDATE messages SET starred = 1 WHERE id = 1", [])
            .unwrap();
        connection
            .execute(
                "INSERT INTO folders (id, account_id, remote_key, name, role)
                 VALUES (2, 1, 'trash', 'Trash', 'trash'),
                        (3, 1, 'archive', 'Archive', 'archive')",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO message_folders (message_id, folder_id, account_id)
                 VALUES (1, 3, 1)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO messages
                 (id, account_id, remote_key, subject, received_at_ms, unread, starred)
                 VALUES (2, 1, 'trashed', 'Trashed', 20000, 1, 1)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO message_folders (message_id, folder_id, account_id)
                 VALUES (2, 1, 1), (2, 2, 1), (2, 3, 1)",
                [],
            )
            .unwrap();
        super::super::stats::rebuild_account(&connection, 1).unwrap();

        for folder in [FolderScope::Starred, FolderScope::Unread] {
            let spec =
                PageSpec::new(AccountScope::All, folder, None, PageBoundary::First, 50).unwrap();
            let page = query_mailbox(&connection, &spec).unwrap();
            assert_eq!(page.rows.len(), 1);
            assert_eq!(page.rows[0].id, MessageId::new(1).unwrap());
            assert_eq!(page.stats.selected_total, Some(1));
            assert_eq!(page.stats.inbox_unread, 1);
            assert_eq!(page.stats.starred_total, 1);
        }

        let trash = PageSpec::new(
            AccountScope::All,
            FolderScope::Trash,
            None,
            PageBoundary::First,
            50,
        )
        .unwrap();
        let page = query_mailbox(&connection, &trash).unwrap();
        assert_eq!(page.rows.len(), 1);
        assert_eq!(page.rows[0].id, MessageId::new(2).unwrap());
        assert_eq!(page.stats.selected_total, Some(1));
    }
}
