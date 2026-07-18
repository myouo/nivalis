use std::{error::Error, fmt};

use rusqlite::{Connection, TransactionBehavior};

pub(crate) const LATEST_SCHEMA_VERSION: u32 = 6;

#[derive(Clone, Copy)]
struct Migration {
    version: u32,
    sql: &'static str,
}

const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        sql: include_str!("../../../migrations/0001_init.sql"),
    },
    Migration {
        version: 2,
        sql: include_str!("../../../migrations/0002_mail_mutations.sql"),
    },
    Migration {
        version: 3,
        sql: include_str!("../../../migrations/0003_file_reference_indexes.sql"),
    },
    Migration {
        version: 4,
        sql: include_str!("../../../migrations/0004_mutation_guards.sql"),
    },
    Migration {
        version: 5,
        sql: include_str!("../../../migrations/0005_clean_stale_file_gc.sql"),
    },
    Migration {
        version: 6,
        sql: include_str!("../../../migrations/0006_account_mailbox_stats.sql"),
    },
];

const _: () = {
    let mut index = 0;
    while index < MIGRATIONS.len() {
        assert!(MIGRATIONS[index].version == index as u32 + 1);
        index += 1;
    }
    assert!(MIGRATIONS.len() == LATEST_SCHEMA_VERSION as usize);
};

#[derive(Debug)]
pub(crate) enum MigrationError {
    Database(rusqlite::Error),
    ForeignKeysDisabled,
    InvalidSchemaVersion(i64),
    FutureSchema { found: u32, supported: u32 },
}

impl fmt::Display for MigrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => write!(formatter, "database migration failed: {error}"),
            Self::ForeignKeysDisabled => {
                formatter.write_str("SQLite foreign-key enforcement could not be enabled")
            }
            Self::InvalidSchemaVersion(version) => {
                write!(formatter, "invalid SQLite schema version {version}")
            }
            Self::FutureSchema { found, supported } => write!(
                formatter,
                "SQLite schema version {found} is newer than supported version {supported}"
            ),
        }
    }
}

impl Error for MigrationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::ForeignKeysDisabled
            | Self::InvalidSchemaVersion(_)
            | Self::FutureSchema { .. } => None,
        }
    }
}

impl From<rusqlite::Error> for MigrationError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error)
    }
}

pub(crate) fn migrate(connection: &mut Connection) -> Result<(), MigrationError> {
    enable_foreign_keys(connection)?;
    migrate_with(connection, MIGRATIONS, LATEST_SCHEMA_VERSION)
}

fn enable_foreign_keys(connection: &Connection) -> Result<(), MigrationError> {
    connection.pragma_update(None, "foreign_keys", true)?;

    let enabled: i64 = connection.pragma_query_value(None, "foreign_keys", |row| row.get(0))?;
    if enabled == 1 {
        Ok(())
    } else {
        Err(MigrationError::ForeignKeysDisabled)
    }
}

fn migrate_with(
    connection: &mut Connection,
    migrations: &[Migration],
    supported_version: u32,
) -> Result<(), MigrationError> {
    let raw_version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let current_version = u32::try_from(raw_version)
        .map_err(|_| MigrationError::InvalidSchemaVersion(raw_version))?;

    if current_version > supported_version {
        return Err(MigrationError::FutureSchema {
            found: current_version,
            supported: supported_version,
        });
    }

    for migration in migrations
        .iter()
        .filter(|migration| migration.version > current_version)
    {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(migration.sql)?;
        transaction.pragma_update(None, "user_version", migration.version)?;
        transaction.commit()?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use rusqlite::{Connection, ErrorCode, params};

    use super::{
        LATEST_SCHEMA_VERSION, MIGRATIONS, Migration, MigrationError, enable_foreign_keys, migrate,
        migrate_with,
    };

    const TABLES: &[&str] = &[
        "account_mailbox_stats",
        "accounts",
        "attachments",
        "file_gc",
        "folders",
        "message_content",
        "message_folders",
        "message_tombstones",
        "messages",
        "outbox",
        "outbox_recipients",
        "sync_state",
        "trash_undo",
        "trash_undo_folders",
    ];

    const MAILBOX_STATS_COLUMNS: &[&str] = &[
        "inbox_total",
        "inbox_unread",
        "starred_total",
        "sent_total",
        "drafts_total",
        "archive_total",
        "trash_total",
        "dirty",
    ];

    fn memory_connection() -> Connection {
        Connection::open_in_memory().expect("open in-memory SQLite database")
    }

    fn schema_version(connection: &Connection) -> i64 {
        connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("read schema version")
    }

    fn insert_account(connection: &Connection, id: i64, remote_key: &str) {
        connection
            .execute(
                "INSERT INTO accounts
                 (id, provider, remote_key, name, address, sort_order, state, accent_rgb)
                 VALUES (?1, 'imap', ?2, 'Test', ?2, 0, 'active', 0)",
                params![id, remote_key],
            )
            .expect("insert account");
    }

    fn insert_folder(
        connection: &Connection,
        id: i64,
        account_id: i64,
        remote_key: &str,
        role: &str,
    ) {
        connection
            .execute(
                "INSERT INTO folders (id, account_id, remote_key, name, role)
                 VALUES (?1, ?2, ?3, 'Folder', ?4)",
                params![id, account_id, remote_key, role],
            )
            .expect("insert folder");
    }

    fn insert_message(connection: &Connection, id: i64, account_id: i64, remote_key: &str) {
        connection
            .execute(
                "INSERT INTO messages (id, account_id, remote_key, received_at_ms)
                 VALUES (?1, ?2, ?3, 0)",
                params![id, account_id, remote_key],
            )
            .expect("insert message");
    }

    fn mailbox_stats(connection: &Connection, account_id: i64) -> [i64; 7] {
        connection
            .query_row(
                "SELECT inbox_total, inbox_unread, starred_total, sent_total,
                        drafts_total, archive_total, trash_total
                 FROM account_mailbox_stats
                 WHERE account_id = ?1",
                [account_id],
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
            .expect("read account mailbox statistics")
    }

    #[test]
    fn creates_expected_schema_and_indexes() {
        let mut connection = memory_connection();

        migrate(&mut connection).expect("apply initial migration");

        assert_eq!(
            schema_version(&connection),
            i64::from(LATEST_SCHEMA_VERSION)
        );
        let foreign_keys: i64 = connection
            .pragma_query_value(None, "foreign_keys", |row| row.get(0))
            .expect("read foreign-key setting");
        assert_eq!(foreign_keys, 1);

        let tables = connection
            .prepare(
                "SELECT name FROM sqlite_schema
                 WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
                 ORDER BY name",
            )
            .expect("prepare table query")
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query tables")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect tables");
        assert_eq!(tables, TABLES);

        let stats_columns = connection
            .prepare("PRAGMA table_info(account_mailbox_stats)")
            .expect("prepare mailbox stats table-info query")
            .query_map([], |row| row.get::<_, String>(1))
            .expect("query mailbox stats columns")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect mailbox stats columns");
        assert_eq!(
            stats_columns,
            std::iter::once("account_id")
                .chain(MAILBOX_STATS_COLUMNS.iter().copied())
                .collect::<Vec<_>>()
        );

        let indexes = connection
            .prepare(
                "SELECT name FROM sqlite_schema
                 WHERE type = 'index' AND name LIKE 'idx_%'",
            )
            .expect("prepare index query")
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query indexes")
            .collect::<Result<BTreeSet<_>, _>>()
            .expect("collect indexes");
        assert_eq!(
            indexes,
            BTreeSet::from([
                "idx_folders_account".to_owned(),
                "idx_folders_system_role".to_owned(),
                "idx_file_gc_queued".to_owned(),
                "idx_attachments_file".to_owned(),
                "idx_message_folders_folder".to_owned(),
                "idx_message_content_body_file".to_owned(),
                "idx_message_tombstones_deleted".to_owned(),
                "idx_messages_account_time".to_owned(),
                "idx_messages_global_time".to_owned(),
                "idx_messages_starred".to_owned(),
                "idx_messages_unread".to_owned(),
                "idx_outbox_mime_file".to_owned(),
                "idx_outbox_pending".to_owned(),
            ])
        );
    }

    #[test]
    fn migration_is_idempotent_and_preserves_data() {
        let mut connection = memory_connection();
        migrate(&mut connection).expect("apply initial migration");
        insert_account(&connection, 1, "user@example.test");
        insert_folder(&connection, 10, 1, "inbox", "inbox");
        insert_message(&connection, 100, 1, "message-1");
        connection
            .execute(
                "INSERT INTO message_folders (message_id, folder_id, account_id)
                 VALUES (100, 10, 1)",
                [],
            )
            .expect("insert folder membership");
        connection
            .execute(
                "INSERT INTO outbox
                 (message_id, mime_file_key, envelope_from, wire_byte_count, state)
                 VALUES (100, 'mime/100.eml', 'user@example.test', 256, 'pending')",
                [],
            )
            .expect("insert outbox item");
        connection
            .execute(
                "INSERT INTO outbox_recipients
                 (message_id, kind, ordinal, address, display_name)
                 VALUES (100, 'to', 0, 'recipient@example.test', 'Recipient')",
                [],
            )
            .expect("insert outbox recipient");

        migrate(&mut connection).expect("run migration again");

        let recipient_count: i64 = connection
            .query_row("SELECT count(*) FROM outbox_recipients", [], |row| {
                row.get(0)
            })
            .expect("count outbox recipients");
        assert_eq!(recipient_count, 1);
        assert_eq!(
            schema_version(&connection),
            i64::from(LATEST_SCHEMA_VERSION)
        );
    }

    #[test]
    fn upgrades_v1_data_and_enforces_unique_system_roles() {
        let mut connection = memory_connection();
        enable_foreign_keys(&connection).expect("enable foreign keys");
        migrate_with(&mut connection, &MIGRATIONS[..1], 1).expect("create v1 schema");
        insert_account(&connection, 1, "user@example.test");
        insert_folder(&connection, 10, 1, "inbox", "inbox");
        insert_folder(&connection, 11, 1, "label-a", "label");
        insert_folder(&connection, 12, 1, "inbox-legacy", "inbox");
        insert_message(&connection, 100, 1, "message-1");
        connection
            .execute(
                "INSERT INTO message_folders (message_id, folder_id, account_id)
                 VALUES (100, 10, 1), (100, 11, 1)",
                [],
            )
            .expect("seed v1 memberships");

        migrate(&mut connection).expect("upgrade v1 schema");

        let membership_count: i64 = connection
            .query_row("SELECT count(*) FROM message_folders", [], |row| row.get(0))
            .expect("count preserved memberships");
        assert_eq!(membership_count, 2);
        assert_eq!(
            schema_version(&connection),
            i64::from(LATEST_SCHEMA_VERSION)
        );

        let legacy_inbox_count: i64 = connection
            .query_row(
                "SELECT count(*) FROM folders WHERE account_id = 1 AND role = 'inbox'",
                [],
                |row| row.get(0),
            )
            .expect("count preserved legacy system folders");
        assert_eq!(legacy_inbox_count, 2);

        connection
            .execute(
                "INSERT INTO folders (id, account_id, remote_key, name, role)
                 VALUES (10, 1, 'inbox', 'Refreshed Inbox', 'inbox')
                 ON CONFLICT (account_id, remote_key) DO UPDATE
                 SET name = excluded.name, role = excluded.role",
                [],
            )
            .expect("an existing system folder can be refreshed with UPSERT");
        let inbox_name: String = connection
            .query_row("SELECT name FROM folders WHERE id = 10", [], |row| {
                row.get(0)
            })
            .expect("read refreshed folder");
        assert_eq!(inbox_name, "Refreshed Inbox");

        let duplicate_system_role = connection
            .execute(
                "INSERT INTO folders (id, account_id, remote_key, name, role)
                 VALUES (13, 1, 'inbox-2', 'Inbox duplicate', 'inbox')",
                [],
            )
            .expect_err("system roles must be unique per account");
        assert_eq!(
            duplicate_system_role.sqlite_error_code(),
            Some(ErrorCode::ConstraintViolation)
        );
        let replace_label_as_inbox = connection
            .execute(
                "INSERT OR REPLACE INTO folders (id, account_id, remote_key, name, role)
                 VALUES (11, 1, 'label-a', 'Disguised Inbox', 'inbox')",
                [],
            )
            .expect_err("REPLACE cannot turn an existing label into a duplicate Inbox");
        assert_eq!(
            replace_label_as_inbox.sqlite_error_code(),
            Some(ErrorCode::ConstraintViolation)
        );
        let preserved_role: String = connection
            .query_row("SELECT role FROM folders WHERE id = 11", [], |row| {
                row.get(0)
            })
            .expect("read preserved label role");
        assert_eq!(preserved_role, "label");
        connection
            .execute(
                "INSERT INTO folders (id, account_id, remote_key, name, role)
                 VALUES (14, 1, 'label-b', 'Second label', 'label')",
                [],
            )
            .expect("custom roles may repeat");
    }

    #[test]
    fn v6_backfills_deduplicated_stats_with_trash_precedence() {
        let mut connection = memory_connection();
        enable_foreign_keys(&connection).expect("enable foreign keys");
        migrate_with(&mut connection, &MIGRATIONS[..1], 1).expect("create v1 schema");
        insert_account(&connection, 1, "one@example.test");
        insert_account(&connection, 2, "two@example.test");

        for (id, remote_key, role) in [
            (10, "inbox", "inbox"),
            (11, "legacy-inbox", "inbox"),
            (12, "archive", "archive"),
            (13, "trash", "trash"),
            (14, "sent", "sent"),
            (15, "drafts", "drafts"),
            (16, "label", "label"),
        ] {
            insert_folder(&connection, id, 1, remote_key, role);
        }
        connection
            .execute_batch(
                "INSERT INTO messages
                     (id, account_id, remote_key, received_at_ms, unread, starred)
                 VALUES
                     (100, 1, 'inbox-starred', 0, 1, 1),
                     (101, 1, 'archive-starred', 0, 0, 1),
                     (102, 1, 'inbox-and-trash', 0, 1, 1),
                     (103, 1, 'sent', 0, 0, 0),
                     (104, 1, 'draft', 0, 0, 0),
                     (105, 1, 'archive-and-trash', 0, 0, 0),
                     (106, 1, 'unfiled-starred', 0, 1, 1);

                 INSERT INTO message_folders (message_id, folder_id, account_id)
                 VALUES
                     (100, 10, 1),
                     (100, 11, 1),
                     (100, 16, 1),
                     (101, 12, 1),
                     (102, 10, 1),
                     (102, 13, 1),
                     (103, 14, 1),
                     (104, 15, 1),
                     (105, 12, 1),
                     (105, 13, 1);",
            )
            .expect("seed v1 mailbox data");

        migrate_with(&mut connection, &MIGRATIONS[..5], 5).expect("upgrade data to v5");
        assert_eq!(schema_version(&connection), 5);

        migrate(&mut connection).expect("upgrade data to v6");

        assert_eq!(schema_version(&connection), 6);
        assert_eq!(mailbox_stats(&connection, 1), [1, 1, 2, 1, 1, 1, 2]);
        assert_eq!(mailbox_stats(&connection, 2), [0; 7]);
        let legacy_inbox_count: i64 = connection
            .query_row(
                "SELECT count(*) FROM folders
                 WHERE account_id = 1 AND role = 'inbox'",
                [],
                |row| row.get(0),
            )
            .expect("count legacy Inbox folders");
        assert_eq!(legacy_inbox_count, 2);
    }

    #[test]
    fn account_stats_follow_account_lifecycle_and_enforce_count_bounds() {
        let mut connection = memory_connection();
        migrate(&mut connection).expect("apply migrations");

        insert_account(&connection, 1, "one@example.test");
        assert_eq!(mailbox_stats(&connection, 1), [0; 7]);

        connection
            .execute(
                "UPDATE account_mailbox_stats
                 SET inbox_total = ?1,
                     inbox_unread = ?1,
                     starred_total = ?1,
                     sent_total = ?1,
                     drafts_total = ?1,
                     archive_total = ?1,
                     trash_total = ?1
                 WHERE account_id = 1",
                [i64::MAX],
            )
            .expect("the maximum signed SQLite count is valid");
        assert_eq!(mailbox_stats(&connection, 1), [i64::MAX; 7]);

        for column in MAILBOX_STATS_COLUMNS {
            let error = connection
                .execute(
                    &format!("UPDATE account_mailbox_stats SET {column} = -1 WHERE account_id = 1"),
                    [],
                )
                .expect_err("mailbox counts cannot be negative");
            assert_eq!(
                error.sqlite_error_code(),
                Some(ErrorCode::ConstraintViolation)
            );
        }

        connection
            .execute("DELETE FROM accounts WHERE id = 1", [])
            .expect("delete account");
        let remaining_stats: i64 = connection
            .query_row("SELECT count(*) FROM account_mailbox_stats", [], |row| {
                row.get(0)
            })
            .expect("count remaining mailbox stats rows");
        assert_eq!(remaining_stats, 0);
    }

    #[test]
    fn account_limit_is_enforced_without_blocking_existing_account_updates() {
        let mut connection = memory_connection();
        migrate(&mut connection).expect("apply migrations");

        for id in 1..=64 {
            insert_account(&connection, id, &format!("account-{id}"));
        }

        let error = connection
            .execute(
                "INSERT INTO accounts
                 (id, provider, remote_key, name, address, state, accent_rgb)
                 VALUES (65, 'imap', 'account-65', 'Too many', 'account-65', 'active', 0)",
                [],
            )
            .expect_err("the sixty-fifth account must be rejected");
        assert_eq!(
            error.sqlite_error_code(),
            Some(ErrorCode::ConstraintViolation)
        );

        connection
            .execute(
                "INSERT INTO accounts
                 (id, provider, remote_key, name, address, state, accent_rgb)
                 VALUES (64, 'imap', 'account-64', 'Updated', 'account-64', 'active', 0)
                 ON CONFLICT(id) DO UPDATE SET name = excluded.name",
                [],
            )
            .expect("upsert an existing account at the limit");
        let name: String = connection
            .query_row("SELECT name FROM accounts WHERE id = 64", [], |row| {
                row.get(0)
            })
            .expect("read updated account");
        assert_eq!(name, "Updated");
    }

    #[test]
    fn rejects_schema_from_a_newer_application() {
        let mut connection = memory_connection();
        connection
            .pragma_update(None, "user_version", LATEST_SCHEMA_VERSION + 1)
            .expect("set future schema version");

        let error = migrate(&mut connection).expect_err("reject future schema");

        assert!(matches!(
            error,
            MigrationError::FutureSchema {
                found: 7,
                supported: LATEST_SCHEMA_VERSION,
            }
        ));
    }

    #[test]
    fn failed_migration_rolls_back_schema_and_version() {
        const BROKEN: &[Migration] = &[Migration {
            version: 1,
            sql: "CREATE TABLE should_rollback (id INTEGER PRIMARY KEY) STRICT;
                  INSERT INTO table_that_does_not_exist VALUES (1);",
        }];

        let mut connection = memory_connection();
        enable_foreign_keys(&connection).expect("enable foreign keys");

        let error = migrate_with(&mut connection, BROKEN, 1).expect_err("migration must fail");

        assert!(matches!(error, MigrationError::Database(_)));
        assert_eq!(schema_version(&connection), 0);
        let table_count: i64 = connection
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name = 'should_rollback'",
                [],
                |row| row.get(0),
            )
            .expect("check rolled-back table");
        assert_eq!(table_count, 0);
    }

    #[test]
    fn supports_multi_folder_membership_and_enforces_account_ownership() {
        let mut connection = memory_connection();
        migrate(&mut connection).expect("apply initial migration");
        insert_account(&connection, 1, "one@example.test");
        insert_account(&connection, 2, "two@example.test");
        insert_folder(&connection, 10, 1, "inbox", "inbox");
        insert_folder(&connection, 11, 1, "important", "label");
        insert_folder(&connection, 20, 2, "inbox", "inbox");
        insert_message(&connection, 100, 1, "message-1");
        insert_message(&connection, 200, 2, "message-1");

        let duplicate_message = connection
            .execute(
                "INSERT INTO messages (id, account_id, remote_key, received_at_ms)
                 VALUES (101, 1, 'message-1', 0)",
                [],
            )
            .expect_err("remote message keys are unique within an account");
        assert_eq!(
            duplicate_message.sqlite_error_code(),
            Some(ErrorCode::ConstraintViolation)
        );

        connection
            .execute(
                "INSERT INTO message_folders (message_id, folder_id, account_id)
                 VALUES (100, 10, 1), (100, 11, 1)",
                [],
            )
            .expect("put one message in multiple folders");
        let membership_count: i64 = connection
            .query_row(
                "SELECT count(*) FROM message_folders WHERE message_id = 100",
                [],
                |row| row.get(0),
            )
            .expect("count folder memberships");
        assert_eq!(membership_count, 2);

        let folder_error = connection
            .execute(
                "INSERT INTO message_folders (message_id, folder_id, account_id)
                 VALUES (100, 20, 1)",
                [],
            )
            .expect_err("folder cannot contain another account's message");
        assert_eq!(
            folder_error.sqlite_error_code(),
            Some(ErrorCode::ConstraintViolation)
        );

        let message_error = connection
            .execute(
                "INSERT INTO message_folders (message_id, folder_id, account_id)
                 VALUES (100, 20, 2)",
                [],
            )
            .expect_err("membership account must own the message");
        assert_eq!(
            message_error.sqlite_error_code(),
            Some(ErrorCode::ConstraintViolation)
        );

        connection
            .execute("DELETE FROM accounts WHERE id = 1", [])
            .expect("delete account");
        let folder_count: i64 = connection
            .query_row(
                "SELECT count(*) FROM folders WHERE account_id = 1",
                [],
                |row| row.get(0),
            )
            .expect("count folders");
        assert_eq!(folder_count, 0);
        let membership_count: i64 = connection
            .query_row("SELECT count(*) FROM message_folders", [], |row| row.get(0))
            .expect("count remaining memberships");
        assert_eq!(membership_count, 0);
    }

    #[test]
    fn stores_file_references_without_blob_columns() {
        let mut connection = memory_connection();
        migrate(&mut connection).expect("apply initial migration");

        for table in TABLES {
            let declared_types = connection
                .prepare(&format!("PRAGMA table_info({table})"))
                .expect("prepare table-info query")
                .query_map([], |row| row.get::<_, String>(2))
                .expect("query columns")
                .collect::<Result<Vec<_>, _>>()
                .expect("collect declared types");
            assert!(
                declared_types.iter().all(|kind| kind != "BLOB"),
                "{table} contains a BLOB column"
            );
        }

        let message_columns = connection
            .prepare("PRAGMA table_info(messages)")
            .expect("prepare message column query")
            .query_map([], |row| row.get::<_, String>(1))
            .expect("query message columns")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect message columns");
        assert!(!message_columns.iter().any(|name| name.contains("body")));

        let outbox_columns = connection
            .prepare("PRAGMA table_info(outbox)")
            .expect("prepare outbox column query")
            .query_map([], |row| row.get::<_, String>(1))
            .expect("query outbox columns")
            .collect::<Result<BTreeSet<_>, _>>()
            .expect("collect outbox columns");
        assert!(outbox_columns.contains("mime_file_key"));
        assert!(outbox_columns.contains("envelope_from"));
        assert!(outbox_columns.contains("wire_byte_count"));
        assert!(!outbox_columns.contains("mime"));
    }

    #[test]
    fn live_file_references_remove_stale_gc_entries_transactionally() {
        let mut connection = memory_connection();
        migrate(&mut connection).expect("apply migrations");
        insert_account(&connection, 1, "sender@example.test");
        insert_message(&connection, 100, 1, "message-1");
        connection
            .execute(
                "INSERT INTO file_gc (file_key, queued_at_ms)
                 VALUES ('body.eml', 0), ('attachment.bin', 0), ('outbox.eml', 0)",
                [],
            )
            .expect("seed stale GC entries");

        connection
            .execute(
                "INSERT INTO message_content (message_id, body_file_key)
                 VALUES (100, 'body.eml')",
                [],
            )
            .expect("reference body file");
        connection
            .execute(
                "INSERT INTO attachments (id, message_id, ordinal, file_key)
                 VALUES (1, 100, 0, 'attachment.bin')",
                [],
            )
            .expect("reference attachment file");
        connection
            .execute(
                "INSERT INTO outbox
                 (message_id, mime_file_key, envelope_from, wire_byte_count, state)
                 VALUES (100, 'outbox.eml', 'sender@example.test', 1, 'pending')",
                [],
            )
            .expect("reference Outbox file");

        let queued_count: i64 = connection
            .query_row("SELECT count(*) FROM file_gc", [], |row| row.get(0))
            .expect("count remaining GC entries");
        assert_eq!(queued_count, 0);
    }

    #[test]
    fn v5_upgrade_removes_preexisting_gc_entries_for_live_files() {
        let mut connection = memory_connection();
        enable_foreign_keys(&connection).expect("enable foreign keys");
        migrate_with(&mut connection, &MIGRATIONS[..4], 4).expect("create v4 schema");
        insert_account(&connection, 1, "sender@example.test");
        insert_message(&connection, 100, 1, "message-1");
        connection
            .execute(
                "INSERT INTO message_content (message_id, body_file_key)
                 VALUES (100, 'live.eml')",
                [],
            )
            .expect("reference live file");
        connection
            .execute(
                "INSERT INTO file_gc (file_key, queued_at_ms)
                 VALUES ('live.eml', 0), ('orphan.eml', 0)",
                [],
            )
            .expect("seed v4 GC queue");

        migrate_with(&mut connection, &MIGRATIONS[..5], 5).expect("upgrade to v5");

        let queued = connection
            .prepare("SELECT file_key FROM file_gc ORDER BY file_key")
            .expect("prepare GC query")
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query GC entries")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect GC entries");
        assert_eq!(queued, ["orphan.eml"]);
        assert_eq!(schema_version(&connection), 5);
    }

    #[test]
    fn persists_bounded_smtp_source_and_recipients() {
        let mut connection = memory_connection();
        migrate(&mut connection).expect("apply initial migration");
        insert_account(&connection, 1, "sender@example.test");
        insert_message(&connection, 100, 1, "draft-1");

        let missing_source = connection
            .execute(
                "INSERT INTO outbox
                 (message_id, mime_file_key, envelope_from, wire_byte_count, state)
                 VALUES (100, '', 'sender@example.test', 128, 'pending')",
                [],
            )
            .expect_err("MIME file key must not be empty");
        assert_eq!(
            missing_source.sqlite_error_code(),
            Some(ErrorCode::ConstraintViolation)
        );

        connection
            .execute(
                "INSERT INTO outbox
                 (message_id, mime_file_key, envelope_from, wire_byte_count, state)
                 VALUES (100, 'mime/100.eml', 'sender@example.test', 128, 'pending')",
                [],
            )
            .expect("insert durable outbox source");
        connection
            .execute(
                "INSERT INTO outbox_recipients
                 (message_id, kind, ordinal, address, display_name)
                 VALUES (100, 'to', 0, 'recipient@example.test', 'Recipient')",
                [],
            )
            .expect("insert outbox recipient");

        for (kind, ordinal, address, display_name) in [
            ("reply-to", 1_i64, "reply@example.test", "Reply".to_owned()),
            ("cc", 65_536, "cc@example.test", "Copy".to_owned()),
            ("bcc", 1, "", "Blind copy".to_owned()),
            ("to", 1, "second@example.test", "x".repeat(321)),
        ] {
            let error = connection
                .execute(
                    "INSERT INTO outbox_recipients
                     (message_id, kind, ordinal, address, display_name)
                     VALUES (100, ?1, ?2, ?3, ?4)",
                    params![kind, ordinal, address, display_name],
                )
                .expect_err("reject invalid outbox recipient");
            assert_eq!(
                error.sqlite_error_code(),
                Some(ErrorCode::ConstraintViolation)
            );
        }

        connection
            .execute("DELETE FROM outbox WHERE message_id = 100", [])
            .expect("delete outbox item");
        let recipient_count: i64 = connection
            .query_row("SELECT count(*) FROM outbox_recipients", [], |row| {
                row.get(0)
            })
            .expect("count remaining recipients");
        assert_eq!(recipient_count, 0);
    }

    #[test]
    fn enforces_utf8_byte_limits() {
        let mut connection = memory_connection();
        migrate(&mut connection).expect("apply initial migration");
        let provider = "邮".repeat(22);

        let error = connection
            .execute(
                "INSERT INTO accounts
                 (provider, remote_key, name, address, state, accent_rgb)
                 VALUES (?1, 'remote', 'Name', 'user@example.test', 'active', 0)",
                [provider],
            )
            .expect_err("provider exceeds 64 UTF-8 bytes");

        assert_eq!(
            error.sqlite_error_code(),
            Some(ErrorCode::ConstraintViolation)
        );
    }

    #[test]
    fn rejects_non_positive_entity_ids() {
        let mut connection = memory_connection();
        migrate(&mut connection).expect("apply initial migration");

        let error = connection
            .execute(
                "INSERT INTO accounts
                 (id, provider, remote_key, name, address, state, accent_rgb)
                 VALUES (-1, 'imap', 'remote', 'Name', 'user@example.test', 'active', 0)",
                [],
            )
            .expect_err("entity ids must be positive");

        assert_eq!(
            error.sqlite_error_code(),
            Some(ErrorCode::ConstraintViolation)
        );
    }
}
