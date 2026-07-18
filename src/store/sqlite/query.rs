use std::fmt::Write;

use rusqlite::{
    Connection, OptionalExtension, params_from_iter,
    types::{Type, Value},
};

use super::domain::{
    DbFailure, FolderScope, MailSummaryDto, MailboxPage, MessageDetail, MessageId, PageCursor,
    PageSpec,
};
use super::stats::query_mailbox_stats;

const MAILBOX_SELECT: &str = "
    SELECT m.id, m.account_id, m.sender_name, m.sender_address, m.subject, m.preview,
           m.received_at_ms, m.unread, m.starred, m.has_attachment
      FROM messages AS m
     WHERE ";

const OPEN_MESSAGE_SQL: &str = "
    SELECT m.id, m.sender_name, m.sender_address, m.subject, m.received_at_ms,
           coalesce(c.reader_excerpt, ''), coalesce(c.truncated, 0),
           coalesce(c.body_byte_count, 0), c.body_file_key
      FROM messages AS m
      LEFT JOIN message_content AS c ON c.message_id = m.id
     WHERE m.id = ?1";

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
    let next_cursor = has_more
        .then(|| rows.last())
        .flatten()
        .map(|row| PageCursor {
            received_at_ms: row.received_at_ms,
            message_id: row.id,
        });

    Ok(MailboxPage {
        rows: rows.into_boxed_slice(),
        next_cursor,
        stats,
    })
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

    if let Some(cursor) = spec.after {
        parameters.push(Value::Integer(cursor.received_at_ms));
        let time_parameter = parameters.len();
        parameters.push(Value::Integer(cursor.message_id.get()));
        let id_parameter = parameters.len();
        write!(
            sql,
            " AND (m.received_at_ms, m.id) < (?{time_parameter}, ?{id_parameter})"
        )
        .expect("writing SQL to a String cannot fail");
    }

    parameters.push(Value::Integer(i64::from(spec.limit) + 1));
    write!(
        sql,
        " ORDER BY m.received_at_ms DESC, m.id DESC LIMIT ?{}",
        parameters.len()
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
            let byte_count: i64 = row.get(7)?;
            Ok(MessageDetail {
                id: MessageId::from_database(row.get(0)?),
                sender_name: row.get::<_, String>(1)?.into_boxed_str(),
                sender_address: row.get::<_, String>(2)?.into_boxed_str(),
                subject: row.get::<_, String>(3)?.into_boxed_str(),
                received_at_ms: row.get(4)?,
                reader_excerpt: row.get::<_, String>(5)?.into_boxed_str(),
                body_truncated: row.get(6)?,
                body_byte_count: u64::try_from(byte_count).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(7, Type::Integer, Box::new(error))
                })?,
                body_file_key: row.get::<_, Option<String>>(8)?.map(String::into_boxed_str),
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
        domain::{AccountScope, FolderScope, PageSpec},
        migrations::migrate,
    };

    fn seeded_connection(count: i64) -> Connection {
        let mut connection = Connection::open_in_memory().unwrap();
        migrate(&mut connection).unwrap();
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

    #[test]
    fn keyset_pages_are_bounded_and_do_not_repeat_rows() {
        let connection = seeded_connection(51);
        let first_spec =
            PageSpec::new(AccountScope::All, FolderScope::Inbox, None, None, 50).unwrap();
        let first = query_mailbox(&connection, &first_spec).unwrap();
        assert_eq!(first.rows.len(), 50);
        assert_eq!(first.stats.selected_total, Some(51));
        assert_eq!(first.stats.inbox_unread, 51);
        assert_eq!(first.stats.account_unread.len(), 1);
        let cursor = first.next_cursor.expect("second page cursor");

        let second_spec = PageSpec::new(
            AccountScope::All,
            FolderScope::Inbox,
            None,
            Some(cursor),
            50,
        )
        .unwrap();
        let second = query_mailbox(&connection, &second_spec).unwrap();
        assert_eq!(second.rows.len(), 1);
        assert_eq!(second.stats.selected_total, Some(51));
        assert!(second.next_cursor.is_none());
        assert!(!first.rows.iter().any(|row| row.id == second.rows[0].id));
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
            None,
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
        assert!(detail.body_truncated);
        assert_eq!(detail.body_byte_count, 70_000);
    }

    #[test]
    fn account_page_uses_bounded_metadata_index_without_count_or_body_join() {
        let connection = seeded_connection(1);
        let spec = PageSpec::new(
            AccountScope::account(1).unwrap(),
            FolderScope::Inbox,
            None,
            None,
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
            let spec = PageSpec::new(AccountScope::All, folder, None, None, 50).unwrap();
            let page = query_mailbox(&connection, &spec).unwrap();
            assert_eq!(page.rows.len(), 1);
            assert_eq!(page.rows[0].id, MessageId::new(1).unwrap());
            assert_eq!(page.stats.selected_total, Some(1));
            assert_eq!(page.stats.inbox_unread, 1);
            assert_eq!(page.stats.starred_total, 1);
        }

        let trash = PageSpec::new(AccountScope::All, FolderScope::Trash, None, None, 50).unwrap();
        let page = query_mailbox(&connection, &trash).unwrap();
        assert_eq!(page.rows.len(), 1);
        assert_eq!(page.rows[0].id, MessageId::new(2).unwrap());
        assert_eq!(page.stats.selected_total, Some(1));
    }
}
