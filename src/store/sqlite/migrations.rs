use std::{error::Error, fmt};

use rusqlite::{Connection, TransactionBehavior};

pub(crate) const LATEST_SCHEMA_VERSION: u32 = 9;

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
    Migration {
        version: 7,
        sql: include_str!("../../../migrations/0007_remote_change_journal.sql"),
    },
    Migration {
        version: 8,
        sql: include_str!("../../../migrations/0008_remote_lease_reservations.sql"),
    },
    Migration {
        version: 9,
        sql: include_str!("../../../migrations/0009_message_search.sql"),
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
    RecursiveTriggersDisabled,
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
            Self::RecursiveTriggersDisabled => {
                formatter.write_str("SQLite recursive-trigger enforcement could not be enabled")
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
            | Self::RecursiveTriggersDisabled
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
    enable_recursive_triggers(connection)?;
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

fn enable_recursive_triggers(connection: &Connection) -> Result<(), MigrationError> {
    connection.pragma_update(None, "recursive_triggers", true)?;

    let enabled: i64 =
        connection.pragma_query_value(None, "recursive_triggers", |row| row.get(0))?;
    if enabled == 1 {
        Ok(())
    } else {
        Err(MigrationError::RecursiveTriggersDisabled)
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

    use crate::store::sqlite::{
        domain::{AccountScope, FolderScope, PageBoundary, PageSpec},
        query::query_mailbox,
    };

    use super::{
        LATEST_SCHEMA_VERSION, MIGRATIONS, Migration, MigrationError, enable_foreign_keys,
        enable_recursive_triggers, migrate, migrate_with,
    };

    const MEMORY_FIXTURE_SQL: &str = include_str!("../../../scripts/fixtures/memory.sql");

    const TABLES: &[&str] = &[
        "account_mailbox_stats",
        "account_object_states",
        "accounts",
        "attachments",
        "file_gc",
        "folders",
        "imap_message_locations",
        "message_content",
        "message_folders",
        "message_tombstone_imap_locations",
        "message_tombstones",
        "messages",
        "outbox",
        "outbox_recipients",
        "remote_account_reconciliations",
        "remote_change_intent_folders",
        "remote_change_intent_imap_sources",
        "remote_change_intents",
        "remote_journal_usage",
        "sync_state",
        "trash_undo",
        "trash_undo_folders",
    ];

    const FTS_TABLES: &[&str] = &[
        "message_search",
        "message_search_config",
        "message_search_data",
        "message_search_idx",
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
        let recursive_triggers: i64 = connection
            .pragma_query_value(None, "recursive_triggers", |row| row.get(0))
            .expect("read recursive-trigger setting");
        assert_eq!(recursive_triggers, 1);

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
        let mut expected_tables = TABLES.iter().chain(FTS_TABLES).copied().collect::<Vec<_>>();
        expected_tables.sort_unstable();
        assert_eq!(tables, expected_tables);

        let fts5_enabled: bool = connection
            .query_row(
                "SELECT sqlite_compileoption_used('ENABLE_FTS5')",
                [],
                |row| row.get(0),
            )
            .expect("read bundled SQLite FTS5 support");
        assert!(fts5_enabled);

        let message_search_sql: String = connection
            .query_row(
                "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = 'message_search'",
                [],
                |row| row.get(0),
            )
            .expect("read message search schema");
        assert!(message_search_sql.contains("content = 'messages'"));
        assert!(message_search_sql.contains("content_rowid = 'id'"));
        assert!(message_search_sql.contains("columnsize = 0"));

        let search_triggers: i64 = connection
            .query_row(
                "SELECT count(*) FROM sqlite_schema
                 WHERE type = 'trigger' AND name LIKE 'sync_message_search_%'",
                [],
                |row| row.get(0),
            )
            .expect("count message search triggers");
        assert_eq!(search_triggers, 3);

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
                "idx_messages_legacy_reconcile_pending".to_owned(),
                "idx_messages_starred".to_owned(),
                "idx_messages_unread".to_owned(),
                "idx_outbox_mime_file".to_owned(),
                "idx_outbox_pending".to_owned(),
                "idx_remote_intents_account_due".to_owned(),
                "idx_remote_intents_global_due".to_owned(),
            ])
        );
    }

    #[test]
    fn memory_fixture_matches_the_current_schema_and_resource_bounds() {
        let mut connection = memory_connection();
        migrate(&mut connection).expect("apply current schema");

        connection
            .execute_batch(MEMORY_FIXTURE_SQL)
            .expect("seed bounded memory fixture");

        let counts = connection
            .query_row(
                "SELECT (SELECT count(*) FROM accounts),
                        (SELECT count(*) FROM folders),
                        (SELECT count(*) FROM messages),
                        (SELECT count(*) FROM message_content),
                        (SELECT count(*) FROM account_mailbox_stats WHERE dirty)",
                [],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                },
            )
            .expect("read fixture counts");
        assert_eq!(counts, (64, 64, 51, 51, 0));

        let bounds = connection
            .query_row(
                "SELECT min(length(CAST(preview AS BLOB))),
                        max(length(CAST(preview AS BLOB))),
                        min(length(CAST(reader_excerpt AS BLOB))),
                        max(length(CAST(reader_excerpt AS BLOB)))
                 FROM messages
                 JOIN message_content ON message_content.message_id = messages.id",
                [],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .expect("read fixture text bounds");
        assert_eq!(bounds, (2_048, 2_048, 65_536, 65_536));

        let search = connection
            .query_row(
                "SELECT count(*), min(rowid), max(rowid)
                   FROM message_search
                  WHERE message_search MATCH '\"message 51\"'",
                [],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .expect("query fixture FTS identity");
        assert_eq!(search, (1, 51, 51));

        let first_spec = PageSpec::new(
            AccountScope::All,
            FolderScope::Inbox,
            None,
            PageBoundary::First,
            50,
        )
        .unwrap();
        let first_page = query_mailbox(&connection, &first_spec).expect("query first fixture page");
        assert_eq!(first_page.rows.len(), 50);
        let next_cursor = first_page.next_cursor.expect("fixture has a second page");

        let second_spec = PageSpec::new(
            AccountScope::All,
            FolderScope::Inbox,
            None,
            PageBoundary::After(next_cursor),
            50,
        )
        .unwrap();
        let second_page =
            query_mailbox(&connection, &second_spec).expect("query second fixture page");
        assert_eq!(second_page.rows.len(), 1);
        assert!(second_page.next_cursor.is_none());

        let integrity: String = connection
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .expect("check fixture integrity");
        assert_eq!(integrity, "ok");
        let foreign_key_violation = connection
            .prepare("PRAGMA foreign_key_check")
            .expect("prepare foreign-key check")
            .exists([])
            .expect("run foreign-key check");
        assert!(!foreign_key_violation);
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
    fn v9_backfills_and_tracks_external_message_search() {
        let mut connection = memory_connection();
        enable_foreign_keys(&connection).expect("enable foreign keys");
        enable_recursive_triggers(&connection).expect("enable recursive triggers");
        migrate_with(&mut connection, &MIGRATIONS[..8], 8).expect("create v8 schema");
        insert_account(&connection, 1, "user@example.test");
        connection
            .execute(
                "INSERT INTO messages
                 (id, account_id, remote_key, sender_name, sender_address,
                  subject, preview, received_at_ms)
                 VALUES (100, 1, 'legacy', 'Ada Lovelace', 'ada@example.test',
                         'Legacy launch plan', 'Backfilled preview token', 100)",
                [],
            )
            .expect("seed a pre-FTS message");

        migrate(&mut connection).expect("upgrade v8 schema to v9");

        let matching_rows = |query: &str| {
            connection
                .query_row(
                    "SELECT count(*) FROM message_search
                     WHERE message_search MATCH ?1",
                    [query],
                    |row| row.get::<_, i64>(0),
                )
                .expect("query message search index")
        };
        assert_eq!(schema_version(&connection), 9);
        assert_eq!(matching_rows("\"legacy launch\""), 1);
        assert_eq!(matching_rows("\"backfilled preview\""), 1);

        connection
            .execute(
                "UPDATE messages
                 SET subject = 'Current release notes', preview = 'Updated index token'
                 WHERE id = 100",
                [],
            )
            .expect("update indexed message text");
        assert_eq!(matching_rows("\"legacy launch\""), 0);
        assert_eq!(matching_rows("\"current release\""), 1);

        connection
            .execute("DELETE FROM messages WHERE id = 100", [])
            .expect("delete indexed message");
        assert_eq!(matching_rows("\"current release\""), 0);
        connection
            .execute(
                "INSERT INTO message_search(message_search, rank)
                 VALUES ('integrity-check', 1)",
                [],
            )
            .expect("verify FTS index against external message content");
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

        migrate_with(&mut connection, &MIGRATIONS[..6], 6).expect("upgrade data to v6");

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
    fn v7_scopes_remote_identity_and_cascades_imap_locations() {
        let mut connection = memory_connection();
        migrate(&mut connection).expect("apply migrations");
        insert_account(&connection, 1, "one@example.test");
        insert_account(&connection, 2, "two@example.test");
        insert_folder(&connection, 10, 1, "inbox", "inbox");
        insert_folder(&connection, 20, 2, "inbox", "inbox");
        insert_message(&connection, 100, 1, "message-100");
        insert_message(&connection, 101, 1, "message-101");
        connection
            .execute(
                "INSERT INTO message_folders (message_id, folder_id, account_id)
                 VALUES (100, 10, 1), (101, 10, 1)",
                [],
            )
            .expect("seed IMAP memberships");

        connection
            .execute(
                "INSERT INTO sync_state
                     (folder_id, uid_validity, highest_modseq, mailbox_object_id)
                 VALUES (10, 7, 11, 'mailbox-object')",
                [],
            )
            .expect("store per-mailbox IMAP state");
        connection
            .execute(
                "INSERT INTO account_object_states
                     (account_id, object_kind, state_token, updated_at_ms)
                 VALUES (1, 'email', 'jmap-email-state', 0),
                        (1, 'mailbox', 'jmap-mailbox-state', 0)",
                [],
            )
            .expect("store account-scoped JMAP states");
        connection
            .execute(
                "INSERT INTO imap_message_locations
                     (message_id, folder_id, account_id, uid_validity, uid, modseq,
                      email_id, remote_seen, remote_flagged)
                 VALUES (100, 10, 1, 7, 42, 11, 'email-object', 0, 1)",
                [],
            )
            .expect("store mailbox-scoped IMAP locator");

        let duplicate = connection
            .execute(
                "INSERT INTO imap_message_locations
                     (message_id, folder_id, account_id, uid_validity, uid,
                      remote_seen, remote_flagged)
                 VALUES (101, 10, 1, 7, 42, 0, 0)",
                [],
            )
            .expect_err("one mailbox epoch and UID cannot identify two messages");
        assert_eq!(
            duplicate.sqlite_error_code(),
            Some(ErrorCode::ConstraintViolation)
        );
        let zero_epoch = connection
            .execute(
                "INSERT INTO imap_message_locations
                     (message_id, folder_id, account_id, uid_validity, uid,
                      remote_seen, remote_flagged)
                 VALUES (101, 10, 1, 0, 43, 0, 0)",
                [],
            )
            .expect_err("UIDVALIDITY must be non-zero");
        assert_eq!(
            zero_epoch.sqlite_error_code(),
            Some(ErrorCode::ConstraintViolation)
        );

        connection
            .execute(
                "DELETE FROM message_folders WHERE message_id = 100 AND folder_id = 10",
                [],
            )
            .expect("remove membership");
        let remaining: i64 = connection
            .query_row("SELECT count(*) FROM imap_message_locations", [], |row| {
                row.get(0)
            })
            .expect("count remaining IMAP locators");
        assert_eq!(remaining, 0);
    }

    #[test]
    fn v7_marks_legacy_remote_state_for_account_reconciliation() {
        let mut connection = memory_connection();
        enable_foreign_keys(&connection).expect("enable foreign keys");
        migrate_with(&mut connection, &MIGRATIONS[..6], 6).expect("create v6 schema");
        insert_account(&connection, 1, "one@example.test");
        insert_message(&connection, 100, 1, "changed-message");
        connection
            .execute("UPDATE messages SET revision = 3 WHERE id = 100", [])
            .expect("seed pre-journal local revision");
        connection
            .execute(
                "INSERT INTO message_tombstones (account_id, remote_key, deleted_at_ms)
                 VALUES (1, 'deleted-message', 123)",
                [],
            )
            .expect("seed legacy tombstone");

        migrate(&mut connection).expect("upgrade to v7");

        let reconciliation: String = connection
            .query_row(
                "SELECT reason
                 FROM remote_account_reconciliations
                 WHERE account_id = 1",
                [],
                |row| row.get(0),
            )
            .expect("read legacy account reconciliation marker");
        assert_eq!(reconciliation, "legacy_journal_bootstrap");
        let legacy_revision: i64 = connection
            .query_row(
                "SELECT legacy_reconcile_revision FROM messages WHERE id = 100",
                [],
                |row| row.get(0),
            )
            .expect("read target-level legacy reconciliation marker");
        assert_eq!(legacy_revision, 3);
        let advanced_marker = connection
            .execute(
                "UPDATE messages SET legacy_reconcile_revision = 4 WHERE id = 100",
                [],
            )
            .expect_err("legacy marker cannot exceed the current local revision");
        assert_eq!(
            advanced_marker.sqlite_error_code(),
            Some(ErrorCode::ConstraintViolation)
        );
        let feeder_plan: String = connection
            .query_row(
                "EXPLAIN QUERY PLAN
                 SELECT id FROM messages
                 WHERE account_id = 1 AND legacy_reconcile_revision IS NOT NULL
                 ORDER BY id
                 LIMIT 1",
                [],
                |row| row.get(3),
            )
            .expect("explain bounded legacy feeder query");
        assert!(
            feeder_plan.contains("idx_messages_legacy_reconcile_pending"),
            "legacy feeder must use its partial index: {feeder_plan}"
        );
        let intent_count: i64 = connection
            .query_row("SELECT count(*) FROM remote_change_intents", [], |row| {
                row.get(0)
            })
            .expect("count target intents after legacy migration");
        assert_eq!(intent_count, 0);
        assert_eq!(
            schema_version(&connection),
            i64::from(LATEST_SCHEMA_VERSION)
        );
    }

    #[test]
    fn v7_migrates_legacy_state_above_target_journal_caps() {
        let mut connection = memory_connection();
        enable_foreign_keys(&connection).expect("enable foreign keys");
        migrate_with(&mut connection, &MIGRATIONS[..6], 6).expect("create v6 schema");
        for account_id in 1..=5 {
            insert_account(
                &connection,
                account_id,
                &format!("account-{account_id}@example.test"),
            );
        }

        {
            let transaction = connection
                .transaction()
                .expect("start legacy overflow transaction");
            {
                let mut insert = transaction
                    .prepare(
                        "INSERT INTO messages
                             (id, account_id, remote_key, received_at_ms, revision)
                         VALUES (?1, ?2, ?3, 0, 1)",
                    )
                    .expect("prepare legacy message insert");
                for index in 0..16_385_i64 {
                    let account_id = index / 4_096 + 1;
                    insert
                        .execute(params![index + 1, account_id, format!("legacy-{index}")])
                        .expect("seed legacy state beyond global journal cap");
                }
            }
            transaction.commit().expect("commit legacy overflow data");
        }

        migrate(&mut connection).expect("upgrade oversized legacy state to the latest schema");

        let marker_count: i64 = connection
            .query_row(
                "SELECT count(*) FROM remote_account_reconciliations",
                [],
                |row| row.get(0),
            )
            .expect("count bounded account reconciliation markers");
        assert_eq!(marker_count, 5);
        let pending_count: i64 = connection
            .query_row(
                "SELECT count(*) FROM messages
                 WHERE legacy_reconcile_revision IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .expect("count durable target-level reconciliation markers");
        assert_eq!(pending_count, 16_385);
        let intent_count: i64 = connection
            .query_row("SELECT count(*) FROM remote_change_intents", [], |row| {
                row.get(0)
            })
            .expect("count target intents after oversized migration");
        assert_eq!(intent_count, 0);
        assert_eq!(
            schema_version(&connection),
            i64::from(LATEST_SCHEMA_VERSION)
        );
    }

    #[test]
    fn remote_journal_identity_and_lease_state_are_immutable() {
        let mut connection = memory_connection();
        migrate(&mut connection).expect("apply migrations");
        insert_account(&connection, 1, "one@example.test");
        insert_account(&connection, 2, "two@example.test");
        insert_folder(&connection, 10, 1, "folder-10", "label");
        insert_message(&connection, 100, 1, "message-100");
        connection
            .execute(
                "INSERT INTO message_folders (message_id, folder_id, account_id)
                 VALUES (100, 10, 1)",
                [],
            )
            .expect("seed message membership");
        connection
            .execute(
                "INSERT INTO remote_change_intents
                     (account_id, message_id, target_key, local_revision,
                      unread_base, unread_desired, placement_active,
                      not_before_ms, created_at_ms, updated_at_ms)
                 VALUES (1, 100, 'message-100', 0, 1, 0, 1, 0, 0, 0)",
                [],
            )
            .expect("insert identity-bound intent");
        let intent_id = connection.last_insert_rowid();
        connection
            .execute(
                "INSERT INTO remote_change_intent_folders (intent_id, side, folder_key)
                 VALUES (?1, 'base', 'folder-10')",
                [intent_id],
            )
            .expect("insert folder snapshot");
        connection
            .execute(
                "INSERT INTO remote_change_intent_imap_sources
                     (intent_id, folder_key, uid_validity, uid,
                      remote_seen, remote_flagged)
                 VALUES (?1, 'folder-10', 7, 42, 0, 0)",
                [intent_id],
            )
            .expect("insert frozen IMAP source");
        connection
            .execute(
                "INSERT INTO message_tombstones (account_id, remote_key, deleted_at_ms)
                 VALUES (1, 'deleted-message', 0)",
                [],
            )
            .expect("seed tombstone");
        connection
            .execute(
                "INSERT INTO message_tombstone_imap_locations
                     (account_id, target_key, folder_key, uid_validity, uid)
                 VALUES (1, 'deleted-message', 'folder-10', 7, 43)",
                [],
            )
            .expect("insert frozen tombstone locator");
        let child_count: i64 = connection
            .query_row(
                "SELECT child_count FROM remote_journal_usage WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .expect("read journal child usage");
        assert_eq!(child_count, 3);

        connection
            .execute(
                "INSERT OR REPLACE INTO remote_change_intent_imap_sources
                     (intent_id, folder_key, uid_validity, uid,
                      modseq, remote_seen, remote_flagged)
                 VALUES (?1, 'folder-10', 7, 42, 2, 0, 0)",
                [intent_id],
            )
            .expect("replace a frozen source without drifting its quota");
        let child_count_after_replace: i64 = connection
            .query_row(
                "SELECT child_count FROM remote_journal_usage WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .expect("read usage after source replacement");
        assert_eq!(child_count_after_replace, 3);

        connection
            .execute(
                "UPDATE remote_journal_usage SET child_count = 65536 WHERE singleton = 1",
                [],
            )
            .expect("simulate a full global child budget");
        let updated_count = connection
            .execute(
                "INSERT INTO remote_change_intent_imap_sources
                     (intent_id, folder_key, uid_validity, uid,
                      modseq, remote_seen, remote_flagged)
                 VALUES (?1, 'folder-10', 7, 42, 9, 1, 1)
                 ON CONFLICT(intent_id, folder_key, uid_validity, uid) DO UPDATE
                 SET modseq = excluded.modseq,
                     remote_seen = excluded.remote_seen,
                     remote_flagged = excluded.remote_flagged",
                [intent_id],
            )
            .expect("an existing source remains updatable at the global cap");
        assert_eq!(updated_count, 1);
        let updated_source: (i64, bool, bool) = connection
            .query_row(
                "SELECT modseq, remote_seen, remote_flagged
                 FROM remote_change_intent_imap_sources
                 WHERE intent_id = ?1",
                [intent_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("read updated source checkpoint");
        assert_eq!(updated_source, (9, true, true));
        let global_overflow = connection
            .execute(
                "INSERT INTO remote_change_intent_imap_sources
                     (intent_id, folder_key, uid_validity, uid,
                      remote_seen, remote_flagged)
                 VALUES (?1, 'folder-10', 7, 44, 0, 0)",
                [intent_id],
            )
            .expect_err("a new source cannot exceed the global child cap");
        assert_eq!(
            global_overflow.sqlite_error_code(),
            Some(ErrorCode::ConstraintViolation)
        );
        connection
            .execute(
                "UPDATE remote_journal_usage SET child_count = 3 WHERE singleton = 1",
                [],
            )
            .expect("restore the measured child usage");

        for (sql, message) in [
            (
                "UPDATE messages SET account_id = 2 WHERE id = 100",
                "message account cannot move",
            ),
            (
                "UPDATE messages SET remote_key = 'other-message' WHERE id = 100",
                "message target key cannot change",
            ),
            (
                "UPDATE accounts SET provider = 'jmap' WHERE id = 1",
                "account provider cannot change",
            ),
            (
                "UPDATE accounts SET remote_key = 'other-account' WHERE id = 1",
                "account remote key cannot change",
            ),
            (
                "UPDATE folders SET account_id = 2 WHERE id = 10",
                "folder account cannot move",
            ),
            (
                "UPDATE remote_change_intent_folders
                 SET folder_key = 'other-folder'
                 WHERE intent_id = 1 AND side = 'base' AND folder_key = 'folder-10'",
                "folder snapshot identity cannot change",
            ),
            (
                "UPDATE remote_change_intent_imap_sources SET uid = 99 WHERE intent_id = 1",
                "source locator identity cannot change",
            ),
            (
                "UPDATE message_tombstone_imap_locations SET uid = 99
                 WHERE account_id = 1 AND target_key = 'deleted-message'",
                "tombstone locator identity cannot change",
            ),
        ] {
            let error = connection.execute(sql, []).expect_err(message);
            assert_eq!(
                error.sqlite_error_code(),
                Some(ErrorCode::ConstraintViolation)
            );
        }

        let mismatched_target = connection
            .execute(
                "INSERT INTO remote_change_intents
                     (account_id, message_id, target_key, local_revision,
                      starred_base, starred_desired,
                      not_before_ms, created_at_ms, updated_at_ms)
                 VALUES (1, 100, 'other-target', 0, 0, 1, 0, 0, 0)",
                [],
            )
            .expect_err("intent target must equal the message remote key");
        assert_eq!(
            mismatched_target.sqlite_error_code(),
            Some(ErrorCode::ConstraintViolation)
        );
        let wrong_account = connection
            .execute(
                "INSERT INTO remote_change_intents
                     (account_id, message_id, target_key, local_revision,
                      starred_base, starred_desired,
                      not_before_ms, created_at_ms, updated_at_ms)
                 VALUES (2, 100, 'message-100', 0, 0, 1, 0, 0, 0)",
                [],
            )
            .expect_err("intent message must belong to its account");
        assert_eq!(
            wrong_account.sqlite_error_code(),
            Some(ErrorCode::ConstraintViolation)
        );

        connection
            .execute(
                "UPDATE remote_change_intents
                 SET intent_version = 2, claim_epoch = 1
                 WHERE id = ?1",
                [intent_id],
            )
            .expect("advance intent version and claim epoch");
        for sql in [
            "UPDATE remote_change_intents SET intent_version = 1 WHERE id = 1",
            "UPDATE remote_change_intents SET claim_epoch = 0 WHERE id = 1",
            "UPDATE remote_change_intents SET leased_version = 1 WHERE id = 1",
        ] {
            let error = connection
                .execute(sql, [])
                .expect_err("versions and lease pairs cannot regress");
            assert_eq!(
                error.sqlite_error_code(),
                Some(ErrorCode::ConstraintViolation)
            );
        }

        connection
            .execute(
                "DELETE FROM remote_change_intents WHERE id = ?1",
                [intent_id],
            )
            .expect("delete intent and its frozen children");
        let after_intent_delete: i64 = connection
            .query_row(
                "SELECT child_count FROM remote_journal_usage WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .expect("read usage after intent cascade");
        assert_eq!(after_intent_delete, 1);
        connection
            .execute(
                "DELETE FROM message_tombstones
                 WHERE account_id = 1 AND remote_key = 'deleted-message'",
                [],
            )
            .expect("delete tombstone and its frozen locator");
        let after_tombstone_delete: i64 = connection
            .query_row(
                "SELECT child_count FROM remote_journal_usage WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .expect("read usage after tombstone cascade");
        assert_eq!(after_tombstone_delete, 0);
    }

    #[test]
    fn remote_intent_constraints_and_account_cap_are_hard_bounds() {
        let mut connection = memory_connection();
        migrate(&mut connection).expect("apply migrations");
        insert_account(&connection, 1, "one@example.test");
        insert_account(&connection, 2, "two@example.test");
        insert_message(&connection, 100, 1, "message-100");

        connection
            .execute(
                "INSERT INTO remote_change_intents
                 (account_id, message_id, target_key, local_revision,
                      unread_base, unread_desired, not_before_ms, created_at_ms, updated_at_ms)
                 VALUES (1, 100, 'message-100', 0, 1, 0, 0, 0, 0)",
                [],
            )
            .expect("insert bounded desired-state intent");
        let intent_id = connection.last_insert_rowid();

        let unmatched_flag = connection
            .execute(
                "INSERT INTO remote_change_intents
                     (account_id, target_key, local_revision, unread_base,
                      not_before_ms, created_at_ms, updated_at_ms)
                 VALUES (1, 'invalid-pair', 0, 1, 0, 0, 0)",
                [],
            )
            .expect_err("base and desired flags must be paired");
        assert_eq!(
            unmatched_flag.sqlite_error_code(),
            Some(ErrorCode::ConstraintViolation)
        );
        let wrong_account = connection
            .execute(
                "INSERT INTO remote_change_intents
                     (account_id, message_id, target_key, local_revision,
                      starred_base, starred_desired, not_before_ms,
                      created_at_ms, updated_at_ms)
                 VALUES (2, 100, 'wrong-account', 0, 0, 1, 0, 0, 0)",
                [],
            )
            .expect_err("intent message must belong to its account");
        assert_eq!(
            wrong_account.sqlite_error_code(),
            Some(ErrorCode::ConstraintViolation)
        );
        let invalid_lease = connection
            .execute(
                "INSERT INTO remote_change_intents
                     (account_id, target_key, local_revision,
                      reconcile_requested, state, leased_version,
                      lease_expires_at_ms, not_before_ms, created_at_ms, updated_at_ms)
                 VALUES (1, 'invalid-lease', 0, 1, 'in_flight', 1, 1, 0, 0, 0)",
                [],
            )
            .expect_err("an in-flight lease needs a positive claim and attempt");
        assert_eq!(
            invalid_lease.sqlite_error_code(),
            Some(ErrorCode::ConstraintViolation)
        );

        connection
            .execute(
                "UPDATE remote_change_intents SET placement_active = 1 WHERE id = ?1",
                [intent_id],
            )
            .expect("activate placement snapshot");
        {
            let transaction = connection
                .transaction()
                .expect("start folder-cap transaction");
            {
                let mut insert = transaction
                    .prepare(
                        "INSERT INTO remote_change_intent_folders
                             (intent_id, side, folder_key)
                         VALUES (?1, 'base', ?2)",
                    )
                    .expect("prepare folder snapshot insert");
                for index in 0..256 {
                    insert
                        .execute(params![intent_id, format!("folder-{index}")])
                        .expect("insert folder snapshot within cap");
                }
            }
            transaction.commit().expect("commit folder snapshots");
        }
        connection
            .execute(
                "INSERT OR IGNORE INTO remote_change_intent_folders
                     (intent_id, side, folder_key)
                 VALUES (?1, 'base', 'folder-0')",
                [intent_id],
            )
            .expect("an existing folder remains idempotent at the cap");
        let child_count: i64 = connection
            .query_row(
                "SELECT child_count FROM remote_journal_usage WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .expect("read folder snapshot usage");
        assert_eq!(child_count, 256);
        let folder_overflow = connection
            .execute(
                "INSERT INTO remote_change_intent_folders
                     (intent_id, side, folder_key)
                 VALUES (?1, 'base', 'folder-overflow')",
                [intent_id],
            )
            .expect_err("the 257th base folder must be rejected");
        assert_eq!(
            folder_overflow.sqlite_error_code(),
            Some(ErrorCode::ConstraintViolation)
        );

        {
            let transaction = connection
                .transaction()
                .expect("start intent-cap transaction");
            {
                let mut insert = transaction
                    .prepare(
                        "INSERT INTO remote_change_intents
                             (account_id, target_key, local_revision,
                              starred_base, starred_desired,
                              not_before_ms, created_at_ms, updated_at_ms)
                         VALUES (1, ?1, 0, 0, 1, 0, 0, 0)",
                    )
                    .expect("prepare intent insert");
                for index in 2..=4096 {
                    insert
                        .execute([format!("target-{index}")])
                        .expect("insert intent within account cap");
                }
            }
            transaction.commit().expect("commit capped intents");
        }
        let account_overflow = connection
            .execute(
                "INSERT INTO remote_change_intents
                     (account_id, target_key, local_revision,
                      starred_base, starred_desired,
                      not_before_ms, created_at_ms, updated_at_ms)
                 VALUES (1, 'target-4097', 0, 0, 1, 0, 0, 0)",
                [],
            )
            .expect_err("the 4097th account intent must be rejected");
        assert_eq!(
            account_overflow.sqlite_error_code(),
            Some(ErrorCode::ConstraintViolation)
        );
        connection
            .execute(
                "INSERT INTO remote_change_intents
                     (account_id, target_key, local_revision,
                      unread_base, unread_desired,
                      not_before_ms, created_at_ms, updated_at_ms)
                 VALUES (1, 'message-100', 1, 1, 1, 0, 0, 1)
                 ON CONFLICT(account_id, target_key) DO UPDATE
                 SET unread_desired = excluded.unread_desired,
                     intent_version = remote_change_intents.intent_version + 1,
                     updated_at_ms = excluded.updated_at_ms",
                [],
            )
            .expect("merge an existing target while the account is at its cap");
        let version: i64 = connection
            .query_row(
                "SELECT intent_version FROM remote_change_intents WHERE id = ?1",
                [intent_id],
                |row| row.get(0),
            )
            .expect("read merged intent version");
        assert_eq!(version, 2);
    }

    #[test]
    fn remote_lease_reservations_share_and_release_the_child_budget() {
        let mut connection = memory_connection();
        migrate(&mut connection).expect("apply migrations");
        insert_account(&connection, 1, "one@example.test");
        let bypass = connection
            .execute(
                "INSERT INTO remote_change_intents
                     (account_id, target_key, local_revision,
                      unread_base, unread_desired, state, leased_version,
                      claim_epoch, lease_expires_at_ms, attempt_count,
                      leased_folder_reserve, not_before_ms,
                      created_at_ms, updated_at_ms)
                 VALUES (1, 'bypass', 0, 1, 0, 'in_flight', 1,
                         1, 1, 1, 1, 0, 0, 0)",
                [],
            )
            .expect_err("a reservation cannot bypass accounting through insert");
        assert_eq!(
            bypass.sqlite_error_code(),
            Some(ErrorCode::ConstraintViolation)
        );
        connection
            .execute(
                "INSERT INTO remote_change_intents
                     (account_id, target_key, local_revision, placement_active,
                      not_before_ms, created_at_ms, updated_at_ms)
                 VALUES (1, 'message-1', 0, 1, 0, 0, 0)",
                [],
            )
            .expect("insert placement intent");
        let intent_id = connection.last_insert_rowid();
        connection
            .execute(
                "INSERT INTO remote_change_intent_folders (intent_id, side, folder_key)
                 VALUES (?1, 'desired', 'archive')",
                [intent_id],
            )
            .expect("insert desired folder");
        connection
            .execute(
                "UPDATE remote_journal_usage SET child_count = 65534 WHERE singleton = 1",
                [],
            )
            .expect("move usage near the global limit");
        connection
            .execute(
                "UPDATE remote_change_intents
                 SET state = 'in_flight', leased_version = 1, claim_epoch = 1,
                     lease_expires_at_ms = 1, attempt_count = 1,
                     leased_folder_reserve = 2
                 WHERE id = ?1",
                [intent_id],
            )
            .expect("reserve the remaining child capacity");
        let reserved: i64 = connection
            .query_row(
                "SELECT reserved_count FROM remote_journal_usage WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .expect("read reserved usage");
        assert_eq!(reserved, 2);

        let full = connection
            .execute(
                "INSERT INTO remote_change_intent_folders (intent_id, side, folder_key)
                 VALUES (?1, 'desired', 'trash')",
                [intent_id],
            )
            .expect_err("reserved capacity must exclude unrelated child inserts");
        assert_eq!(
            full.sqlite_error_code(),
            Some(ErrorCode::ConstraintViolation)
        );
        let invalid_release = connection
            .execute(
                "UPDATE remote_change_intents SET state = 'ready' WHERE id = ?1",
                [intent_id],
            )
            .expect_err("a non-zero reserve requires an active lease");
        assert_eq!(
            invalid_release.sqlite_error_code(),
            Some(ErrorCode::ConstraintViolation)
        );

        connection
            .execute(
                "UPDATE remote_change_intents
                 SET state = 'ready', leased_version = NULL,
                     lease_expires_at_ms = NULL, leased_folder_reserve = 0
                 WHERE id = ?1",
                [intent_id],
            )
            .expect("release the lease and its reservation atomically");
        connection
            .execute(
                "INSERT INTO remote_change_intent_folders (intent_id, side, folder_key)
                 VALUES (?1, 'desired', 'trash')",
                [intent_id],
            )
            .expect("released capacity is available to child rows");
        connection
            .execute(
                "UPDATE remote_change_intents
                 SET state = 'in_flight', leased_version = 1,
                     lease_expires_at_ms = 2, leased_folder_reserve = 1
                 WHERE id = ?1",
                [intent_id],
            )
            .expect("reserve capacity again");
        connection
            .execute(
                "DELETE FROM remote_change_intents WHERE id = ?1",
                [intent_id],
            )
            .expect("delete the leased intent");
        let reserved_after_delete: i64 = connection
            .query_row(
                "SELECT reserved_count FROM remote_journal_usage WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .expect("read usage after delete");
        assert_eq!(reserved_after_delete, 0);
    }

    #[test]
    fn v8_recovers_unreserved_v7_leases_before_report_processing() {
        let mut connection = memory_connection();
        enable_foreign_keys(&connection).expect("enable foreign keys");
        migrate_with(&mut connection, &MIGRATIONS[..7], 7).expect("create v7 schema");
        insert_account(&connection, 1, "one@example.test");
        connection
            .execute(
                "INSERT INTO remote_change_intents
                     (account_id, target_key, local_revision, placement_active,
                      dispatched_mask, state, leased_version, claim_epoch,
                      lease_expires_at_ms, attempt_count, not_before_ms,
                      created_at_ms, updated_at_ms)
                 VALUES (1, 'message-1', 0, 1, 4, 'in_flight', 1, 1,
                         253402300799999, 1, 0, 0, 0)",
                [],
            )
            .expect("insert a v7 lease without a reservation");
        let intent_id = connection.last_insert_rowid();
        connection
            .execute(
                "INSERT INTO remote_change_intent_folders (intent_id, side, folder_key)
                 VALUES (?1, 'base', 'inbox'), (?1, 'desired', 'archive')",
                [intent_id],
            )
            .expect("insert v7 placement snapshots");

        migrate(&mut connection).expect("upgrade the active v7 lease to v8");

        let recovered: (String, Option<i64>, Option<i64>, bool, i64, String) = connection
            .query_row(
                "SELECT state, leased_version, lease_expires_at_ms,
                        reconcile_requested, leased_folder_reserve, error_code
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
            .expect("read the recovered v8 intent");
        assert_eq!(
            recovered,
            (
                "reconcile".into(),
                None,
                None,
                true,
                0,
                "upgrade_lease_recovery".into(),
            )
        );
        let reserved: i64 = connection
            .query_row(
                "SELECT reserved_count FROM remote_journal_usage WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .expect("read v8 reservation usage");
        assert_eq!(reserved, 0);
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
                found,
                supported: LATEST_SCHEMA_VERSION,
            } if found == LATEST_SCHEMA_VERSION + 1
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
